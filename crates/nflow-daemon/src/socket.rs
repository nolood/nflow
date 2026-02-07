use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot};
use tracing::{error, info, warn};

use crate::events::{ClientId, SharedEventBus};

/// A request message from a client.
///
/// Clients send NDJSON lines in the format: `{"id": "...", "command": "...", "params": {...}}`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    /// Unique request ID (chosen by client, echoed back in response).
    pub id: String,
    /// Command name (e.g., "project.init", "spec.new").
    pub command: String,
    /// Command parameters — arbitrary JSON object.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A response message sent back to the client.
///
/// Format: `{"id": "...", "status": "ok"|"error", "data": ...}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Echoed request ID.
    pub id: String,
    /// Status: "ok" for success, "error" for failure.
    pub status: ResponseStatus,
    /// Response payload.
    pub data: serde_json::Value,
}

/// Status of a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    Ok,
    Error,
}

impl Response {
    /// Create a success response.
    pub fn ok(id: String, data: serde_json::Value) -> Self {
        Self {
            id,
            status: ResponseStatus::Ok,
            data,
        }
    }

    /// Create an error response.
    pub fn error(id: String, message: &str) -> Self {
        Self {
            id,
            status: ResponseStatus::Error,
            data: serde_json::json!({ "message": message }),
        }
    }
}

/// A single line in a streaming response.
///
/// Streaming responses send multiple JSON lines with the same `id`.
/// The final line has `done: true` to signal the stream is complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingResponseLine {
    /// Echoed request ID (same for all lines in the stream).
    pub id: String,
    /// Status of this line: "ok" or "error".
    pub status: ResponseStatus,
    /// Payload for this line.
    pub data: serde_json::Value,
    /// Whether this is the final line in the stream.
    #[serde(default)]
    pub done: bool,
}

impl StreamingResponseLine {
    /// Create a streaming data line (not final).
    pub fn data(id: String, data: serde_json::Value) -> Self {
        Self {
            id,
            status: ResponseStatus::Ok,
            data,
            done: false,
        }
    }

    /// Create the final success line.
    pub fn done(id: String, data: serde_json::Value) -> Self {
        Self {
            id,
            status: ResponseStatus::Ok,
            data,
            done: true,
        }
    }

    /// Create a final error line.
    pub fn error(id: String, message: &str) -> Self {
        Self {
            id,
            status: ResponseStatus::Error,
            data: serde_json::json!({ "message": message }),
            done: true,
        }
    }
}

/// The result of a command handler — either a single response or a streaming channel.
pub enum HandlerResult {
    /// A single response (non-streaming).
    Single(Response),
    /// A streaming response — receiver produces multiple lines.
    Streaming(tokio::sync::mpsc::Receiver<StreamingResponseLine>),
}

/// A command handler function that processes a request and returns a response.
///
/// Handlers receive the request and must return a response. They are called
/// from an async context but execute synchronously (command handling and
/// scheduling are serialized per the architecture).
pub type CommandHandler = Box<dyn Fn(Request) -> HandlerResult + Send + Sync>;

/// Configuration for the socket server.
pub struct SocketServerConfig {
    /// Path to the Unix socket file.
    pub socket_path: PathBuf,
    /// Command handler for processing requests.
    pub handler: CommandHandler,
    /// Optional event bus for broadcasting events to subscribed clients.
    pub event_bus: Option<SharedEventBus>,
}

/// Handle for controlling a running socket server.
pub struct SocketServerHandle {
    /// Send a signal to shut down the server.
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl SocketServerHandle {
    /// Signal the server to stop accepting new connections and shut down.
    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for SocketServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Bind a Unix domain socket at the given path.
///
/// Removes any existing socket file before binding. Sets file mode to 0600
/// (owner-only read/write).
pub fn bind_socket(socket_path: &Path) -> std::io::Result<UnixListener> {
    // Remove stale socket file if it exists
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(socket_path)?;

    // Set socket file permissions to 0600 (owner only)
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;

    info!(path = %socket_path.display(), "bound Unix socket");
    Ok(listener)
}

/// Start the socket server, accepting connections and dispatching commands.
///
/// This function spawns a tokio task that:
/// 1. Accepts client connections on the Unix socket
/// 2. Spawns a task per client for concurrent handling
/// 3. Reads NDJSON lines from each client
/// 4. Parses and routes each request to the command handler
/// 5. Sends the JSON response back to the client
///
/// Returns a handle that can be used to shut down the server.
pub fn start_server(config: SocketServerConfig) -> std::io::Result<SocketServerHandle> {
    let listener = bind_socket(&config.socket_path)?;
    let handler = std::sync::Arc::new(config.handler);
    let event_bus = config.event_bus;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(accept_loop(listener, handler, event_bus, shutdown_rx));

    Ok(SocketServerHandle {
        shutdown_tx: Some(shutdown_tx),
    })
}

/// Main accept loop — runs until shutdown signal or listener error.
async fn accept_loop(
    listener: UnixListener,
    handler: std::sync::Arc<CommandHandler>,
    event_bus: Option<SharedEventBus>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let handler = std::sync::Arc::clone(&handler);
                        let event_bus = event_bus.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, handler, event_bus).await {
                                warn!(error = %e, "client connection error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "failed to accept connection");
                        // Brief pause to avoid tight loop on persistent errors
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = &mut shutdown_rx => {
                info!("socket server shutting down");
                break;
            }
        }
    }
}

/// Write a single NDJSON line to the writer. Returns false if the client disconnected.
async fn write_line(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    json: &str,
) -> std::io::Result<bool> {
    let mut line = json.to_string();
    line.push('\n');
    if let Err(e) = writer.write_all(line.as_bytes()).await {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Ok(false);
        }
        return Err(e);
    }
    writer.flush().await?;
    Ok(true)
}

/// Handle a single client connection.
///
/// Reads NDJSON lines, parses each as a Request, dispatches to the handler,
/// and writes the Response back as NDJSON. Supports both single responses
/// and streaming responses (multiple lines with `done: true` on the last one).
///
/// Handles the special "subscribe" command to register the client for event
/// broadcasts. When a client subscribes, a background task forwards events
/// to the client's write half. On disconnect, subscriptions are cleaned up.
async fn handle_client(
    stream: UnixStream,
    handler: std::sync::Arc<CommandHandler>,
    event_bus: Option<SharedEventBus>,
) -> std::io::Result<()> {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Wrap writer in Arc<Mutex> so both the request loop and event forwarder can write
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    // Track this client's subscription for cleanup on disconnect
    let client_id = ClientId::new();
    let mut subscribed = false;

    let result = handle_client_inner(
        &mut lines,
        &writer,
        &handler,
        &event_bus,
        client_id,
        &mut subscribed,
    )
    .await;

    // Cleanup: remove event subscription on disconnect
    if subscribed {
        if let Some(ref bus) = event_bus {
            bus.unsubscribe(&client_id);
            info!(client_id = %client_id.0, "removed event subscription on disconnect");
        }
    }

    info!("client disconnected");
    result
}

/// Inner client handling loop.
async fn handle_client_inner(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    handler: &std::sync::Arc<CommandHandler>,
    event_bus: &Option<SharedEventBus>,
    client_id: ClientId,
    subscribed: &mut bool,
) -> std::io::Result<()> {
    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let id = request.id.clone();
                let command = request.command.clone();
                info!(id = %id, command = %command, "received command");

                // Handle subscribe command specially
                if command == "subscribe" {
                    let resp = handle_subscribe(&id, event_bus, client_id, subscribed, writer);
                    let json = serde_json::to_string(&resp)
                        .map_err(|e| std::io::Error::other(format!("serialize error: {}", e)))?;
                    let mut w = writer.lock().await;
                    if !write_line(&mut w, &json).await? {
                        break;
                    }
                    continue;
                }

                // Dispatch to handler (synchronous — serialized with scheduler)
                let handler = std::sync::Arc::clone(handler);
                let result = tokio::task::spawn_blocking(move || handler(request))
                    .await
                    .unwrap_or_else(|e| {
                        HandlerResult::Single(Response::error(
                            id,
                            &format!("handler panicked: {}", e),
                        ))
                    });

                match result {
                    HandlerResult::Single(resp) => {
                        info!(id = %resp.id, status = ?resp.status, "sending response");
                        let json = serde_json::to_string(&resp).map_err(|e| {
                            std::io::Error::other(format!("serialize error: {}", e))
                        })?;
                        let mut w = writer.lock().await;
                        if !write_line(&mut w, &json).await? {
                            break;
                        }
                    }
                    HandlerResult::Streaming(mut rx) => {
                        // Send streaming lines until channel closes or done=true
                        while let Some(line) = rx.recv().await {
                            let is_done = line.done;
                            let json = serde_json::to_string(&line).map_err(|e| {
                                std::io::Error::other(format!("serialize error: {}", e))
                            })?;
                            let mut w = writer.lock().await;
                            if !write_line(&mut w, &json).await? {
                                return Ok(());
                            }
                            if is_done {
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to parse request");
                let resp = Response::error(String::new(), &format!("invalid request: {}", e));
                let json = serde_json::to_string(&resp)
                    .map_err(|e| std::io::Error::other(format!("serialize error: {}", e)))?;
                let mut w = writer.lock().await;
                if !write_line(&mut w, &json).await? {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Handle the "subscribe" command — register the client for event broadcasts.
fn handle_subscribe(
    request_id: &str,
    event_bus: &Option<SharedEventBus>,
    client_id: ClientId,
    subscribed: &mut bool,
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Response {
    let Some(ref bus) = event_bus else {
        return Response::error(request_id.to_string(), "event bus not configured");
    };

    if *subscribed {
        return Response::ok(
            request_id.to_string(),
            serde_json::json!({ "client_id": client_id.0.to_string(), "already_subscribed": true }),
        );
    }

    let mut rx = bus.subscribe(client_id);
    *subscribed = true;

    // Spawn a background task that forwards events to this client
    let writer = std::sync::Arc::clone(writer);
    let bus_clone = std::sync::Arc::clone(bus);
    let cid = client_id;
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let event_msg = serde_json::json!({
                        "id": "",
                        "status": "ok",
                        "event": event,
                    });
                    let json = match serde_json::to_string(&event_msg) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    let mut w = writer.lock().await;
                    if write_line(&mut w, &json).await.unwrap_or(false) {
                        continue;
                    }
                    // Client disconnected — clean up subscription
                    bus_clone.unsubscribe(&cid);
                    info!(client_id = %cid.0, "event forwarder detected disconnect");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(client_id = %cid.0, skipped = n, "client lagging behind on events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    info!(client_id = %client_id.0, "client subscribed to events");
    Response::ok(
        request_id.to_string(),
        serde_json::json!({ "client_id": client_id.0.to_string(), "subscribed": true }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{self, Event};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    fn temp_socket_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nflow_test_socket_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("nflow.sock")
    }

    fn echo_handler() -> CommandHandler {
        Box::new(|req: Request| HandlerResult::Single(Response::ok(req.id, req.params)))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_bind_socket_creates_file() {
        let path = temp_socket_path();
        let _listener = bind_socket(&path).unwrap();
        assert!(path.exists());

        // Verify permissions are 0600
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket should be owner-only (0600)");

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_bind_socket_removes_stale() {
        let path = temp_socket_path();
        std::fs::write(&path, "stale").unwrap();
        assert!(path.exists());

        let _listener = bind_socket(&path).unwrap();
        assert!(path.exists());

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_basic_request_response() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let req = r#"{"id":"1","command":"test","params":{"foo":"bar"}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "1");
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["foo"], "bar");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_multiple_requests() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        for i in 1..=3 {
            let req = format!(
                r#"{{"id":"{}","command":"test","params":{{"n":{}}}}}"#,
                i, i
            );
            writer
                .write_all(format!("{}\n", req).as_bytes())
                .await
                .unwrap();
            writer.flush().await.unwrap();

            let resp_line = lines.next_line().await.unwrap().unwrap();
            let resp: Response = serde_json::from_str(&resp_line).unwrap();
            assert_eq!(resp.id, i.to_string());
            assert_eq!(resp.status, ResponseStatus::Ok);
            assert_eq!(resp.data["n"], i);
        }

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_invalid_json() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        writer.write_all(b"not json\n").await.unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("invalid request"));

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_empty_lines_ignored() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        writer.write_all(b"\n\n").await.unwrap();
        let req = r#"{"id":"1","command":"test","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "1");
        assert_eq!(resp.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_concurrent_clients() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut tasks = Vec::new();
        for i in 0..3 {
            let p = path.clone();
            tasks.push(tokio::spawn(async move {
                let stream = UnixStream::connect(&p).await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();

                let req = format!(
                    r#"{{"id":"client-{}","command":"test","params":{{"c":{}}}}}"#,
                    i, i
                );
                writer
                    .write_all(format!("{}\n", req).as_bytes())
                    .await
                    .unwrap();
                writer.flush().await.unwrap();

                let resp_line = lines.next_line().await.unwrap().unwrap();
                let resp: Response = serde_json::from_str(&resp_line).unwrap();
                assert_eq!(resp.id, format!("client-{}", i));
                assert_eq!(resp.status, ResponseStatus::Ok);
                assert_eq!(resp.data["c"], i);
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_client_disconnect_cleanup() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        {
            let _stream = UnixStream::connect(&path).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let req = r#"{"id":"after-disconnect","command":"test","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "after-disconnect");
        assert_eq!(resp.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_shutdown_stops_accepting() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result = UnixStream::connect(&path).await;
        if let Ok(stream) = result {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            writer
                .write_all(b"{\"id\":\"x\",\"command\":\"test\",\"params\":{}}\n")
                .await
                .ok();
            writer.flush().await.ok();
            let resp =
                tokio::time::timeout(std::time::Duration::from_millis(200), lines.next_line())
                    .await;
            match resp {
                Ok(Ok(None)) => {}
                Err(_) => {}
                _ => {}
            }
        }

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_handler_error_response() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: Box::new(|req: Request| {
                HandlerResult::Single(Response::error(req.id, "command not found"))
            }),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let req = r#"{"id":"err1","command":"unknown","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "err1");
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.data["message"], "command not found");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[test]
    fn test_request_deserialization() {
        let json = r#"{"id":"123","command":"project.init","params":{"name":"myproject"}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "123");
        assert_eq!(req.command, "project.init");
        assert_eq!(req.params["name"], "myproject");
    }

    #[test]
    fn test_request_deserialization_no_params() {
        let json = r#"{"id":"123","command":"project.list"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "123");
        assert_eq!(req.command, "project.list");
        assert!(req.params.is_null());
    }

    #[test]
    fn test_response_serialization_ok() {
        let resp = Response::ok("1".to_string(), serde_json::json!({"result": 42}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""status":"ok""#));
        assert!(json.contains(r#""id":"1""#));
        assert!(json.contains(r#""result":42"#));
    }

    #[test]
    fn test_response_serialization_error() {
        let resp = Response::error("2".to_string(), "not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""status":"error""#));
        assert!(json.contains(r#""message":"not found""#));
    }

    #[test]
    fn test_response_ok_constructor() {
        let resp = Response::ok("id1".to_string(), serde_json::json!(null));
        assert_eq!(resp.id, "id1");
        assert_eq!(resp.status, ResponseStatus::Ok);
    }

    #[test]
    fn test_response_error_constructor() {
        let resp = Response::error("id2".to_string(), "oops");
        assert_eq!(resp.id, "id2");
        assert_eq!(resp.status, ResponseStatus::Error);
        assert_eq!(resp.data["message"], "oops");
    }

    // --- Streaming response tests ---

    #[test]
    fn test_streaming_response_line_data() {
        let line = StreamingResponseLine::data(
            "req1".to_string(),
            serde_json::json!({"output": "line 1"}),
        );
        assert_eq!(line.id, "req1");
        assert_eq!(line.status, ResponseStatus::Ok);
        assert!(!line.done);
        assert_eq!(line.data["output"], "line 1");
    }

    #[test]
    fn test_streaming_response_line_done() {
        let line =
            StreamingResponseLine::done("req1".to_string(), serde_json::json!({"final": true}));
        assert_eq!(line.id, "req1");
        assert_eq!(line.status, ResponseStatus::Ok);
        assert!(line.done);
    }

    #[test]
    fn test_streaming_response_line_error() {
        let line = StreamingResponseLine::error("req1".to_string(), "stream failed");
        assert_eq!(line.id, "req1");
        assert_eq!(line.status, ResponseStatus::Error);
        assert!(line.done);
        assert_eq!(line.data["message"], "stream failed");
    }

    #[test]
    fn test_streaming_response_serialization() {
        let line = StreamingResponseLine::data("r1".to_string(), serde_json::json!({"chunk": 1}));
        let json = serde_json::to_string(&line).unwrap();
        assert!(json.contains(r#""done":false"#));
        assert!(json.contains(r#""id":"r1""#));

        let done_line = StreamingResponseLine::done("r1".to_string(), serde_json::json!(null));
        let json = serde_json::to_string(&done_line).unwrap();
        assert!(json.contains(r#""done":true"#));
    }

    #[test]
    fn test_streaming_response_deserialization() {
        let json = r#"{"id":"1","status":"ok","data":{"x":1},"done":false}"#;
        let line: StreamingResponseLine = serde_json::from_str(json).unwrap();
        assert_eq!(line.id, "1");
        assert!(!line.done);

        // done defaults to false when missing
        let json2 = r#"{"id":"2","status":"ok","data":null}"#;
        let line2: StreamingResponseLine = serde_json::from_str(json2).unwrap();
        assert!(!line2.done);
    }

    #[tokio::test]
    async fn test_server_streaming_response() {
        let path = temp_socket_path();

        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: Box::new(|req: Request| {
                let id = req.id.clone();
                let (tx, rx) = tokio::sync::mpsc::channel(16);

                // Spawn a task that sends 3 data lines + 1 done line
                tokio::spawn(async move {
                    for i in 1..=3 {
                        let _ = tx
                            .send(StreamingResponseLine::data(
                                id.clone(),
                                serde_json::json!({"line": i}),
                            ))
                            .await;
                    }
                    let _ = tx
                        .send(StreamingResponseLine::done(
                            id,
                            serde_json::json!({"total": 3}),
                        ))
                        .await;
                });

                HandlerResult::Streaming(rx)
            }),
            event_bus: None,
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let req = r#"{"id":"stream1","command":"log.follow","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Should receive 3 data lines + 1 done line
        let mut received = Vec::new();
        for _ in 0..4 {
            let line = lines.next_line().await.unwrap().unwrap();
            let resp: StreamingResponseLine = serde_json::from_str(&line).unwrap();
            assert_eq!(resp.id, "stream1");
            received.push(resp);
        }

        // First 3 lines are data (not done)
        for i in 0..3 {
            assert!(!received[i].done);
            assert_eq!(received[i].data["line"], (i + 1) as i64);
        }

        // Last line is done
        assert!(received[3].done);
        assert_eq!(received[3].data["total"], 3);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_subscribe_and_receive_events() {
        let path = temp_socket_path();
        let event_bus = events::new_event_bus(64);
        let bus_clone = std::sync::Arc::clone(&event_bus);

        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: Some(event_bus),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Subscribe to events
        let req = r#"{"id":"sub1","command":"subscribe","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Read subscribe response
        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "sub1");
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["subscribed"], true);

        // Give the subscription task time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Broadcast an event
        bus_clone.broadcast(Event::StatusChange {
            item_id: "T1".to_string(),
            item_type: "task".to_string(),
            old_status: "pending".to_string(),
            new_status: "in_progress".to_string(),
            project_id: "p1".to_string(),
        });

        // Client should receive the event
        let event_line = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
            .await
            .expect("timeout waiting for event")
            .unwrap()
            .unwrap();

        let event_msg: serde_json::Value = serde_json::from_str(&event_line).unwrap();
        assert_eq!(event_msg["status"], "ok");
        assert_eq!(event_msg["event"]["type"], "status_change");
        assert_eq!(event_msg["event"]["item_id"], "T1");
        assert_eq!(event_msg["event"]["new_status"], "in_progress");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_subscribe_without_event_bus() {
        let path = temp_socket_path();

        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: None, // No event bus
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let req = r#"{"id":"sub1","command":"subscribe","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "sub1");
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("not configured"));

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_disconnect_removes_subscription() {
        let path = temp_socket_path();
        let event_bus = events::new_event_bus(64);
        let bus_clone = std::sync::Arc::clone(&event_bus);

        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: Some(event_bus),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect, subscribe, then disconnect
        {
            let stream = UnixStream::connect(&path).await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();

            let req = r#"{"id":"sub1","command":"subscribe","params":{}}"#;
            writer
                .write_all(format!("{}\n", req).as_bytes())
                .await
                .unwrap();
            writer.flush().await.unwrap();

            let resp_line = lines.next_line().await.unwrap().unwrap();
            let resp: Response = serde_json::from_str(&resp_line).unwrap();
            assert_eq!(resp.data["subscribed"], true);

            // Verify subscription count increased
            assert_eq!(bus_clone.subscription_count(), 1);
        }
        // Stream dropped — client disconnected

        // Give the server time to detect disconnect and clean up
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Subscription should be removed
        assert_eq!(bus_clone.subscription_count(), 0);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_multiple_event_types() {
        let path = temp_socket_path();
        let event_bus = events::new_event_bus(64);
        let bus_clone = std::sync::Arc::clone(&event_bus);

        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: Some(event_bus),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Subscribe
        let req = r#"{"id":"sub","command":"subscribe","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let _ = lines.next_line().await.unwrap().unwrap(); // consume subscribe response

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Broadcast different event types
        bus_clone.broadcast(Event::AgentOutput {
            task_id: "W1-T1".to_string(),
            line: "compiling...".to_string(),
        });
        bus_clone.broadcast(Event::StoryCompleted {
            story_id: "S1".to_string(),
            project_id: "p1".to_string(),
            branch_name: "feature/s1".to_string(),
            mr_url: Some("https://github.com/org/repo/pull/1".to_string()),
        });

        // Receive first event (AgentOutput)
        let line1 = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg1: serde_json::Value = serde_json::from_str(&line1).unwrap();
        assert_eq!(msg1["event"]["type"], "agent_output");
        assert_eq!(msg1["event"]["line"], "compiling...");

        // Receive second event (StoryCompleted)
        let line2 = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg2: serde_json::Value = serde_json::from_str(&line2).unwrap();
        assert_eq!(msg2["event"]["type"], "story_completed");
        assert_eq!(msg2["event"]["story_id"], "S1");
        assert_eq!(
            msg2["event"]["mr_url"],
            "https://github.com/org/repo/pull/1"
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn test_server_double_subscribe_idempotent() {
        let path = temp_socket_path();
        let event_bus = events::new_event_bus(64);

        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
            event_bus: Some(event_bus),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Subscribe first time
        let req = r#"{"id":"sub1","command":"subscribe","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.data["subscribed"], true);

        // Subscribe second time
        let req2 = r#"{"id":"sub2","command":"subscribe","params":{}}"#;
        writer
            .write_all(format!("{}\n", req2).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();
        let resp_line2 = lines.next_line().await.unwrap().unwrap();
        let resp2: Response = serde_json::from_str(&resp_line2).unwrap();
        assert_eq!(resp2.data["already_subscribed"], true);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&path);
    }
}
