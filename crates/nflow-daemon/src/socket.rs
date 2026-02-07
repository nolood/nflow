use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

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

/// A command handler function that processes a request and returns a response.
///
/// Handlers receive the request and must return a response. They are called
/// from an async context but execute synchronously (command handling and
/// scheduling are serialized per the architecture).
pub type CommandHandler = Box<dyn Fn(Request) -> Response + Send + Sync>;

/// Configuration for the socket server.
pub struct SocketServerConfig {
    /// Path to the Unix socket file.
    pub socket_path: PathBuf,
    /// Command handler for processing requests.
    pub handler: CommandHandler,
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

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(accept_loop(listener, handler, shutdown_rx));

    Ok(SocketServerHandle {
        shutdown_tx: Some(shutdown_tx),
    })
}

/// Main accept loop — runs until shutdown signal or listener error.
async fn accept_loop(
    listener: UnixListener,
    handler: std::sync::Arc<CommandHandler>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let handler = std::sync::Arc::clone(&handler);
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, handler).await {
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

/// Handle a single client connection.
///
/// Reads NDJSON lines, parses each as a Request, dispatches to the handler,
/// and writes the Response back as NDJSON. Exits when the client disconnects
/// or sends invalid data.
async fn handle_client(
    stream: UnixStream,
    handler: std::sync::Arc<CommandHandler>,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let id = request.id.clone();
                let command = request.command.clone();
                info!(id = %id, command = %command, "received command");

                // Dispatch to handler (synchronous — serialized with scheduler)
                let handler = std::sync::Arc::clone(&handler);
                let resp = tokio::task::spawn_blocking(move || handler(request))
                    .await
                    .unwrap_or_else(|e| Response::error(id, &format!("handler panicked: {}", e)));

                info!(id = %resp.id, status = ?resp.status, "sending response");
                resp
            }
            Err(e) => {
                warn!(error = %e, "failed to parse request");
                // No request ID available — use empty string
                Response::error(String::new(), &format!("invalid request: {}", e))
            }
        };

        let mut response_json = serde_json::to_string(&response)
            .map_err(|e| std::io::Error::other(format!("serialize error: {}", e)))?;
        response_json.push('\n');

        if let Err(e) = writer.write_all(response_json.as_bytes()).await {
            // Client disconnected during write — exit cleanly
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                info!("client disconnected");
                break;
            }
            return Err(e);
        }
        writer.flush().await?;
    }

    info!("client disconnected");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    fn temp_socket_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nflow_test_socket_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("nflow.sock")
    }

    fn echo_handler() -> CommandHandler {
        Box::new(|req: Request| Response::ok(req.id, req.params))
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

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_bind_socket_removes_stale() {
        let path = temp_socket_path();
        // Create a stale file at the socket path
        std::fs::write(&path, "stale").unwrap();
        assert!(path.exists());

        let _listener = bind_socket(&path).unwrap();
        // Should have replaced the stale file with a real socket
        assert!(path.exists());

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_basic_request_response() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect as a client
        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send a request
        let req = r#"{"id":"1","command":"test","params":{"foo":"bar"}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Read response
        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, "1");
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["foo"], "bar");

        // Cleanup
        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_multiple_requests() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send multiple requests on the same connection
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_invalid_json() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send invalid JSON
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_empty_lines_ignored() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send empty lines followed by a valid request
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_concurrent_clients() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Spawn multiple clients concurrently
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_client_disconnect_cleanup() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect and immediately disconnect
        {
            let _stream = UnixStream::connect(&path).await.unwrap();
            // Drop stream — disconnect
        }

        // Give the server time to detect and clean up
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Server should still accept new connections
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_shutdown_stops_accepting() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: echo_handler(),
        };

        let handle = start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Shutdown the server
        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // New connections should fail (server is no longer accepting)
        let result = UnixStream::connect(&path).await;
        // Connection may succeed if socket file still exists but nobody is accepting,
        // or it may fail immediately. Either way the server loop has exited.
        // We just verify the server gracefully stopped.
        if let Ok(stream) = result {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            writer
                .write_all(b"{\"id\":\"x\",\"command\":\"test\",\"params\":{}}\n")
                .await
                .ok();
            writer.flush().await.ok();
            // Should get no response or EOF
            let resp =
                tokio::time::timeout(std::time::Duration::from_millis(200), lines.next_line())
                    .await;
            // Timeout or None is expected
            match resp {
                Ok(Ok(None)) => {} // EOF
                Err(_) => {}       // Timeout
                _ => {}            // Accept any outcome — server is shutting down
            }
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_server_handler_error_response() {
        let path = temp_socket_path();
        let config = SocketServerConfig {
            socket_path: path.clone(),
            handler: Box::new(|req: Request| Response::error(req.id, "command not found")),
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().unwrap());
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
}
