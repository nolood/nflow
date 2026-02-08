//! Integration tests for execution commands through the socket protocol.
//!
//! Tests exercise full command flow: client → socket server → handler → DB → response.
//! Each test creates an isolated environment with an on-disk SQLite database.
//!
//! exec.retry and exec.skip/exec.continue happy-path tests that would spawn Claude agents
//! use mock Claude binary. Error-path tests exercise handler validation before agents are invoked.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use crate::db::{self, open_connection, run_migrations};
    use crate::handlers::{create_handler, HandlerState};
    use crate::socket::{Response, ResponseStatus, SocketServerConfig, PROTOCOL_VERSION};

    // -----------------------------------------------------------------------
    // Test helpers (same pattern as other integration test files)
    // -----------------------------------------------------------------------

    fn create_test_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let nflow_home = tmp.path().join(".nflow");
        fs::create_dir_all(&nflow_home).unwrap();
        fs::set_permissions(&nflow_home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(nflow_home.join("logs")).unwrap();
        (tmp, nflow_home)
    }

    fn create_test_db(nflow_home: &Path) -> PathBuf {
        let db_path = nflow_home.join("nflow.db");
        let conn = open_connection(&db_path).expect("failed to open test db");
        run_migrations(&conn, &db_path).expect("failed to run migrations");
        db_path
    }

    fn temp_socket_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nflow_exec_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("nflow.sock")
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(path.parent().unwrap());
    }

    fn start_real_server(socket_path: &Path, db_path: &Path) -> crate::socket::SocketServerHandle {
        let state = Arc::new(HandlerState {
            db_path: db_path.to_path_buf(),
            event_bus: None,
        });
        let handler = create_handler(state);
        let config = SocketServerConfig {
            socket_path: socket_path.to_path_buf(),
            handler,
            event_bus: None,
        };
        crate::socket::start_server(config).unwrap()
    }

    async fn do_handshake(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) {
        let handshake = format!("{{\"protocol_version\":{}}}\n", PROTOCOL_VERSION);
        writer.write_all(handshake.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: crate::socket::HandshakeResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.protocol_version, PROTOCOL_VERSION);
        assert_eq!(resp.status, ResponseStatus::Ok);
    }

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

    async fn connect_and_handshake(
        socket_path: &Path,
    ) -> (
        tokio::net::unix::OwnedWriteHalf,
        tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) {
        let stream = UnixStream::connect(socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        do_handshake(&mut writer, &mut lines).await;
        (writer, lines)
    }

    fn insert_test_project(db_path: &Path, name: &str) -> uuid::Uuid {
        let conn = open_connection(db_path).unwrap();
        let project = nflow_core::project::ProjectService::create(
            nflow_core::project::CreateProjectParams {
                name: name.to_string(),
                path: "/tmp/test-project".to_string(),
                remote_url: None,
                git_provider_override: Some(nflow_core::project::GitProvider::Github),
                head_ref: Some("main".to_string()),
            },
            |_| false,
        )
        .unwrap();
        db::projects::insert_project(&conn, &project).unwrap();
        project.id
    }

    /// Insert a full work item hierarchy: session + epic + story + impl task + verify task.
    /// Returns (session_id, epic_id, story_id, impl_task_id, verify_task_id).
    fn insert_exec_hierarchy(
        db_path: &Path,
        project_id: uuid::Uuid,
        wave: u32,
        story_status: &str,
        task_status: &str,
        verify_status: &str,
    ) -> (uuid::Uuid, uuid::Uuid, uuid::Uuid, uuid::Uuid, uuid::Uuid) {
        let conn = open_connection(db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string(), wave],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Test Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, worktree_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Test Story', 'desc', ?4, 'S1', 0, '/tmp/fake-wt', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![
                story_id.to_string(),
                epic_id.to_string(),
                session_id.to_string(),
                story_status
            ],
        )
        .unwrap();

        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Impl Task', 'desc', ?4, 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![
                task_id.to_string(),
                story_id.to_string(),
                session_id.to_string(),
                task_status
            ],
        )
        .unwrap();

        let verify_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'verify', 'Verify Task', 'desc', ?4, 'T1v', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![
                verify_id.to_string(),
                story_id.to_string(),
                session_id.to_string(),
                verify_status
            ],
        )
        .unwrap();

        (session_id, epic_id, story_id, task_id, verify_id)
    }

    /// Insert a dependency between two stories.
    fn insert_dependency(db_path: &Path, blocker_id: uuid::Uuid, blocked_id: uuid::Uuid) {
        let conn = open_connection(db_path).unwrap();
        conn.execute(
            "INSERT INTO dependencies (blocker_id, blocked_id) VALUES (?1, ?2)",
            rusqlite::params![blocker_id.to_string(), blocked_id.to_string()],
        )
        .unwrap();
    }

    /// Read a work item status from DB.
    fn get_item_status(db_path: &Path, item_id: uuid::Uuid) -> String {
        let conn = open_connection(db_path).unwrap();
        conn.query_row(
            "SELECT status FROM work_items WHERE id = ?1",
            rusqlite::params![item_id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Read project execution_enabled from DB.
    fn get_execution_enabled(db_path: &Path, name: &str) -> bool {
        let conn = open_connection(db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, name)
            .unwrap()
            .unwrap();
        project.execution_enabled
    }

    // -----------------------------------------------------------------------
    // exec.run — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_run_missing_project_name() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.run",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("missing required parameter: project_name"),
            "got: {:?}",
            resp.data
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_run_project_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.run",
            serde_json::json!({ "project_name": "nonexistent" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_run_enables_execution() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        // Insert project with execution_enabled = false
        let conn = open_connection(&db_path).unwrap();
        let project = nflow_core::project::Project {
            id: uuid::Uuid::new_v4(),
            name: "exec-run-test".to_string(),
            path: "/tmp/fakepath".to_string(),
            base_branch: "main".to_string(),
            git_provider: nflow_core::project::GitProvider::Github,
            execution_enabled: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::projects::insert_project(&conn, &project).unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.run",
            serde_json::json!({ "project_name": "exec-run-test" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"], true);
        assert_eq!(resp.data["running_count"], 0);
        assert_eq!(resp.data["pending_count"], 0);

        // Verify DB state
        assert!(get_execution_enabled(&db_path, "exec-run-test"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.pause — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_pause_missing_project_name() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.pause",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_pause_disables_execution() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let _project_id = insert_test_project(&db_path, "exec-pause-test");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.pause",
            serde_json::json!({ "project_name": "exec-pause-test" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"], false);

        // Verify DB state
        assert!(!get_execution_enabled(&db_path, "exec-pause-test"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_run_then_pause_round_trip() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        // Insert project with execution_enabled = false
        let conn = open_connection(&db_path).unwrap();
        let project = nflow_core::project::Project {
            id: uuid::Uuid::new_v4(),
            name: "roundtrip-test".to_string(),
            path: "/tmp/fakepath".to_string(),
            base_branch: "main".to_string(),
            git_provider: nflow_core::project::GitProvider::Github,
            execution_enabled: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::projects::insert_project(&conn, &project).unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // exec.run → enable
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.run",
            serde_json::json!({ "project_name": "roundtrip-test" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"], true);

        // exec.pause → disable
        let resp = send_request(
            &mut writer,
            &mut lines,
            "2",
            "exec.pause",
            serde_json::json!({ "project_name": "roundtrip-test" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"], false);

        // Verify final DB state
        assert!(!get_execution_enabled(&db_path, "roundtrip-test"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.status — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_status_missing_project_name() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.status",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_status_empty_project() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let _project_id = insert_test_project(&db_path, "status-empty");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.status",
            serde_json::json!({ "project_name": "status-empty" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["project_name"], "status-empty");
        assert_eq!(resp.data["execution_enabled"], true);
        assert_eq!(resp.data["running_count"], 0);
        let waves = resp.data["waves"].as_array().unwrap();
        assert!(waves.is_empty());

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_status_hierarchical_structure() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "status-hier");
        let (_session_id, _epic_id, _story_id, task_id, _verify_id) = insert_exec_hierarchy(
            &db_path,
            project_id,
            1,
            "in_progress",
            "done",
            "in_progress",
        );

        // Insert an agent run for the impl task
        let conn = open_connection(&db_path).unwrap();
        let run_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO agent_runs (id, work_item_id, status, exit_code, started_at, finished_at)
             VALUES (?1, ?2, 'succeeded', 0, '2024-01-01T00:01:00Z', '2024-01-01T00:05:00Z')",
            rusqlite::params![run_id.to_string(), task_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.status",
            serde_json::json!({ "project_name": "status-hier" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["project_name"], "status-hier");

        let waves = resp.data["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0]["wave_number"], 1);

        let epics = waves[0]["epics"].as_array().unwrap();
        assert_eq!(epics.len(), 1);
        assert_eq!(epics[0]["short_id"], "W1-E1");

        let stories = epics[0]["stories"].as_array().unwrap();
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0]["short_id"], "W1-S1");

        let tasks = stories[0]["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["short_id"], "W1-T1");
        assert_eq!(tasks[0]["kind"], "impl");
        assert_eq!(tasks[0]["status"], "done");
        assert_eq!(tasks[0]["exit_code"], 0);
        assert_eq!(tasks[0]["retry_count"], 1);

        assert_eq!(tasks[1]["short_id"], "W1-T1v");
        assert_eq!(tasks[1]["kind"], "verify");
        assert_eq!(tasks[1]["status"], "in_progress");

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_status_wave_filter() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "status-wave");
        // Wave 1
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "done", "done", "done");
        // Wave 2 — insert a second session manually
        let conn = open_connection(&db_path).unwrap();
        let session2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 2, 'approved', '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![session2_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        let epic2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'W2 Epic', 'desc', 'pending', 'E1', 0, '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![epic2_id.to_string(), session2_id.to_string()],
        )
        .unwrap();
        let story2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'W2 Story', 'desc', 'pending', 'S1', 0, '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![story2_id.to_string(), epic2_id.to_string(), session2_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // Without filter: 2 waves
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.status",
            serde_json::json!({ "project_name": "status-wave" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["waves"].as_array().unwrap().len(), 2);

        // With wave=1: only wave 1
        let resp = send_request(
            &mut writer,
            &mut lines,
            "2",
            "exec.status",
            serde_json::json!({ "project_name": "status-wave", "wave": 1 }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        let waves = resp.data["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0]["wave_number"], 1);

        // Non-existent wave
        let resp = send_request(
            &mut writer,
            &mut lines,
            "3",
            "exec.status",
            serde_json::json!({ "project_name": "status-wave", "wave": 99 }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.stop — integration tests (terminates agents, cancels stories)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_stop_missing_params() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.stop",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_stop_story_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.stop",
            serde_json::json!({ "story_id": "W1-S99" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_stop_story_wrong_state() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "stop-state");
        // Story is pending (not in_progress) — exec.stop should reject
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "pending", "pending", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.stop",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_stop_by_story_no_agents() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "stop-noagents");
        let (_, _, story_id, _task_id, _verify_id) = insert_exec_hierarchy(
            &db_path,
            project_id,
            1,
            "in_progress",
            "in_progress",
            "pending",
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.stop",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["story_id"], "W1-S1");
        assert_eq!(resp.data["agents_stopped"], 0);

        // Verify DB: story cancelled, tasks cancelled/failed
        assert_eq!(get_item_status(&db_path, story_id), "cancelled");

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_stop_by_project() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "stop-proj");
        let (_, _, _story_id, _, _) = insert_exec_hierarchy(
            &db_path,
            project_id,
            1,
            "in_progress",
            "in_progress",
            "pending",
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.stop",
            serde_json::json!({ "project_name": "stop-proj" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["agents_stopped"], 0);

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_stop_wave_requires_project() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.stop",
            serde_json::json!({ "wave": 1 }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("project_name"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.retry — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_retry_missing_task_id() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.retry",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_retry_task_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.retry",
            serde_json::json!({ "task_id": "W1-T99" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_retry_task_not_failed() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "retry-notfailed");
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "in_progress", "pending", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.retry",
            serde_json::json!({ "task_id": "W1-T1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_retry_not_a_task() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "retry-notatask");
        // Insert hierarchy; we'll try to retry the epic
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "failed", "failed", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        // Try to retry the epic (E1), not a task
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.retry",
            serde_json::json!({ "task_id": "W1-E1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(msg.contains("INVALID") || msg.contains("not a task"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.skip — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_skip_missing_task_id() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.skip",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_skip_task_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.skip",
            serde_json::json!({ "task_id": "W1-T99" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_skip_task_not_failed() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "skip-notfailed");
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "in_progress", "pending", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.skip",
            serde_json::json!({ "task_id": "W1-T1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_skip_marks_task_done_and_cancels_verify() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "skip-verify");
        // Story failed, impl task failed, verify task pending
        let (_, _, story_id, task_id, verify_id) =
            insert_exec_hierarchy(&db_path, project_id, 1, "failed", "failed", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.skip",
            serde_json::json!({ "task_id": "W1-T1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["task_id"], "W1-T1");
        assert_eq!(resp.data["skipped_verify"].as_str().unwrap(), "T1v");
        assert_eq!(resp.data["story_completed"], true);

        // Verify DB states
        assert_eq!(get_item_status(&db_path, task_id), "done");
        assert_eq!(get_item_status(&db_path, verify_id), "cancelled");
        assert_eq!(get_item_status(&db_path, story_id), "done");

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.continue — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_continue_missing_story_id() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.continue",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_continue_story_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.continue",
            serde_json::json!({ "story_id": "W1-S99" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_continue_wrong_state() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cont-wrong");
        // Story is in_progress, not failed/cancelled
        let _ = insert_exec_hierarchy(
            &db_path,
            project_id,
            1,
            "in_progress",
            "in_progress",
            "pending",
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.continue",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_continue_not_a_story() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cont-notstory");
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "failed", "failed", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        // Try to continue an epic
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.continue",
            serde_json::json!({ "story_id": "W1-E1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(msg.contains("INVALID") || msg.contains("not a story"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_continue_cancelled_without_worktree_no_blockers() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cont-cancelled");
        let conn = open_connection(&db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Story WITHOUT worktree_path, status cancelled
        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story', 'desc', 'cancelled', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Cancelled tasks
        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task', 'desc', 'cancelled', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.continue",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);

        // Without worktree, no blockers → story set to ready
        let story_status = get_item_status(&db_path, story_id);
        assert!(
            story_status == "ready" || story_status == "pending",
            "expected ready or pending, got: {}",
            story_status
        );

        // Task should be restored to pending
        assert_eq!(get_item_status(&db_path, task_id), "pending");

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.cancel — integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_cancel_missing_story_id() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.cancel",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_cancel_story_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.cancel",
            serde_json::json!({ "story_id": "W1-S99" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_cancel_wrong_state() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cancel-wrong");
        // Story is in_progress — cannot cancel (use exec.stop instead)
        let _ = insert_exec_hierarchy(
            &db_path,
            project_id,
            1,
            "in_progress",
            "in_progress",
            "pending",
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.cancel",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_cancel_pending_story() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cancel-pending");
        let (_, _, story_id, task_id, verify_id) =
            insert_exec_hierarchy(&db_path, project_id, 1, "pending", "pending", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.cancel",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["story_id"], "W1-S1");
        assert_eq!(resp.data["cancelled"], true);

        // Verify DB: story and tasks cancelled
        assert_eq!(get_item_status(&db_path, story_id), "cancelled");
        assert_eq!(get_item_status(&db_path, task_id), "cancelled");
        assert_eq!(get_item_status(&db_path, verify_id), "cancelled");

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_cancel_ready_story() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cancel-ready");
        let conn = open_connection(&db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story', 'desc', 'ready', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task', 'desc', 'pending', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.cancel",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["cancelled"], true);

        // Verify DB
        assert_eq!(get_item_status(&db_path, story_id), "cancelled");
        assert_eq!(get_item_status(&db_path, task_id), "cancelled");

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_exec_cancel_with_dependents_warns() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "cancel-deps");
        let conn = open_connection(&db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Story S1 (blocker) — pending
        let story1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story 1', 'desc', 'pending', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story1_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Story S2 (blocked by S1) — pending
        let story2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story 2', 'desc', 'pending', 'S2', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story2_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        // Add dependency: S1 blocks S2
        insert_dependency(&db_path, story1_id, story2_id);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.cancel",
            serde_json::json!({ "story_id": "W1-S1" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["cancelled"], true);
        // Should warn about dependent S2
        assert!(resp.data["warning"].as_str().is_some());

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.run + exec.status combined flow
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_run_then_status_shows_pending() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "flow-test");
        let _ = insert_exec_hierarchy(&db_path, project_id, 1, "pending", "pending", "pending");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // exec.run
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.run",
            serde_json::json!({ "project_name": "flow-test" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"], true);
        assert_eq!(resp.data["pending_count"], 1);

        // exec.status
        let resp = send_request(
            &mut writer,
            &mut lines,
            "2",
            "exec.status",
            serde_json::json!({ "project_name": "flow-test" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"], true);

        let waves = resp.data["waves"].as_array().unwrap();
        assert_eq!(waves.len(), 1);
        let summary = &waves[0]["summary"];
        assert_eq!(summary["pending"], 1);

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // exec.skip + exec.cancel combined flow
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_exec_skip_then_cancel_remaining() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "skip-cancel-flow");
        let conn = open_connection(&db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Story S1 — failed, with failed impl task
        let story1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, worktree_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story 1', 'desc', 'failed', 'S1', 0, '/tmp/fake-wt1', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story1_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let task1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task 1', 'desc', 'failed', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task1_id.to_string(), story1_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let verify1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'verify', 'Verify 1', 'desc', 'pending', 'T1v', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![verify1_id.to_string(), story1_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Story S2 — pending
        let story2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story 2', 'desc', 'pending', 'S2', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story2_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let task2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task 2', 'desc', 'pending', 'T2', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task2_id.to_string(), story2_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // Skip the failed task in S1 → story completes
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "exec.skip",
            serde_json::json!({ "task_id": "W1-T1" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["story_completed"], true);
        assert_eq!(get_item_status(&db_path, story1_id), "done");

        // Cancel S2
        let resp = send_request(
            &mut writer,
            &mut lines,
            "2",
            "exec.cancel",
            serde_json::json!({ "story_id": "W1-S2" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["cancelled"], true);
        assert_eq!(get_item_status(&db_path, story2_id), "cancelled");

        handle.shutdown();
        cleanup(&socket_path);
    }
}
