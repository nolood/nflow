use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::daemon_client::socket_path;
use crate::error::{Result, TuiError};

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

#[allow(dead_code)]
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

impl SocketClient {
    /// Connect to the daemon socket and perform the protocol handshake.
    pub async fn connect() -> Result<Self> {
        let sock_path = socket_path()?;
        Self::connect_to(&sock_path).await
    }

    /// Connect to a specific socket path and perform the protocol handshake.
    pub async fn connect_to(sock_path: &std::path::Path) -> Result<Self> {
        if !sock_path.exists() {
            return Err(TuiError::Socket(format!(
                "daemon socket not found at {} — is the daemon running? Try: nflow daemon start",
                sock_path.display()
            )));
        }

        let stream = timeout(CONNECTION_TIMEOUT, UnixStream::connect(&sock_path))
            .await
            .map_err(|_| {
                TuiError::Socket(format!(
                    "connection timed out after {}s connecting to {}",
                    CONNECTION_TIMEOUT.as_secs(),
                    sock_path.display()
                ))
            })?
            .map_err(|e| {
                TuiError::Socket(format!(
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
                TuiError::Socket(format!(
                    "handshake timed out after {}s",
                    CONNECTION_TIMEOUT.as_secs()
                ))
            })??;

        match response.status {
            ResponseStatus::Ok => {
                if response.protocol_version != PROTOCOL_VERSION {
                    return Err(TuiError::Socket(format!(
                        "protocol version mismatch: client={}, daemon={}",
                        PROTOCOL_VERSION, response.protocol_version
                    )));
                }
                Ok(())
            }
            ResponseStatus::Error => Err(TuiError::Socket(format!(
                "handshake rejected: {}",
                response.message.unwrap_or_else(|| "unknown error".into())
            ))),
        }
    }

    /// Send a command and wait for a single response.
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
            TuiError::Socket(format!(
                "command '{}' timed out after {}s waiting for response",
                command,
                cmd_timeout.as_secs()
            ))
        })??;

        if response.id != request_id {
            return Err(TuiError::Socket(format!(
                "response ID mismatch: expected {}, got {}",
                request_id, response.id
            )));
        }

        Ok(response)
    }

    /// Write a serializable value as a single NDJSON line.
    async fn write_line<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| TuiError::Socket(format!("failed to serialize message: {}", e)))?;
        line.push('\n');

        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| TuiError::Socket(format!("failed to write to daemon socket: {}", e)))?;
        self.writer
            .flush()
            .await
            .map_err(|e| TuiError::Socket(format!("failed to flush daemon socket: {}", e)))?;

        Ok(())
    }

    /// Read a single NDJSON line and deserialize it.
    async fn read_line<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = self.reader.read_line(&mut line).await.map_err(|e| {
                TuiError::Socket(format!("failed to read from daemon socket: {}", e))
            })?;

            if bytes_read == 0 {
                return Err(TuiError::Socket(
                    "daemon closed the connection unexpectedly".to_string(),
                ));
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            return serde_json::from_str(trimmed).map_err(|e| {
                TuiError::Socket(format!("invalid JSON from daemon: {}: {}", e, trimmed))
            });
        }
    }
}

/// Determine the appropriate command timeout based on command name.
fn command_timeout(command: &str) -> Duration {
    if command.starts_with("spec.") || command.starts_with("plan.") {
        LONG_COMMAND_TIMEOUT
    } else {
        DEFAULT_COMMAND_TIMEOUT
    }
}
