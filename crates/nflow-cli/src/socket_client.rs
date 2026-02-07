use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::daemon_client::socket_path;
use crate::error::{CliError, Result};

/// Current protocol version for the NDJSON handshake.
const PROTOCOL_VERSION: u32 = 1;

/// Default connection timeout (5 seconds).
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Default command timeout (60 seconds).
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Extended timeout for long-running commands like spec and plan (300 seconds).
const LONG_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

// --- Protocol types (duplicated from daemon for independence) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Handshake {
    protocol_version: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HandshakeResponse {
    protocol_version: u32,
    status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Request {
    id: String,
    command: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub status: ResponseStatus,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingResponseLine {
    pub id: String,
    pub status: ResponseStatus,
    pub data: serde_json::Value,
    #[serde(default)]
    pub done: bool,
}

/// A connected socket client to the nflow daemon.
pub struct SocketClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl std::fmt::Debug for SocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketClient").finish_non_exhaustive()
    }
}

impl SocketClient {
    /// Connect to the daemon socket and perform the protocol handshake.
    ///
    /// Returns a ready-to-use client, or an error if:
    /// - The socket doesn't exist
    /// - Connection times out (5s)
    /// - Handshake fails (version mismatch, etc.)
    pub async fn connect() -> Result<Self> {
        let sock_path = socket_path()?;
        Self::connect_to(&sock_path).await
    }

    /// Connect to a specific socket path and perform the protocol handshake.
    pub async fn connect_to(sock_path: &std::path::Path) -> Result<Self> {
        if !sock_path.exists() {
            return Err(CliError::Socket(format!(
                "daemon socket not found at {} — is the daemon running? Try: nflow daemon start",
                sock_path.display()
            )));
        }

        let stream = timeout(CONNECTION_TIMEOUT, UnixStream::connect(&sock_path))
            .await
            .map_err(|_| {
                CliError::Socket(format!(
                    "connection timed out after {}s connecting to {}",
                    CONNECTION_TIMEOUT.as_secs(),
                    sock_path.display()
                ))
            })?
            .map_err(|e| {
                CliError::Socket(format!(
                    "failed to connect to daemon at {}: {}",
                    sock_path.display(),
                    e
                ))
            })?;

        let (read_half, write_half) = tokio::io::split(stream);
        let reader = BufReader::new(read_half);

        let mut client = Self {
            reader,
            writer: write_half,
        };

        client.perform_handshake().await?;

        Ok(client)
    }

    /// Perform the NDJSON protocol handshake.
    async fn perform_handshake(&mut self) -> Result<()> {
        let handshake = Handshake {
            protocol_version: PROTOCOL_VERSION,
        };

        self.write_line(&handshake).await?;

        let response: HandshakeResponse = timeout(CONNECTION_TIMEOUT, self.read_line())
            .await
            .map_err(|_| {
                CliError::Socket(format!(
                    "handshake timed out after {}s",
                    CONNECTION_TIMEOUT.as_secs()
                ))
            })??;

        match response.status {
            ResponseStatus::Ok => {
                if response.protocol_version != PROTOCOL_VERSION {
                    return Err(CliError::Socket(format!(
                        "protocol version mismatch: client={}, daemon={}",
                        PROTOCOL_VERSION, response.protocol_version
                    )));
                }
                Ok(())
            }
            ResponseStatus::Error => Err(CliError::Socket(format!(
                "handshake rejected: {}",
                response.message.unwrap_or_else(|| "unknown error".into())
            ))),
        }
    }

    /// Send a command and wait for a single response.
    ///
    /// Uses the default command timeout (60s). For long-running commands,
    /// use `send_command_with_timeout`.
    pub async fn send_command(
        &mut self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<Response> {
        let cmd_timeout = command_timeout(command);
        self.send_command_with_timeout(command, params, cmd_timeout)
            .await
    }

    /// Send a command and wait for a single response with a custom timeout.
    pub async fn send_command_with_timeout(
        &mut self,
        command: &str,
        params: serde_json::Value,
        cmd_timeout: Duration,
    ) -> Result<Response> {
        let request_id = uuid::Uuid::new_v4().to_string();

        let request = Request {
            id: request_id.clone(),
            command: command.to_string(),
            params,
        };

        self.write_line(&request).await?;

        let response: Response = timeout(cmd_timeout, self.read_line()).await.map_err(|_| {
            CliError::Socket(format!(
                "command '{}' timed out after {}s waiting for response",
                command,
                cmd_timeout.as_secs()
            ))
        })??;

        if response.id != request_id {
            return Err(CliError::Socket(format!(
                "response ID mismatch: expected {}, got {}",
                request_id, response.id
            )));
        }

        Ok(response)
    }

    /// Send a command and receive a streaming response.
    ///
    /// Returns a `StreamingReader` that yields lines until `done: true`.
    pub async fn send_streaming_command(
        &mut self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<StreamingReader<'_>> {
        let request_id = uuid::Uuid::new_v4().to_string();

        let request = Request {
            id: request_id.clone(),
            command: command.to_string(),
            params,
        };

        self.write_line(&request).await?;

        Ok(StreamingReader {
            client: self,
            request_id,
            done: false,
        })
    }

    /// Write a serializable value as a single NDJSON line.
    async fn write_line<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| CliError::Socket(format!("failed to serialize message: {}", e)))?;
        line.push('\n');

        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| CliError::Socket(format!("failed to write to daemon socket: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| CliError::Socket(format!("failed to flush daemon socket: {}", e)))?;

        Ok(())
    }

    /// Read a single NDJSON line and deserialize it.
    async fn read_line<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = self.reader.read_line(&mut line).await.map_err(|e| {
                CliError::Socket(format!("failed to read from daemon socket: {}", e))
            })?;

            if bytes_read == 0 {
                return Err(CliError::Socket(
                    "daemon closed the connection unexpectedly".to_string(),
                ));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // skip empty lines per NDJSON spec
            }

            return serde_json::from_str(trimmed).map_err(|e| {
                CliError::Socket(format!("invalid JSON from daemon: {}: {}", e, trimmed))
            });
        }
    }
}

/// A reader for streaming responses from the daemon.
///
/// Yields `StreamingResponseLine` until the final line with `done: true`.
pub struct StreamingReader<'a> {
    client: &'a mut SocketClient,
    request_id: String,
    done: bool,
}

impl StreamingReader<'_> {
    /// Read the next streaming line.
    ///
    /// Returns `None` when the stream is complete (`done: true` was received).
    pub async fn next_line(&mut self) -> Result<Option<StreamingResponseLine>> {
        if self.done {
            return Ok(None);
        }

        let line: StreamingResponseLine = timeout(LONG_COMMAND_TIMEOUT, self.client.read_line())
            .await
            .map_err(|_| {
                CliError::Socket(format!(
                    "streaming response timed out after {}s",
                    LONG_COMMAND_TIMEOUT.as_secs()
                ))
            })??;

        if line.id != self.request_id {
            return Err(CliError::Socket(format!(
                "streaming response ID mismatch: expected {}, got {}",
                self.request_id, line.id
            )));
        }

        if line.done {
            self.done = true;
        }

        Ok(Some(line))
    }

    /// Collect all remaining streaming lines into a Vec.
    pub async fn collect_all(&mut self) -> Result<Vec<StreamingResponseLine>> {
        let mut lines = Vec::new();
        while let Some(line) = self.next_line().await? {
            lines.push(line);
        }
        Ok(lines)
    }
}

/// Determine the appropriate command timeout based on command name.
///
/// Spec and plan commands get 300s; all others get 60s.
fn command_timeout(command: &str) -> Duration {
    if command.starts_with("spec.") || command.starts_with("plan.") {
        LONG_COMMAND_TIMEOUT
    } else {
        DEFAULT_COMMAND_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::net::UnixListener;

    /// Helper: create a temporary Unix socket server that performs handshake
    /// and handles one request-response cycle.
    async fn start_test_server(
        sock_path: PathBuf,
        handler: impl FnOnce(String, String, serde_json::Value) -> Response + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            // Read handshake
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let hs: Handshake = serde_json::from_str(line.trim()).unwrap();

            // Send handshake response
            let hs_resp = if hs.protocol_version == PROTOCOL_VERSION {
                HandshakeResponse {
                    protocol_version: PROTOCOL_VERSION,
                    status: ResponseStatus::Ok,
                    message: None,
                }
            } else {
                HandshakeResponse {
                    protocol_version: PROTOCOL_VERSION,
                    status: ResponseStatus::Error,
                    message: Some("unsupported protocol version".into()),
                }
            };
            let mut resp_line = serde_json::to_string(&hs_resp).unwrap();
            resp_line.push('\n');
            write_half.write_all(resp_line.as_bytes()).await.unwrap();

            if hs.protocol_version != PROTOCOL_VERSION {
                return;
            }

            // Read request
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(line.trim()).unwrap();

            // Call handler
            let response = handler(req.id, req.command, req.params);

            let mut resp_line = serde_json::to_string(&response).unwrap();
            resp_line.push('\n');
            write_half.write_all(resp_line.as_bytes()).await.unwrap();
        })
    }

    /// Helper: create a streaming test server.
    async fn start_streaming_test_server(
        sock_path: PathBuf,
        handler: impl FnOnce(String) -> Vec<StreamingResponseLine> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            // Handshake
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let hs_resp = HandshakeResponse {
                protocol_version: PROTOCOL_VERSION,
                status: ResponseStatus::Ok,
                message: None,
            };
            let mut resp_line = serde_json::to_string(&hs_resp).unwrap();
            resp_line.push('\n');
            write_half.write_all(resp_line.as_bytes()).await.unwrap();

            // Read request
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(line.trim()).unwrap();

            // Send streaming lines
            let lines = handler(req.id);
            for sl in lines {
                let mut resp_line = serde_json::to_string(&sl).unwrap();
                resp_line.push('\n');
                write_half.write_all(resp_line.as_bytes()).await.unwrap();
            }
        })
    }

    fn test_sock_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("nflow-cli-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}.sock", name))
    }

    fn cleanup_sock(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_connect_socket_not_found() {
        let path = std::path::Path::new("/tmp/nflow-test-nonexistent.sock");
        let result = SocketClient::connect_to(path).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("daemon socket not found"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_successful_handshake_and_command() {
        let sock_path = test_sock_path("handshake_cmd");
        cleanup_sock(&sock_path);

        let sp = sock_path.clone();
        let _server = start_test_server(sp, |id, command, _params| Response {
            id,
            status: ResponseStatus::Ok,
            data: serde_json::json!({ "command_received": command }),
        })
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = SocketClient::connect_to(&sock_path).await.unwrap();

        let resp = client
            .send_command("daemon.status", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["command_received"], "daemon.status");

        cleanup_sock(&sock_path);
    }

    #[tokio::test]
    async fn test_handshake_version_mismatch() {
        let sock_path = test_sock_path("version_mismatch");
        cleanup_sock(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();

            // Respond with different protocol version and error
            let hs_resp = HandshakeResponse {
                protocol_version: 999,
                status: ResponseStatus::Error,
                message: Some("unsupported protocol version".into()),
            };
            let mut resp_line = serde_json::to_string(&hs_resp).unwrap();
            resp_line.push('\n');
            write_half.write_all(resp_line.as_bytes()).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = SocketClient::connect_to(&sock_path).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("rejected"), "got: {}", err);

        cleanup_sock(&sock_path);
    }

    #[tokio::test]
    async fn test_command_timeout() {
        let sock_path = test_sock_path("cmd_timeout");
        cleanup_sock(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            // Complete handshake
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let hs_resp = HandshakeResponse {
                protocol_version: PROTOCOL_VERSION,
                status: ResponseStatus::Ok,
                message: None,
            };
            let mut resp = serde_json::to_string(&hs_resp).unwrap();
            resp.push('\n');
            write_half.write_all(resp.as_bytes()).await.unwrap();

            // Read request but never respond
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = SocketClient::connect_to(&sock_path).await.unwrap();

        let result = client
            .send_command_with_timeout(
                "test.slow",
                serde_json::json!({}),
                Duration::from_millis(200),
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timed out"), "got: {}", err);

        cleanup_sock(&sock_path);
    }

    #[tokio::test]
    async fn test_daemon_error_response() {
        let sock_path = test_sock_path("daemon_error");
        cleanup_sock(&sock_path);

        let sp = sock_path.clone();
        let _server = start_test_server(sp, |id, _command, _params| Response {
            id,
            status: ResponseStatus::Error,
            data: serde_json::json!({ "message": "NOT_FOUND: project does not exist" }),
        })
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = SocketClient::connect_to(&sock_path).await.unwrap();

        let resp = client
            .send_command("project.delete", serde_json::json!({"name": "foo"}))
            .await
            .unwrap();

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        cleanup_sock(&sock_path);
    }

    #[tokio::test]
    async fn test_streaming_response() {
        let sock_path = test_sock_path("streaming");
        cleanup_sock(&sock_path);

        let sp = sock_path.clone();
        let _server = start_streaming_test_server(sp, |id| {
            vec![
                StreamingResponseLine {
                    id: id.clone(),
                    status: ResponseStatus::Ok,
                    data: serde_json::json!({ "line": 1 }),
                    done: false,
                },
                StreamingResponseLine {
                    id: id.clone(),
                    status: ResponseStatus::Ok,
                    data: serde_json::json!({ "line": 2 }),
                    done: false,
                },
                StreamingResponseLine {
                    id,
                    status: ResponseStatus::Ok,
                    data: serde_json::json!({ "line": 3, "final": true }),
                    done: true,
                },
            ]
        })
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = SocketClient::connect_to(&sock_path).await.unwrap();

        let mut reader = client
            .send_streaming_command("spec.new", serde_json::json!({"name": "test"}))
            .await
            .unwrap();

        let lines = reader.collect_all().await.unwrap();

        assert_eq!(lines.len(), 3);
        assert!(!lines[0].done);
        assert!(!lines[1].done);
        assert!(lines[2].done);
        assert_eq!(lines[0].data["line"], 1);
        assert_eq!(lines[1].data["line"], 2);
        assert_eq!(lines[2].data["line"], 3);

        cleanup_sock(&sock_path);
    }

    #[tokio::test]
    async fn test_connection_closed_unexpectedly() {
        let sock_path = test_sock_path("closed_unexpectedly");
        cleanup_sock(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            // Complete handshake
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let hs_resp = HandshakeResponse {
                protocol_version: PROTOCOL_VERSION,
                status: ResponseStatus::Ok,
                message: None,
            };
            let mut resp = serde_json::to_string(&hs_resp).unwrap();
            resp.push('\n');
            write_half.write_all(resp.as_bytes()).await.unwrap();

            // Read request, then close connection without responding
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            drop(write_half);
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = SocketClient::connect_to(&sock_path).await.unwrap();

        let result = client.send_command("test.cmd", serde_json::json!({})).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("closed the connection"), "got: {}", err);

        cleanup_sock(&sock_path);
    }

    #[test]
    fn test_command_timeout_values() {
        assert_eq!(command_timeout("daemon.status"), DEFAULT_COMMAND_TIMEOUT);
        assert_eq!(command_timeout("project.init"), DEFAULT_COMMAND_TIMEOUT);
        assert_eq!(command_timeout("spec.new"), LONG_COMMAND_TIMEOUT);
        assert_eq!(command_timeout("spec.resume"), LONG_COMMAND_TIMEOUT);
        assert_eq!(command_timeout("plan.generate"), LONG_COMMAND_TIMEOUT);
        assert_eq!(command_timeout("plan.feedback"), LONG_COMMAND_TIMEOUT);
    }

    #[tokio::test]
    async fn test_multiple_requests_on_same_connection() {
        let sock_path = test_sock_path("multi_req");
        cleanup_sock(&sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            // Handshake
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let hs_resp = HandshakeResponse {
                protocol_version: PROTOCOL_VERSION,
                status: ResponseStatus::Ok,
                message: None,
            };
            let mut resp = serde_json::to_string(&hs_resp).unwrap();
            resp.push('\n');
            write_half.write_all(resp.as_bytes()).await.unwrap();

            // Handle two requests sequentially
            for i in 1..=2 {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                let req: Request = serde_json::from_str(line.trim()).unwrap();

                let response = Response {
                    id: req.id,
                    status: ResponseStatus::Ok,
                    data: serde_json::json!({ "request_number": i }),
                };
                let mut resp_line = serde_json::to_string(&response).unwrap();
                resp_line.push('\n');
                write_half.write_all(resp_line.as_bytes()).await.unwrap();
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = SocketClient::connect_to(&sock_path).await.unwrap();

        let resp1 = client
            .send_command("cmd.one", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(resp1.data["request_number"], 1);

        let resp2 = client
            .send_command("cmd.two", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(resp2.data["request_number"], 2);

        cleanup_sock(&sock_path);
    }
}
