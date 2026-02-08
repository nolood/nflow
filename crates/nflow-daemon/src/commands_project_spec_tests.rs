//! Integration tests for project and spec commands through the socket protocol.
//!
//! Tests exercise full command flow: client → socket server → handler → DB → response.
//! Each test creates an isolated environment with an on-disk SQLite database.

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
        let dir = std::env::temp_dir().join(format!("nflow_cmd_test_{}", uuid::Uuid::new_v4()));
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

    /// Perform the protocol handshake and verify success.
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

    /// Create a temporary git repo for testing project.init.
    fn create_temp_git_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let repo_path = tmp.path().to_path_buf();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .expect("failed to init git repo");
        // Configure git user for the repo (needed for commits)
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_path)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_path)
            .output()
            .ok();
        (tmp, repo_path)
    }

    /// Helper: connect to server and do handshake, return writer + lines.
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

    /// Insert a project directly into DB for tests that don't need project.init's git validation.
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

    /// Insert a spec directly into DB for tests that don't need spec.new streaming.
    fn insert_test_spec(
        db_path: &Path,
        project_id: uuid::Uuid,
        name: &str,
        status: nflow_core::spec::SpecStatus,
    ) -> nflow_core::spec::Spec {
        let conn = open_connection(db_path).unwrap();
        let mut spec =
            nflow_core::spec::Spec::new(project_id, name.to_string(), format!("/tmp/{}.md", name));
        // Transition to desired status
        match status {
            nflow_core::spec::SpecStatus::Draft => {} // already draft
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

    // -----------------------------------------------------------------------
    // project.init -> created in DB + directories exist; duplicate -> ALREADY_EXISTS
    //
    // Consolidated into a single test because set_var("HOME") is process-global
    // and cannot run safely in parallel with other tests.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_project_init_and_duplicate() {
        let (tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let (_repo_tmp, repo_path) = create_temp_git_repo();

        // Set HOME so that create_project_dirs uses our test nflow_home
        std::env::set_var("HOME", tmp.path());

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // --- Part 1: project.init success ---
        let resp = send_request(
            &mut writer,
            &mut lines,
            "init-1",
            "project.init",
            serde_json::json!({
                "name": "test-project",
                "path": repo_path.to_str().unwrap(),
                "git_provider": "github",
            }),
        )
        .await;

        assert_eq!(resp.id, "init-1");
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["name"], "test-project");
        assert_eq!(resp.data["path"], repo_path.to_str().unwrap());
        assert!(resp.data["project_id"].as_str().is_some());

        // Verify project exists in DB
        let conn = open_connection(&db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, "test-project").unwrap();
        assert!(project.is_some(), "project should exist in DB after init");
        let project = project.unwrap();
        assert_eq!(project.name, "test-project");
        drop(conn);

        // Verify project directories were created
        let project_dir = nflow_home.join("projects").join("test-project");
        assert!(project_dir.join("specs").exists(), "specs dir should exist");
        assert!(
            project_dir.join("agent-logs").exists(),
            "agent-logs dir should exist"
        );

        // --- Part 2: project.init duplicate name -> ALREADY_EXISTS ---
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "init-dup",
            "project.init",
            serde_json::json!({
                "name": "test-project",
                "path": repo_path.to_str().unwrap(),
                "git_provider": "github",
            }),
        )
        .await;

        assert_eq!(resp2.id, "init-dup");
        assert_eq!(resp2.status, ResponseStatus::Error);
        let msg = resp2.data["message"].as_str().unwrap();
        assert!(
            msg.contains("ALREADY_EXISTS"),
            "error should contain ALREADY_EXISTS, got: {}",
            msg
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // project.list, project.delete cascade
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_project_list_empty() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "list-empty",
            "project.list",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        let projects = resp.data["projects"].as_array().unwrap();
        assert!(projects.is_empty(), "project list should be empty");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_project_list_with_projects() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        // Insert projects directly into DB
        insert_test_project(&db_path, "project-alpha");
        insert_test_project(&db_path, "project-beta");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "list-projects",
            "project.list",
            serde_json::json!({}),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        let projects = resp.data["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 2, "should list both projects");

        let names: Vec<&str> = projects
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"project-alpha"));
        assert!(names.contains(&"project-beta"));

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_project_delete_cascade() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        // Insert project with specs
        let project_id = insert_test_project(&db_path, "to-delete");
        insert_test_spec(
            &db_path,
            project_id,
            "spec-one",
            nflow_core::spec::SpecStatus::Draft,
        );
        insert_test_spec(
            &db_path,
            project_id,
            "spec-two",
            nflow_core::spec::SpecStatus::Approved,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // Verify project and specs exist before delete
        let conn = open_connection(&db_path).unwrap();
        let specs_before = db::specs::list_specs_by_project(&conn, &project_id).unwrap();
        assert_eq!(specs_before.len(), 2);
        drop(conn);

        // Delete the project
        let resp = send_request(
            &mut writer,
            &mut lines,
            "delete-1",
            "project.delete",
            serde_json::json!({ "name": "to-delete" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["name"], "to-delete");
        assert_eq!(resp.data["deleted"], true);

        // Verify project gone from DB
        let conn = open_connection(&db_path).unwrap();
        let project_after = db::projects::get_project_by_name(&conn, "to-delete").unwrap();
        assert!(project_after.is_none(), "project should be deleted from DB");

        // Verify specs cascaded (FK cascade deletes them)
        let specs_after = db::specs::list_specs_by_project(&conn, &project_id).unwrap();
        assert!(specs_after.is_empty(), "specs should be cascade-deleted");

        // Verify project not in list
        drop(conn);
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "list-after",
            "project.list",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp2.status, ResponseStatus::Ok);
        let projects = resp2.data["projects"].as_array().unwrap();
        assert!(projects.is_empty(), "deleted project should not appear");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_project_delete_not_found() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "del-404",
            "project.delete",
            serde_json::json!({ "name": "nonexistent" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(
            msg.contains("NOT_FOUND"),
            "should contain NOT_FOUND, got: {}",
            msg
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // spec.new error paths (streaming requires Claude — test error paths only)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spec_new_missing_project() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "spec-new-1",
            "spec.new",
            serde_json::json!({
                "project_name": "nonexistent",
                "spec_name": "test-spec",
            }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(
            msg.contains("NOT_FOUND"),
            "should contain NOT_FOUND, got: {}",
            msg
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // spec.list
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spec_list_empty() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let _project_id = insert_test_project(&db_path, "spec-list-project");

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "spec-list-1",
            "spec.list",
            serde_json::json!({ "project_name": "spec-list-project" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        let specs = resp.data["specs"].as_array().unwrap();
        assert!(specs.is_empty(), "spec list should be empty");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_spec_list_with_specs() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "spec-list-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "spec-a",
            nflow_core::spec::SpecStatus::Draft,
        );
        insert_test_spec(
            &db_path,
            project_id,
            "spec-b",
            nflow_core::spec::SpecStatus::Approved,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "spec-list-2",
            "spec.list",
            serde_json::json!({ "project_name": "spec-list-proj" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        let specs = resp.data["specs"].as_array().unwrap();
        assert_eq!(specs.len(), 2);

        let names: Vec<&str> = specs.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"spec-a"));
        assert!(names.contains(&"spec-b"));

        // Verify statuses
        let spec_a = specs.iter().find(|s| s["name"] == "spec-a").unwrap();
        assert_eq!(spec_a["status"], "draft");
        let spec_b = specs.iter().find(|s| s["name"] == "spec-b").unwrap();
        assert_eq!(spec_b["status"], "approved");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_spec_list_hides_deleted_specs() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "spec-del-list");
        insert_test_spec(
            &db_path,
            project_id,
            "visible-spec",
            nflow_core::spec::SpecStatus::Draft,
        );
        insert_test_spec(
            &db_path,
            project_id,
            "deleted-spec",
            nflow_core::spec::SpecStatus::Deleted,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "spec-list-del",
            "spec.list",
            serde_json::json!({ "project_name": "spec-del-list" }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        let specs = resp.data["specs"].as_array().unwrap();
        assert_eq!(specs.len(), 1, "deleted spec should be hidden");
        assert_eq!(specs[0]["name"], "visible-spec");

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // spec.approve + approve on already approved -> INVALID_STATE
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spec_approve_success() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "approve-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "to-approve",
            nflow_core::spec::SpecStatus::Draft,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "approve-1",
            "spec.approve",
            serde_json::json!({
                "project_name": "approve-proj",
                "spec_name": "to-approve",
            }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["name"], "to-approve");
        assert_eq!(resp.data["status"], "approved");

        // Verify in DB
        let conn = open_connection(&db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "to-approve")
            .unwrap()
            .unwrap();
        assert_eq!(spec.status, nflow_core::spec::SpecStatus::Approved);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_spec_approve_on_approved_fails() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "approve-fail-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "already-approved",
            nflow_core::spec::SpecStatus::Approved,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "approve-fail",
            "spec.approve",
            serde_json::json!({
                "project_name": "approve-fail-proj",
                "spec_name": "already-approved",
            }),
        )
        .await;

        assert_eq!(resp.id, "approve-fail");
        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(
            msg.contains("INVALID_STATE"),
            "should contain INVALID_STATE, got: {}",
            msg
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // spec.delete on decomposed -> error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spec_delete_decomposed_fails() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "delete-decomp-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "decomposed-spec",
            nflow_core::spec::SpecStatus::Decomposed,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        let resp = send_request(
            &mut writer,
            &mut lines,
            "del-decomp",
            "spec.delete",
            serde_json::json!({
                "project_name": "delete-decomp-proj",
                "spec_name": "decomposed-spec",
            }),
        )
        .await;

        assert_eq!(resp.id, "del-decomp");
        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(
            msg.contains("INVALID_STATE"),
            "should contain INVALID_STATE, got: {}",
            msg
        );

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_spec_delete_draft_success() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "del-draft-proj");
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
            "del-draft",
            "spec.delete",
            serde_json::json!({
                "project_name": "del-draft-proj",
                "spec_name": "draft-spec",
            }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["name"], "draft-spec");
        assert_eq!(resp.data["deleted"], true);

        // Verify spec is soft-deleted (still in DB but status is "deleted")
        let conn = open_connection(&db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "draft-spec")
            .unwrap()
            .unwrap();
        assert_eq!(spec.status, nflow_core::spec::SpecStatus::Deleted);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    #[tokio::test]
    async fn test_spec_delete_approved_requires_force() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        let project_id = insert_test_project(&db_path, "del-approved-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "approved-spec",
            nflow_core::spec::SpecStatus::Approved,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // Without force -> error
        let resp = send_request(
            &mut writer,
            &mut lines,
            "del-no-force",
            "spec.delete",
            serde_json::json!({
                "project_name": "del-approved-proj",
                "spec_name": "approved-spec",
            }),
        )
        .await;

        assert_eq!(resp.status, ResponseStatus::Error);
        let msg = resp.data["message"].as_str().unwrap();
        assert!(msg.contains("INVALID_STATE"), "got: {}", msg);

        // With force -> success
        let resp2 = send_request(
            &mut writer,
            &mut lines,
            "del-force",
            "spec.delete",
            serde_json::json!({
                "project_name": "del-approved-proj",
                "spec_name": "approved-spec",
                "force": true,
            }),
        )
        .await;

        assert_eq!(resp2.status, ResponseStatus::Ok);
        assert_eq!(resp2.data["deleted"], true);

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // Full flow: project init -> spec create(via DB) -> list -> approve -> list
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spec_full_lifecycle_via_commands() {
        let (_tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();

        // Insert project and draft spec directly (spec.new requires Claude)
        let project_id = insert_test_project(&db_path, "lifecycle-proj");
        insert_test_spec(
            &db_path,
            project_id,
            "my-spec",
            nflow_core::spec::SpecStatus::Draft,
        );

        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // 1. List specs — should show draft spec
        let resp = send_request(
            &mut writer,
            &mut lines,
            "lc-list-1",
            "spec.list",
            serde_json::json!({ "project_name": "lifecycle-proj" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        let specs = resp.data["specs"].as_array().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0]["status"], "draft");

        // 2. Approve the spec
        let resp = send_request(
            &mut writer,
            &mut lines,
            "lc-approve",
            "spec.approve",
            serde_json::json!({
                "project_name": "lifecycle-proj",
                "spec_name": "my-spec",
            }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.data["status"], "approved");

        // 3. List again — status should be approved
        let resp = send_request(
            &mut writer,
            &mut lines,
            "lc-list-2",
            "spec.list",
            serde_json::json!({ "project_name": "lifecycle-proj" }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok);
        let specs = resp.data["specs"].as_array().unwrap();
        assert_eq!(specs[0]["status"], "approved");

        // 4. Approve again should fail (already approved)
        let resp = send_request(
            &mut writer,
            &mut lines,
            "lc-re-approve",
            "spec.approve",
            serde_json::json!({
                "project_name": "lifecycle-proj",
                "spec_name": "my-spec",
            }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Error);
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));

        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cleanup(&socket_path);
    }
}
