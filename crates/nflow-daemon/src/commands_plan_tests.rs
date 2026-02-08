//! Integration tests for plan commands through the socket protocol.
//!
//! Tests exercise full command flow: client → socket server → handler → DB → response.
//! Each test creates an isolated environment with an on-disk SQLite database.
//!
//! plan.generate and plan.feedback are streaming handlers that spawn Claude. The happy-path
//! test uses a mock Claude binary that outputs deterministic stream-json. Error-path tests
//! exercise handler validation before Claude is invoked.

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
    use crate::socket::{
        Response, ResponseStatus, SocketServerConfig, StreamingResponseLine, PROTOCOL_VERSION,
    };

    // -----------------------------------------------------------------------
    // Test helpers (same pattern as commands_project_spec_tests)
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
        let dir = std::env::temp_dir().join(format!("nflow_plan_test_{}", uuid::Uuid::new_v4()));
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

    /// Send a request and read streaming response lines until done=true.
    /// Returns all streaming lines collected.
    async fn send_streaming_request(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        id: &str,
        command: &str,
        params: serde_json::Value,
    ) -> Vec<StreamingResponseLine> {
        let req = serde_json::json!({
            "id": id,
            "command": command,
            "params": params,
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let mut result = Vec::new();
        loop {
            let resp_line =
                tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                    .await
                    .expect("streaming response timed out")
                    .unwrap()
                    .unwrap();

            let streaming_line: StreamingResponseLine = serde_json::from_str(&resp_line).unwrap();
            let is_done = streaming_line.done;
            result.push(streaming_line);
            if is_done {
                break;
            }
        }
        result
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

    fn insert_test_spec(
        db_path: &Path,
        project_id: uuid::Uuid,
        name: &str,
        status: nflow_core::spec::SpecStatus,
    ) -> nflow_core::spec::Spec {
        insert_test_spec_with_path(
            db_path,
            project_id,
            name,
            status,
            &format!("/tmp/{}.md", name),
        )
    }

    fn insert_test_spec_with_path(
        db_path: &Path,
        project_id: uuid::Uuid,
        name: &str,
        status: nflow_core::spec::SpecStatus,
        file_path: &str,
    ) -> nflow_core::spec::Spec {
        let conn = open_connection(db_path).unwrap();
        let mut spec =
            nflow_core::spec::Spec::new(project_id, name.to_string(), file_path.to_string());
        match status {
            nflow_core::spec::SpecStatus::Draft => {}
            nflow_core::spec::SpecStatus::Approved => {
                spec.approve().unwrap();
            }
            nflow_core::spec::SpecStatus::Decomposed => {
                spec.approve().unwrap();
                spec.decompose().unwrap();
            }
            nflow_core::spec::SpecStatus::Deleted => {
                spec.delete().unwrap();
            }
        }
        db::specs::insert_spec(&conn, &spec).unwrap();
        spec
    }

    /// Insert a decomposition session directly into DB.
    fn insert_test_session(
        db_path: &Path,
        project_id: uuid::Uuid,
        wave_number: u32,
        status: nflow_core::decomposition::DecompositionStatus,
    ) -> nflow_core::decomposition::DecompositionSession {
        let conn = open_connection(db_path).unwrap();
        let session = nflow_core::decomposition::DecompositionSession {
            id: uuid::Uuid::new_v4(),
            project_id,
            wave_number,
            status,
            claude_session_id: Some("test-claude-session-id".to_string()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();
        session
    }

    /// Insert a decomposition spec link (session ↔ spec).
    fn insert_test_decomposition_spec(db_path: &Path, session_id: uuid::Uuid, spec_id: uuid::Uuid) {
        let conn = open_connection(db_path).unwrap();
        let ds = nflow_core::decomposition::DecompositionSpec::new(session_id, spec_id);
        db::decomposition_sessions::insert_decomposition_spec(&conn, &ds).unwrap();
    }

    /// Insert work items (epic → story → tasks) into DB for a session.
    /// Returns (epic_id, story_id, task_ids).
    fn insert_test_work_items(
        db_path: &Path,
        session_id: uuid::Uuid,
        wave_number: u32,
    ) -> (uuid::Uuid, uuid::Uuid, Vec<uuid::Uuid>) {
        let conn = open_connection(db_path).unwrap();

        let epic = nflow_core::work_item::WorkItem::new_epic(
            session_id,
            "Test Epic".to_string(),
            "Epic description".to_string(),
            "E1".to_string(),
            0,
        );
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = nflow_core::work_item::WorkItem::new_story(
            epic.id,
            session_id,
            "Test Story".to_string(),
            "Story description".to_string(),
            "Story AC".to_string(),
            format!("W{}-S1", wave_number),
            0,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let task = nflow_core::work_item::WorkItem::new_task(
            story.id,
            session_id,
            "Test Task".to_string(),
            "Task description".to_string(),
            "Task AC".to_string(),
            "T1".to_string(),
            0,
        );
        db::work_items::insert_work_item(&conn, &task).unwrap();

        // Auto-generate verify task
        let verify_tasks = nflow_core::work_item::auto_generate_verify_tasks(&[task.clone()]);
        for vt in &verify_tasks {
            db::work_items::insert_work_item(&conn, vt).unwrap();
        }

        let mut task_ids = vec![task.id];
        for vt in &verify_tasks {
            task_ids.push(vt.id);
        }

        (epic.id, story.id, task_ids)
    }

    /// Create a mock Claude binary that outputs deterministic stream-json.
    /// Returns the absolute path to the mock binary.
    /// The binary is placed in a unique directory under /tmp that persists past test end.
    fn create_mock_claude_binary() -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/nflow_mock_claude_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let mock_path = dir.join("claude");

        // The mock outputs:
        // 1. A text delta (simulating Claude thinking)
        // 2. A result event with valid decomposition JSON
        let decomposition_json = serde_json::json!({
            "epics": [{
                "id": "E1",
                "title": "Test Epic",
                "description": "A test epic",
                "stories": [{
                    "id": "S1",
                    "title": "Test Story One",
                    "description": "First story",
                    "acceptance_criteria": "It works",
                    "depends_on": [],
                    "tasks": [{
                        "id": "T1",
                        "title": "Implement feature",
                        "description": "Write the code",
                        "acceptance_criteria": "Code compiles"
                    }]
                }, {
                    "id": "S2",
                    "title": "Test Story Two",
                    "description": "Second story",
                    "acceptance_criteria": "It also works",
                    "depends_on": ["S1"],
                    "tasks": [{
                        "id": "T2",
                        "title": "Add tests",
                        "description": "Write tests",
                        "acceptance_criteria": "Tests pass"
                    }]
                }]
            }]
        });

        let result_json = decomposition_json.to_string();

        // The mock script outputs stream-json lines to stdout
        let script = format!(
            r#"#!/bin/bash
# Mock Claude binary for testing plan.generate
# Outputs stream-json format: text delta + result event
echo '{{"type":"stream_event","event":{{"delta":{{"type":"text_delta","text":"Analyzing specs..."}}}}}}'
echo '{{"type":"result","result":"{}","session_id":"mock-session-123"}}'
"#,
            result_json.replace('"', "\\\"")
        );

        fs::write(&mock_path, script).unwrap();
        fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

        mock_path
    }

    // -----------------------------------------------------------------------
    // plan.generate — error paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_generate_missing_project_name() {
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
            "plan.generate",
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
    async fn test_plan_generate_project_not_found() {
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
            "plan.generate",
            serde_json::json!({"project_name": "nonexistent"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_generate_no_approved_specs() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        // Only draft spec — no approved ones
        insert_test_spec(
            &db_path,
            project_id,
            "draft-spec",
            nflow_core::spec::SpecStatus::Draft,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.generate",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("no approved specs"),
            "got: {:?}",
            resp.data
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_generate_existing_draft_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "my-spec",
            nflow_core::spec::SpecStatus::Approved,
        );

        // Insert an existing draft (in_progress) session
        insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.generate",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("already has a draft wave"),
            "got: {:?}",
            resp.data
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // plan.generate — happy path with mock Claude binary
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_generate_with_mock_claude() {
        let (tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");

        // Create a real spec file that the handler can read
        let spec_file = tmp.path().join("test-spec.md");
        fs::write(&spec_file, "# Test Spec\n\nThis is a test specification.").unwrap();
        insert_test_spec_with_path(
            &db_path,
            project_id,
            "test-spec",
            nflow_core::spec::SpecStatus::Approved,
            spec_file.to_str().unwrap(),
        );

        // Create mock Claude binary and set env vars for the test.
        // Use NFLOW_CLAUDE_BINARY (absolute path) instead of PATH manipulation
        // to avoid race conditions with parallel tests.
        let mock_claude_path = create_mock_claude_binary();

        let original_home = std::env::var("HOME").unwrap_or_default();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("NFLOW_CLAUDE_BINARY", mock_claude_path.to_str().unwrap());

        // Create the project specs directory that the handler uses as working_dir
        // when with_codebase=false: ~/.nflow/projects/{name}/specs/
        fs::create_dir_all(nflow_home.join("projects").join("test-proj").join("specs")).unwrap();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let streaming_lines = send_streaming_request(
            &mut writer,
            &mut lines,
            "gen1",
            "plan.generate",
            serde_json::json!({
                "project_name": "test-proj",
                "with_codebase": false
            }),
        )
        .await;

        // Restore env vars
        std::env::set_var("HOME", &original_home);
        std::env::remove_var("NFLOW_CLAUDE_BINARY");

        // The last streaming line should be done=true with work item counts
        let last = streaming_lines.last().unwrap();
        assert!(last.done, "last streaming line should have done=true");
        assert_eq!(
            last.status,
            ResponseStatus::Ok,
            "streaming response should be Ok, got: {:?}",
            last.data
        );

        // Verify work item counts in the final response
        let data = &last.data;
        assert_eq!(data["epic_count"].as_u64().unwrap(), 1);
        assert_eq!(data["story_count"].as_u64().unwrap(), 2);
        // 2 impl tasks + 2 verify tasks = 4
        assert_eq!(data["task_count"].as_u64().unwrap(), 4);
        assert!(data["wave_number"].as_u64().unwrap() >= 1);

        // Verify work items are in DB
        let conn = open_connection(&db_path).unwrap();
        let session_id_str = data["session_id"].as_str().unwrap();
        let session_id = uuid::Uuid::parse_str(session_id_str).unwrap();
        let items = db::work_items::list_work_items_by_session(&conn, &session_id).unwrap();

        // Should have: 1 epic + 2 stories + 2 impl tasks + 2 verify tasks = 7
        assert_eq!(items.len(), 7, "expected 7 work items, got {}", items.len());

        let epics: Vec<_> = items
            .iter()
            .filter(|i| i.item_type == nflow_core::work_item::ItemType::Epic)
            .collect();
        assert_eq!(epics.len(), 1);

        let stories: Vec<_> = items
            .iter()
            .filter(|i| i.item_type == nflow_core::work_item::ItemType::Story)
            .collect();
        assert_eq!(stories.len(), 2);

        let tasks: Vec<_> = items
            .iter()
            .filter(|i| i.item_type == nflow_core::work_item::ItemType::Task)
            .collect();
        assert_eq!(tasks.len(), 4); // 2 impl + 2 verify

        // Verify dependencies exist (S2 depends on S1)
        let deps = db::work_items::list_dependencies_by_session(&conn, &session_id).unwrap();
        assert_eq!(deps.len(), 1, "expected 1 dependency");

        // Verify spec was marked as decomposed
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "test-spec")
            .unwrap()
            .unwrap();
        assert_eq!(spec.status, nflow_core::spec::SpecStatus::Decomposed);

        handle.shutdown();
        cleanup(&socket_path);
        // Clean up mock binary directory
        let _ = fs::remove_dir_all(mock_claude_path.parent().unwrap());
    }

    // -----------------------------------------------------------------------
    // plan.show
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_show_tree_format() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        let session = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_work_items(&db_path, session.id, 1);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.show",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["wave_number"].as_u64().unwrap(), 1);
        assert_eq!(resp.data["status"].as_str().unwrap(), "in_progress");

        let epics = resp.data["epics"].as_array().unwrap();
        assert_eq!(epics.len(), 1);
        assert_eq!(epics[0]["title"].as_str().unwrap(), "Test Epic");

        let stories = epics[0]["stories"].as_array().unwrap();
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0]["title"].as_str().unwrap(), "Test Story");

        let tasks = stories[0]["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2); // impl + verify

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_show_dag_format() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        let session = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_work_items(&db_path, session.id, 1);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.show",
            serde_json::json!({"project_name": "test-proj", "dag": true}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["wave_number"].as_u64().unwrap(), 1);
        assert!(resp.data["adjacency"].is_object());

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_show_specific_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");

        // Wave 1 (approved)
        let session1 = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::Approved,
        );
        insert_test_work_items(&db_path, session1.id, 1);

        // Wave 2 (draft)
        let session2 = insert_test_session(
            &db_path,
            project_id,
            2,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_work_items(&db_path, session2.id, 2);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // Request wave 1 specifically
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.show",
            serde_json::json!({"project_name": "test-proj", "wave": 1}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["wave_number"].as_u64().unwrap(), 1);
        assert_eq!(resp.data["status"].as_str().unwrap(), "approved");

        // Default (latest) should be wave 2
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "2",
            "plan.show",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp2.status, ResponseStatus::Ok);
        assert_eq!(resp2.data["wave_number"].as_u64().unwrap(), 2);

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_show_no_waves() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        insert_test_project(&db_path, "test-proj");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.show",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("no decomposition waves"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // plan.approve
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_approve_latest_draft() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        let session = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_work_items(&db_path, session.id, 1);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.approve",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["wave_number"].as_u64().unwrap(), 1);
        assert_eq!(resp.data["status"].as_str().unwrap(), "approved");
        // execution_enabled defaults to true in ProjectService::create()
        assert_eq!(resp.data["execution_enabled"].as_bool().unwrap(), true);

        // Verify in DB
        let conn = open_connection(&db_path).unwrap();
        let updated = db::decomposition_sessions::get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status,
            nflow_core::decomposition::DecompositionStatus::Approved
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_approve_with_auto_execute() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        let session = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_work_items(&db_path, session.id, 1);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.approve",
            serde_json::json!({"project_name": "test-proj", "auto_execute": true}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["execution_enabled"].as_bool().unwrap(), true);

        // Verify project.execution_enabled is true in DB
        let conn = open_connection(&db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, "test-proj")
            .unwrap()
            .unwrap();
        assert!(project.execution_enabled);

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_approve_specific_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        let session = insert_test_session(
            &db_path,
            project_id,
            3,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_work_items(&db_path, session.id, 3);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.approve",
            serde_json::json!({"project_name": "test-proj", "wave_number": 3}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["wave_number"].as_u64().unwrap(), 3);
        assert_eq!(resp.data["status"].as_str().unwrap(), "approved");

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_approve_already_approved() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::Approved,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // plan.approve defaults to "latest draft" — no draft exists here
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.approve",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        // Try with explicit wave number — should fail because already approved
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "2",
            "plan.approve",
            serde_json::json!({"project_name": "test-proj", "wave_number": 1}),
        )
        .await;

        assert_eq!(resp2.status, ResponseStatus::Error);
        assert!(resp2.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // plan.discard
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_discard_draft_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        let spec = insert_test_spec(
            &db_path,
            project_id,
            "my-spec",
            nflow_core::spec::SpecStatus::Decomposed,
        );
        let session = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_decomposition_spec(&db_path, session.id, spec.id);
        insert_test_work_items(&db_path, session.id, 1);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.discard",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["wave_number"].as_u64().unwrap(), 1);
        assert_eq!(resp.data["status"].as_str().unwrap(), "discarded");

        // Verify in DB: session is discarded
        let conn = open_connection(&db_path).unwrap();
        let updated = db::decomposition_sessions::get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status,
            nflow_core::decomposition::DecompositionStatus::Discarded
        );

        // Verify work items were deleted
        let items = db::work_items::list_work_items_by_session(&conn, &session.id).unwrap();
        assert!(
            items.is_empty(),
            "work items should be deleted after discard"
        );

        // Verify spec was reverted to approved
        let updated_spec = db::specs::get_spec_by_name(&conn, &project_id, "my-spec")
            .unwrap()
            .unwrap();
        assert_eq!(updated_spec.status, nflow_core::spec::SpecStatus::Approved);

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_discard_already_discarded() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::Discarded,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.discard",
            serde_json::json!({"project_name": "test-proj", "wave_number": 1}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("already discarded"),
            "got: {:?}",
            resp.data
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_discard_no_draft_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        insert_test_project(&db_path, "test-proj");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.discard",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // plan.feedback — error paths (streaming handler, needs Claude for happy path)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_feedback_missing_message() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.feedback",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: message"));

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_feedback_no_draft_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        insert_test_project(&db_path, "test-proj");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.feedback",
            serde_json::json!({"project_name": "test-proj", "message": "improve this"}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("no draft wave"),
            "got: {:?}",
            resp.data
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_plan_feedback_on_approved_wave() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");
        insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::Approved,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.feedback",
            serde_json::json!({
                "project_name": "test-proj",
                "message": "please improve",
                "wave_number": 1
            }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(
            resp.data["message"]
                .as_str()
                .unwrap()
                .contains("INVALID_STATE"),
            "got: {:?}",
            resp.data
        );

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // Full workflow: generate → show → approve → discard cycle
    // Uses direct DB insertion for work items (bypasses Claude streaming).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_plan_workflow_show_approve_discard() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "test-proj");

        // Create wave 1 with work items (simulate plan.generate)
        let spec = insert_test_spec(
            &db_path,
            project_id,
            "spec-1",
            nflow_core::spec::SpecStatus::Decomposed,
        );
        let session = insert_test_session(
            &db_path,
            project_id,
            1,
            nflow_core::decomposition::DecompositionStatus::InProgress,
        );
        insert_test_decomposition_spec(&db_path, session.id, spec.id);
        insert_test_work_items(&db_path, session.id, 1);

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // Step 1: plan.show → verify wave is in_progress
        let resp = send_request(
            &mut writer,
            &mut lines,
            "1",
            "plan.show",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["status"].as_str().unwrap(), "in_progress");

        // Step 2: plan.approve → wave transitions to approved
        let resp = send_request(
            &mut writer,
            &mut lines,
            "2",
            "plan.approve",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["status"].as_str().unwrap(), "approved");

        // Step 3: plan.show → verify wave is approved
        let resp = send_request(
            &mut writer,
            &mut lines,
            "3",
            "plan.show",
            serde_json::json!({"project_name": "test-proj"}),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["status"].as_str().unwrap(), "approved");

        // Step 4: plan.discard → approved wave is discarded (with wave_number to target it)
        let resp = send_request(
            &mut writer,
            &mut lines,
            "4",
            "plan.discard",
            serde_json::json!({"project_name": "test-proj", "wave_number": 1}),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["status"].as_str().unwrap(), "discarded");

        // Step 5: plan.show → no waves (since the only wave is discarded, and show filters it)
        // Actually, plan.show doesn't filter discarded — it shows all. Let me verify.
        let resp = send_request(
            &mut writer,
            &mut lines,
            "5",
            "plan.show",
            serde_json::json!({"project_name": "test-proj", "wave": 1}),
        )
        .await;
        // The discarded wave should still be showable
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["status"].as_str().unwrap(), "discarded");

        // Verify spec was reverted to approved
        let conn = open_connection(&db_path).unwrap();
        let updated_spec = db::specs::get_spec_by_name(&conn, &project_id, "spec-1")
            .unwrap()
            .unwrap();
        assert_eq!(updated_spec.status, nflow_core::spec::SpecStatus::Approved);

        handle.shutdown();
        cleanup(&socket_path);
    }
}
