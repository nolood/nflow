//! Integration tests for the NDJSON protocol.
//!
//! These tests exercise the full protocol stack: client ↔ socket server ↔ handler dispatch.
//! Each test creates an isolated NFLOW_HOME with an on-disk SQLite database, starts the
//! socket server with the real command handler, and communicates over Unix sockets.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use crate::db::{open_connection, run_migrations};
    use crate::handlers::{create_handler, HandlerState};
    use crate::socket::{
        HandlerResult, HandshakeResponse, Request, Response, ResponseStatus, SocketServerConfig,
        StreamingResponseLine, PROTOCOL_VERSION,
    };

    /// Create an isolated NFLOW_HOME-like directory structure for testing.
    fn create_test_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let nflow_home = tmp.path().join(".nflow");
        fs::create_dir_all(&nflow_home).unwrap();
        fs::set_permissions(&nflow_home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(nflow_home.join("logs")).unwrap();
        (tmp, nflow_home)
    }

    /// Open a test database in the given nflow_home directory, with migrations applied.
    fn create_test_db(nflow_home: &Path) -> PathBuf {
        let db_path = nflow_home.join("nflow.db");
        let conn = open_connection(&db_path).expect("failed to open test db");
        run_migrations(&conn, &db_path).expect("failed to run migrations");
        db_path
    }

    /// Generate a unique socket path for each test.
    fn temp_socket_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nflow_proto_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("nflow.sock")
    }

    /// Remove socket file and its parent directory.
    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(path.parent().unwrap());
    }

    /// Start a socket server with the real handler dispatch backed by a test DB.
    fn start_real_server(socket_path: &Path, db_path: &Path) -> crate::socket::SocketServerHandle {
        let state = Arc::new(HandlerState {
            db_path: db_path.to_path_buf(),
        });
        let handler = create_handler(state);
        let config = SocketServerConfig {
            socket_path: socket_path.to_path_buf(),
            handler,
            event_bus: None,
        };
        crate::socket::start_server(config).unwrap()
    }

    /// Perform the protocol handshake and verify success.
    async fn do_handshake(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) {
        let handshake = format!("{{\"protocol_version\":{}}}\n", PROTOCOL_VERSION);
        writer.write_all(handshake.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: HandshakeResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.protocol_version, PROTOCOL_VERSION);
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert!(resp.message.is_none());
    }

    /// Send a request and read the response.
    async fn send_request(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        id: &str,
        command: &str,
        params: serde_json::Value,
    ) -> Response {
        let req = serde_json::json!({
            "id": id,
            "command": command,
            "params": params,
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        serde_json::from_str(&resp_line).unwrap()
    }

    // -----------------------------------------------------------------------
    // AC1: Valid request -> valid response with matching UUID
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_protocol_valid_request_matching_uuid() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        // Send a valid "project.list" command (no projects yet, should return empty list)
        let request_id = uuid::Uuid::new_v4().to_string();
        let resp = send_request(
            &mut writer,
            &mut lines,
            &request_id,
            "project.list",
            serde_json::json!({}),
        )
        .await;

        // Response ID must match the request ID
        assert_eq!(resp.id, request_id, "response ID must match request ID");
        assert_eq!(resp.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_multiple_requests_matching_uuids() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        // Send multiple requests with different UUIDs
        for i in 0..3 {
            let request_id = format!("req-{}-{}", i, uuid::Uuid::new_v4());
            let resp = send_request(
                &mut writer,
                &mut lines,
                &request_id,
                "project.list",
                serde_json::json!({}),
            )
            .await;

            assert_eq!(
                resp.id, request_id,
                "response ID must match request ID for request {}",
                i
            );
            assert_eq!(resp.status, ResponseStatus::Ok);
        }

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // AC2: Malformed JSON -> error response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_protocol_malformed_json_error() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        // Send completely invalid JSON
        writer
            .write_all(b"this is not json at all\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("invalid request"),
            "error message should mention 'invalid request', got: {}",
            resp.data["message"]
        );
        // Malformed JSON has no parseable id, so id should be empty
        assert_eq!(resp.id, "", "malformed request should have empty id");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_partial_json_error() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        // Send partial JSON (missing required fields)
        writer.write_all(b"{\"id\":\"partial\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("invalid request"));

        // Connection should still be alive — send valid request after
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "after-error",
            "project.list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp2.id, "after-error");
        assert_eq!(resp2.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // AC3: Unknown command -> NOT_FOUND error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_protocol_unknown_command_error() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        let request_id = uuid::Uuid::new_v4().to_string();
        let resp = send_request(
            &mut writer,
            &mut lines,
            &request_id,
            "nonexistent.command",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.id, request_id, "error response should echo request ID");
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("unknown command"),
            "error should mention 'unknown command', got: {}",
            resp.data["message"]
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_unknown_command_preserves_connection() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        // Unknown command should not break the connection
        let resp1 = send_request(
            &mut writer,
            &mut lines,
            "unknown-1",
            "does.not.exist",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp1.status, ResponseStatus::Error);

        // Valid command should still work after
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "valid-after",
            "project.list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp2.id, "valid-after");
        assert_eq!(resp2.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // AC4: Handshake succeeds/fails
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_protocol_handshake_succeeds() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send correct handshake
        writer
            .write_all(b"{\"protocol_version\":1}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: HandshakeResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.protocol_version, PROTOCOL_VERSION);
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert!(resp.message.is_none());

        // After successful handshake, commands should work
        let cmd_resp = send_request(
            &mut writer,
            &mut lines,
            "post-handshake",
            "project.list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(cmd_resp.id, "post-handshake");
        assert_eq!(cmd_resp.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_handshake_wrong_version_fails() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send handshake with wrong version
        writer
            .write_all(b"{\"protocol_version\":999}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: HandshakeResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp
            .message
            .as_deref()
            .unwrap()
            .contains("unsupported protocol version"));

        // Connection should be closed after failed handshake
        let next = lines.next_line().await.unwrap();
        assert!(
            next.is_none(),
            "connection should be closed after failed handshake"
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_handshake_invalid_json_fails() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send invalid JSON as handshake
        writer.write_all(b"garbage data here\n").await.unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: HandshakeResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp
            .message
            .as_deref()
            .unwrap()
            .contains("invalid handshake"));

        // Connection should be closed
        let next = lines.next_line().await.unwrap();
        assert!(
            next.is_none(),
            "connection should be closed after invalid handshake"
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_handshake_command_before_handshake_fails() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send a command without handshake — server parses it as invalid handshake
        let req = r#"{"id":"1","command":"project.list","params":{}}"#;
        writer
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: HandshakeResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp
            .message
            .as_deref()
            .unwrap()
            .contains("invalid handshake"));

        // Connection should be closed
        let next = lines.next_line().await.unwrap();
        assert!(next.is_none());

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // AC5: Streaming response -> multiple lines until done=true
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_protocol_streaming_response() {
        let socket_path = temp_socket_path();

        // Use a custom streaming handler for this test since spec.new requires
        // Claude binary which isn't available in tests.
        let config = SocketServerConfig {
            socket_path: socket_path.clone(),
            handler: Box::new(|req: Request| {
                if req.command == "stream.test" {
                    let id = req.id.clone();
                    let (tx, rx) = tokio::sync::mpsc::channel(16);
                    tokio::spawn(async move {
                        for i in 1..=3 {
                            let _ = tx
                                .send(StreamingResponseLine::data(
                                    id.clone(),
                                    serde_json::json!({"line": i}),
                                ))
                                .await;
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        let _ = tx
                            .send(StreamingResponseLine::done(
                                id,
                                serde_json::json!({"total": 3}),
                            ))
                            .await;
                    });
                    HandlerResult::Streaming(rx)
                } else {
                    HandlerResult::Single(Response::error(
                        req.id,
                        &format!("unknown command: {}", req.command),
                    ))
                }
            }),
            event_bus: None,
        };

        let handle = crate::socket::start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        let request_id = uuid::Uuid::new_v4().to_string();
        let req = serde_json::json!({
            "id": request_id,
            "command": "stream.test",
            "params": {},
        });
        let mut req_line = serde_json::to_string(&req).unwrap();
        req_line.push('\n');
        writer.write_all(req_line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        // Read streaming lines until done=true
        let mut received = Vec::new();
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                .await
                .expect("timeout waiting for streaming line")
                .unwrap()
                .unwrap();
            let resp: StreamingResponseLine = serde_json::from_str(&line).unwrap();
            assert_eq!(resp.id, request_id, "all streaming lines share request ID");
            assert_eq!(resp.status, ResponseStatus::Ok);
            let is_done = resp.done;
            received.push(resp);
            if is_done {
                break;
            }
        }

        // Should have 3 data lines + 1 done line = 4 total
        assert_eq!(received.len(), 4, "expected 3 data + 1 done lines");

        // First 3 are data lines
        for (i, line) in received[..3].iter().enumerate() {
            assert!(!line.done, "data lines should not be done");
            assert_eq!(line.data["line"], (i + 1) as i64);
        }

        // Last line is done
        assert!(received[3].done, "last line should be done");
        assert_eq!(received[3].data["total"], 3);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_streaming_all_lines_share_id() {
        let socket_path = temp_socket_path();

        let config = SocketServerConfig {
            socket_path: socket_path.clone(),
            handler: Box::new(|req: Request| {
                let id = req.id.clone();
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(async move {
                    for i in 1..=5 {
                        let _ = tx
                            .send(StreamingResponseLine::data(
                                id.clone(),
                                serde_json::json!({"seq": i}),
                            ))
                            .await;
                    }
                    let _ = tx
                        .send(StreamingResponseLine::done(id, serde_json::json!(null)))
                        .await;
                });
                HandlerResult::Streaming(rx)
            }),
            event_bus: None,
        };

        let handle = crate::socket::start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        let request_id = "stream-shared-id-test";
        let req = format!(
            "{{\"id\":\"{}\",\"command\":\"test\",\"params\":{{}}}}\n",
            request_id
        );
        writer.write_all(req.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let mut count = 0;
        loop {
            let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let resp: StreamingResponseLine = serde_json::from_str(&line).unwrap();
            assert_eq!(
                resp.id, request_id,
                "every streaming line must share the request ID"
            );
            count += 1;
            if resp.done {
                break;
            }
        }

        assert_eq!(count, 6, "expected 5 data + 1 done = 6 lines");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // AC6: Client disconnects mid-stream -> daemon handles gracefully
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_protocol_client_disconnect_mid_stream() {
        let socket_path = temp_socket_path();

        // Streaming handler that sends many lines with small delays
        let config = SocketServerConfig {
            socket_path: socket_path.clone(),
            handler: Box::new(|req: Request| {
                let id = req.id.clone();
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(async move {
                    for i in 1..=100 {
                        let result = tx
                            .send(StreamingResponseLine::data(
                                id.clone(),
                                serde_json::json!({"line": i}),
                            ))
                            .await;
                        if result.is_err() {
                            // Receiver dropped — client disconnected
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    let _ = tx
                        .send(StreamingResponseLine::done(id, serde_json::json!(null)))
                        .await;
                });
                HandlerResult::Streaming(rx)
            }),
            event_bus: None,
        };

        let handle = crate::socket::start_server(config).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect, handshake, send streaming request, then disconnect after reading a few lines
        {
            let stream = UnixStream::connect(&socket_path).await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();

            do_handshake(&mut writer, &mut lines).await;

            let req = r#"{"id":"disc1","command":"stream","params":{}}"#;
            writer
                .write_all(format!("{}\n", req).as_bytes())
                .await
                .unwrap();
            writer.flush().await.unwrap();

            // Read only 2 lines then drop the connection
            for _ in 0..2 {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
                    .await;
            }
            // stream is dropped here — client disconnects mid-stream
        }

        // Server should handle gracefully and still accept new connections
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        // Send a normal request to verify server is still functional
        let resp = send_request(
            &mut writer,
            &mut lines,
            "after-disconnect",
            "some.command",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.id, "after-disconnect");
        // Any response is fine — server is still alive and responding

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_client_disconnect_before_handshake() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect and immediately disconnect (no handshake)
        {
            let _stream = UnixStream::connect(&socket_path).await.unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Server should still be alive
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "alive-check",
            "project.list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.id, "alive-check");
        assert_eq!(resp.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_protocol_client_disconnect_after_handshake() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect, complete handshake, then disconnect without sending commands
        {
            let stream = UnixStream::connect(&socket_path).await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            do_handshake(&mut writer, &mut lines).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Server should still accept connections
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        do_handshake(&mut writer, &mut lines).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "still-alive",
            "project.list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.id, "still-alive");
        assert_eq!(resp.status, ResponseStatus::Ok);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }
}
