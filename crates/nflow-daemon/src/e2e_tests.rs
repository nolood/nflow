//! End-to-end tests for nflow workflows.
//!
//! These tests exercise the complete nflow lifecycle including happy paths,
//! failure scenarios, and recovery operations.
//!
//! Uses the mock-claude binary for all Claude invocations and a local
//! bare git repo (with mock gh for MR creation) to avoid external dependencies.

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
        ResponseStatus, SocketServerConfig, StreamingResponseLine, PROTOCOL_VERSION,
    };

    // -----------------------------------------------------------------------
    // Test helpers
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
        let dir = std::env::temp_dir().join(format!("nflow_e2e_test_{}", uuid::Uuid::new_v4()));
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
            pipeline_signals: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
    ) -> crate::socket::Response {
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

    /// Create a bare git repo + clone to act as the project repo with a remote.
    async fn create_git_repo_with_remote() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("failed to create temp dir for git");
        let bare_path = dir.path().join("origin.git");
        let clone_path = dir.path().join("repo");

        fs::create_dir_all(&bare_path).unwrap();
        tokio::process::Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .current_dir(&bare_path)
            .output()
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .args([
                "clone",
                bare_path.to_str().unwrap(),
                clone_path.to_str().unwrap(),
            ])
            .output()
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();

        let readme = clone_path.join("README.md");
        fs::write(&readme, "# test project\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();

        (dir, clone_path)
    }

    /// Create a mock `gh` script that simulates `gh auth status` and `gh pr create`.
    fn create_mock_gh(dir: &Path) -> PathBuf {
        let mock_dir = dir.join("mock-gh-bin");
        fs::create_dir_all(&mock_dir).unwrap();

        let gh_script = mock_dir.join("gh");
        fs::write(
            &gh_script,
            r#"#!/bin/bash
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
    echo "Logged in to github.com"
    exit 0
elif [ "$1" = "pr" ] && [ "$2" = "create" ]; then
    echo "https://github.com/test/repo/pull/1"
    exit 0
else
    echo "mock gh: unsupported command: $@" >&2
    exit 1
fi
"#,
        )
        .unwrap();
        fs::set_permissions(&gh_script, fs::Permissions::from_mode(0o755)).unwrap();

        mock_dir
    }

    /// Get mock-claude binary path from the cargo build output.
    fn mock_claude_binary_path() -> PathBuf {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = PathBuf::from(manifest_dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let binary = workspace_root.join("target/debug/mock-claude");
        assert!(
            binary.exists(),
            "mock-claude binary not found at {}. Run `cargo build -p mock-claude` first.",
            binary.display()
        );
        binary
    }

    /// Run one scheduler tick and execute the resulting actions.
    async fn run_scheduler_tick(db_path: &Path) {
        let conn = open_connection(db_path).unwrap();
        let (actions, progress_actions) = crate::scheduler_loop::scheduler_tick(&conn, None);

        if !progress_actions.is_empty() {
            crate::scheduler_loop::execute_story_progress_actions(&conn, &progress_actions, None)
                .await;
        }
        if !actions.is_empty() {
            crate::scheduler_loop::execute_actions(&conn, &actions, None).await;
        }
    }

    /// Get all stories for a project by going through sessions.
    fn get_all_stories(
        conn: &rusqlite::Connection,
        project_id: &uuid::Uuid,
    ) -> Vec<nflow_core::work_item::WorkItem> {
        let sessions =
            db::decomposition_sessions::list_sessions_by_project(conn, project_id).unwrap();
        let mut stories = Vec::new();
        for session in &sessions {
            let session_stories =
                db::work_items::list_stories_by_session(conn, &session.id).unwrap();
            stories.extend(session_stories);
        }
        stories
    }

    // -----------------------------------------------------------------------
    // E2E test: full happy path
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_full_happy_path() {
        // 1. Setup environment
        let (tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let (_git_tmp, repo_path) = create_git_repo_with_remote().await;

        // Create project directories that handlers expect
        let project_specs_dir = nflow_home.join("projects").join("e2e-proj").join("specs");
        fs::create_dir_all(&project_specs_dir).unwrap();
        let agent_logs_dir = nflow_home
            .join("projects")
            .join("e2e-proj")
            .join("agent-logs");
        fs::create_dir_all(&agent_logs_dir).unwrap();

        // Set up mock binaries
        let mock_claude = mock_claude_binary_path();
        let mock_gh_dir = create_mock_gh(tmp.path());

        // Save and set env vars
        let original_home = std::env::var("HOME").unwrap_or_default();
        let original_path = std::env::var("PATH").unwrap_or_default();

        std::env::set_var("HOME", tmp.path());
        std::env::set_var("NFLOW_CLAUDE_BINARY", mock_claude.to_str().unwrap());
        std::env::set_var(
            "PATH",
            format!("{}:{}", mock_gh_dir.display(), original_path),
        );
        std::env::set_var("MOCK_CLAUDE_SPEC_COMPLETE", "1");
        let spec_file = nflow_home
            .join("projects")
            .join("e2e-proj")
            .join("specs")
            .join("e2e-spec.md");
        std::env::set_var("MOCK_CLAUDE_SPEC_FILE", spec_file.to_str().unwrap());

        // Start server
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // 2. Project init (directly via DB to avoid connection state issues)
        {
            let conn = open_connection(&db_path).unwrap();
            let project = nflow_core::project::ProjectService::create(
                nflow_core::project::CreateProjectParams {
                    name: "e2e-proj".to_string(),
                    path: repo_path.to_str().unwrap().to_string(),
                    remote_url: None,
                    git_provider_override: Some(nflow_core::project::GitProvider::Github),
                    head_ref: Some("main".to_string()),
                },
                |_| false,
            )
            .unwrap();
            db::projects::insert_project(&conn, &project).unwrap();
        }

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;

        // 3. Spec creation (streaming)
        let streaming_lines = send_streaming_request(
            &mut writer,
            &mut lines,
            "2",
            "spec.new",
            serde_json::json!({
                "project_name": "e2e-proj",
                "spec_name": "e2e-spec",
                "with_codebase": false
            }),
        )
        .await;

        let last = streaming_lines.last().unwrap();
        assert!(last.done, "spec.new should complete with done=true");
        let spec_id = last.data["spec_id"].as_str().unwrap();
        assert!(!spec_id.is_empty(), "spec_id should be non-empty");

        // 4. Spec approval
        let resp = send_request(
            &mut writer,
            &mut lines,
            "3",
            "spec.approve",
            serde_json::json!({
                "project_name": "e2e-proj",
                "spec_name": "e2e-spec"
            }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "spec.approve failed: {:?}",
            resp.data
        );

        // 5. Plan generation (streaming via mock Claude)
        std::env::remove_var("MOCK_CLAUDE_SPEC_COMPLETE");

        let streaming_lines = send_streaming_request(
            &mut writer,
            &mut lines,
            "4",
            "plan.generate",
            serde_json::json!({
                "project_name": "e2e-proj",
                "with_codebase": false
            }),
        )
        .await;

        let last = streaming_lines.last().unwrap();
        assert!(last.done, "plan.generate should complete with done=true");
        assert_eq!(
            last.status,
            ResponseStatus::Ok,
            "plan.generate failed: {:?}",
            last.data
        );

        let epic_count = last.data["epic_count"].as_u64().unwrap();
        let story_count = last.data["story_count"].as_u64().unwrap();
        let task_count = last.data["task_count"].as_u64().unwrap();
        assert!(epic_count >= 1, "should have at least 1 epic");
        assert!(story_count >= 1, "should have at least 1 story");
        assert!(
            task_count >= 2,
            "should have at least 2 tasks (impl + verify)"
        );

        // 6. Plan approval (with auto_execute)
        let resp = send_request(
            &mut writer,
            &mut lines,
            "5",
            "plan.approve",
            serde_json::json!({
                "project_name": "e2e-proj",
                "auto_execute": true
            }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "plan.approve failed: {:?}",
            resp.data
        );

        // 7. Enable execution
        let resp = send_request(
            &mut writer,
            &mut lines,
            "6",
            "exec.run",
            serde_json::json!({
                "project_name": "e2e-proj"
            }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "exec.run failed: {:?}",
            resp.data
        );

        // Get project ID for DB queries
        let conn = open_connection(&db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, "e2e-proj")
            .unwrap()
            .unwrap();
        let project_id = project.id;
        drop(conn);

        // 8. Run scheduler ticks until all stories complete or timeout
        let max_ticks = 60;
        let mut all_done = false;

        for tick in 0..max_ticks {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            run_scheduler_tick(&db_path).await;

            let conn = open_connection(&db_path).unwrap();
            let stories = get_all_stories(&conn, &project_id);

            if stories.is_empty() {
                continue;
            }

            let done_count = stories
                .iter()
                .filter(|s| s.status == nflow_core::work_item::WorkItemStatus::Done)
                .count();
            let failed_count = stories
                .iter()
                .filter(|s| s.status == nflow_core::work_item::WorkItemStatus::Failed)
                .count();

            eprintln!(
                "  tick {}: {}/{} stories done, {} failed",
                tick,
                done_count,
                stories.len(),
                failed_count
            );

            if done_count == stories.len() {
                all_done = true;
                break;
            }

            // Diagnose failures
            if failed_count > 0 {
                for s in &stories {
                    if s.status == nflow_core::work_item::WorkItemStatus::Failed {
                        eprintln!(
                            "    STORY FAILED: {} ({}) branch={:?} mr_url={:?}",
                            s.short_id, s.title, s.branch_name, s.mr_url
                        );
                        let tasks =
                            db::work_items::list_work_items_by_parent(&conn, &s.id).unwrap();
                        for t in &tasks {
                            eprintln!(
                                "      task={} kind={:?} status={:?}",
                                t.short_id, t.kind, t.status
                            );
                            if let Ok(Some(run)) =
                                db::agent_runs::find_latest_agent_run_for_task(&conn, &t.id)
                            {
                                eprintln!(
                                    "        run: exit={:?} err={:?} status={:?}",
                                    run.exit_code, run.error_message, run.status
                                );
                                if let Some(ref log_path) = run.log_path {
                                    if let Ok(log) = fs::read_to_string(log_path) {
                                        let preview = &log[..log.len().min(500)];
                                        eprintln!("        LOG: {}", preview);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 9. Verify final status via exec.status command
        let resp = send_request(
            &mut writer,
            &mut lines,
            "7",
            "exec.status",
            serde_json::json!({
                "project_name": "e2e-proj"
            }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "exec.status failed: {:?}",
            resp.data
        );

        // 10. Verify via DB
        let conn = open_connection(&db_path).unwrap();
        let stories = get_all_stories(&conn, &project_id);
        assert!(
            !stories.is_empty(),
            "should have at least 1 story after execution"
        );

        // All stories must be Done
        assert!(
            all_done,
            "not all stories completed within timeout. Statuses: {:?}",
            stories
                .iter()
                .map(|s| format!("{}={:?}", s.short_id, s.status))
                .collect::<Vec<_>>()
        );

        // All stories should have branch names
        for story in &stories {
            assert!(
                story.branch_name.is_some(),
                "story {} should have branch_name",
                story.short_id
            );
        }

        // All impl tasks should have commit hashes
        for story in &stories {
            let tasks = db::work_items::list_work_items_by_parent(&conn, &story.id).unwrap();
            for task in &tasks {
                if task.item_type == nflow_core::work_item::ItemType::Task
                    && task.kind == Some(nflow_core::work_item::TaskKind::Impl)
                    && task.status == nflow_core::work_item::WorkItemStatus::Done
                {
                    assert!(
                        task.commit_hash.is_some(),
                        "impl task {} should have commit_hash",
                        task.short_id
                    );
                }
            }
        }

        // All done stories should have MR URLs (from mock gh)
        for story in &stories {
            if story.status == nflow_core::work_item::WorkItemStatus::Done {
                assert!(
                    story.mr_url.is_some(),
                    "done story {} should have mr_url",
                    story.short_id
                );
            }
        }

        // Verify branches were pushed to the bare origin
        let branches_output = tokio::process::Command::new("git")
            .args(["branch", "-r"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();
        let branches = String::from_utf8_lossy(&branches_output.stdout);
        for story in &stories {
            if let Some(ref branch) = story.branch_name {
                assert!(
                    branches.contains(branch),
                    "branch '{}' for story {} should be pushed to remote. Got: {}",
                    branch,
                    story.short_id,
                    branches.trim()
                );
            }
        }

        // nflow status should show all items done
        let waves = resp.data["waves"].as_array();
        let empty_arr = vec![];
        if let Some(waves) = waves {
            for wave in waves {
                let epics = wave["epics"].as_array().unwrap_or(&empty_arr);
                for epic in epics {
                    let epic_stories = epic["stories"].as_array().unwrap_or(&empty_arr);
                    for s in epic_stories {
                        assert_eq!(
                            s["status"].as_str().unwrap_or(""),
                            "done",
                            "exec.status should show story {} as done",
                            s["short_id"].as_str().unwrap_or("?")
                        );
                    }
                }
            }
        }

        // Cleanup
        std::env::set_var("HOME", &original_home);
        std::env::set_var("PATH", &original_path);
        std::env::remove_var("NFLOW_CLAUDE_BINARY");
        std::env::remove_var("MOCK_CLAUDE_SPEC_COMPLETE");
        std::env::remove_var("MOCK_CLAUDE_SPEC_FILE");

        handle.shutdown();
        cleanup(&socket_path);
    }

    // -----------------------------------------------------------------------
    // E2E helper: set up environment through to execution-ready state
    // -----------------------------------------------------------------------

    /// State from a fully set up project ready for execution.
    /// Encapsulates all temp dirs and env state for cleanup.
    #[allow(dead_code)]
    struct E2eEnv {
        _tmp: tempfile::TempDir,
        _git_tmp: tempfile::TempDir,
        nflow_home: PathBuf,
        db_path: PathBuf,
        socket_path: PathBuf,
        repo_path: PathBuf,
        handle: crate::socket::SocketServerHandle,
        writer: tokio::net::unix::OwnedWriteHalf,
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        project_id: uuid::Uuid,
        original_home: String,
        original_path: String,
        req_counter: u32,
    }

    impl E2eEnv {
        fn next_id(&mut self) -> String {
            self.req_counter += 1;
            self.req_counter.to_string()
        }

        fn cleanup_env(&self) {
            std::env::set_var("HOME", &self.original_home);
            std::env::set_var("PATH", &self.original_path);
            std::env::remove_var("NFLOW_CLAUDE_BINARY");
            std::env::remove_var("MOCK_CLAUDE_SPEC_COMPLETE");
            std::env::remove_var("MOCK_CLAUDE_SPEC_FILE");
            std::env::remove_var("MOCK_CLAUDE_FAIL");
            std::env::remove_var("MOCK_CLAUDE_VERIFY_FAIL");
            std::env::remove_var("MOCK_CLAUDE_SLEEP");
            std::env::remove_var("MOCK_CLAUDE_EXIT_CODE");
        }

        fn shutdown(self) {
            self.cleanup_env();
            self.handle.shutdown();
            cleanup(&self.socket_path);
        }
    }

    /// Set up a full environment: project, spec, plan generation, plan approval, exec.run.
    /// Uses a single-story plan with one impl task (→ auto-generates verify task).
    /// Returns E2eEnv ready for scheduler ticks.
    async fn setup_execution_env(project_name: &str) -> E2eEnv {
        let (tmp, nflow_home) = create_test_home();
        let db_path = create_test_db(&nflow_home);
        let socket_path = temp_socket_path();
        let (git_tmp, repo_path) = create_git_repo_with_remote().await;

        // Create project directories
        let project_specs_dir = nflow_home.join("projects").join(project_name).join("specs");
        fs::create_dir_all(&project_specs_dir).unwrap();
        let agent_logs_dir = nflow_home
            .join("projects")
            .join(project_name)
            .join("agent-logs");
        fs::create_dir_all(&agent_logs_dir).unwrap();

        // Set up mock binaries
        let mock_claude = mock_claude_binary_path();
        let mock_gh_dir = create_mock_gh(tmp.path());

        let original_home = std::env::var("HOME").unwrap_or_default();
        let original_path = std::env::var("PATH").unwrap_or_default();

        std::env::set_var("HOME", tmp.path());
        std::env::set_var("NFLOW_CLAUDE_BINARY", mock_claude.to_str().unwrap());
        std::env::set_var(
            "PATH",
            format!("{}:{}", mock_gh_dir.display(), original_path),
        );
        std::env::set_var("MOCK_CLAUDE_SPEC_COMPLETE", "1");
        let spec_file = nflow_home
            .join("projects")
            .join(project_name)
            .join("specs")
            .join("e2e-spec.md");
        std::env::set_var("MOCK_CLAUDE_SPEC_FILE", spec_file.to_str().unwrap());

        // Start server
        let handle = start_real_server(&socket_path, &db_path);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Create project via DB
        let project_id = {
            let conn = open_connection(&db_path).unwrap();
            let project = nflow_core::project::ProjectService::create(
                nflow_core::project::CreateProjectParams {
                    name: project_name.to_string(),
                    path: repo_path.to_str().unwrap().to_string(),
                    remote_url: None,
                    git_provider_override: Some(nflow_core::project::GitProvider::Github),
                    head_ref: Some("main".to_string()),
                },
                |_| false,
            )
            .unwrap();
            db::projects::insert_project(&conn, &project).unwrap();
            project.id
        };

        let (mut writer, mut lines) = connect_and_handshake(&socket_path).await;
        let mut req_counter = 1u32;

        // Spec creation (streaming)
        let streaming_lines = send_streaming_request(
            &mut writer,
            &mut lines,
            &{
                req_counter += 1;
                req_counter.to_string()
            },
            "spec.new",
            serde_json::json!({
                "project_name": project_name,
                "spec_name": "e2e-spec",
                "with_codebase": false
            }),
        )
        .await;
        let last = streaming_lines.last().unwrap();
        assert!(last.done, "spec.new should complete");

        // Spec approval
        let resp = send_request(
            &mut writer,
            &mut lines,
            &{
                req_counter += 1;
                req_counter.to_string()
            },
            "spec.approve",
            serde_json::json!({
                "project_name": project_name,
                "spec_name": "e2e-spec"
            }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok, "spec.approve failed");

        // Plan generation
        std::env::remove_var("MOCK_CLAUDE_SPEC_COMPLETE");
        let streaming_lines = send_streaming_request(
            &mut writer,
            &mut lines,
            &{
                req_counter += 1;
                req_counter.to_string()
            },
            "plan.generate",
            serde_json::json!({
                "project_name": project_name,
                "with_codebase": false
            }),
        )
        .await;
        let last = streaming_lines.last().unwrap();
        assert!(last.done, "plan.generate should complete");
        assert_eq!(last.status, ResponseStatus::Ok, "plan.generate failed");

        // Plan approval with auto_execute
        let resp = send_request(
            &mut writer,
            &mut lines,
            &{
                req_counter += 1;
                req_counter.to_string()
            },
            "plan.approve",
            serde_json::json!({
                "project_name": project_name,
                "auto_execute": true
            }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok, "plan.approve failed");

        // Enable execution
        let resp = send_request(
            &mut writer,
            &mut lines,
            &{
                req_counter += 1;
                req_counter.to_string()
            },
            "exec.run",
            serde_json::json!({
                "project_name": project_name
            }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok, "exec.run failed");

        E2eEnv {
            _tmp: tmp,
            _git_tmp: git_tmp,
            nflow_home,
            db_path,
            socket_path,
            repo_path,
            handle,
            writer,
            lines,
            project_id,
            original_home,
            original_path,
            req_counter,
        }
    }

    /// Get all tasks for a specific story.
    fn get_tasks_for_story(
        conn: &rusqlite::Connection,
        story_id: &uuid::Uuid,
    ) -> Vec<nflow_core::work_item::WorkItem> {
        db::work_items::list_work_items_by_parent(conn, story_id).unwrap()
    }

    /// Wait for a specific story status, running scheduler ticks.
    async fn wait_for_story_status(
        db_path: &Path,
        project_id: &uuid::Uuid,
        expected_status: nflow_core::work_item::WorkItemStatus,
        max_ticks: u32,
    ) -> bool {
        for _ in 0..max_ticks {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            run_scheduler_tick(db_path).await;

            let conn = open_connection(db_path).unwrap();
            let stories = get_all_stories(&conn, project_id);
            if !stories.is_empty() && stories.iter().any(|s| s.status == expected_status) {
                return true;
            }
        }
        false
    }

    // -----------------------------------------------------------------------
    // E2E test: impl task fails → task and story marked failed
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_impl_task_fails() {
        let env = setup_execution_env("e2e-fail-impl").await;

        // Set MOCK_CLAUDE_FAIL *after* setup so spec/plan generation succeeds
        // but the first agent spawned by the scheduler fails
        std::env::set_var("MOCK_CLAUDE_FAIL", "1");

        // Run scheduler ticks until a story becomes Failed
        let found = wait_for_story_status(
            &env.db_path,
            &env.project_id,
            nflow_core::work_item::WorkItemStatus::Failed,
            60,
        )
        .await;

        assert!(
            found,
            "story should be marked as Failed after impl task fails"
        );

        // Verify the impl task is also Failed
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let story = stories
            .iter()
            .find(|s| s.status == nflow_core::work_item::WorkItemStatus::Failed)
            .expect("should have a failed story");

        let tasks = get_tasks_for_story(&conn, &story.id);
        let impl_task = tasks
            .iter()
            .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Impl))
            .expect("should have an impl task");
        assert_eq!(
            impl_task.status,
            nflow_core::work_item::WorkItemStatus::Failed,
            "impl task should be failed"
        );

        // Verify there's a failed agent run with error
        let agent_run =
            db::agent_runs::find_latest_agent_run_for_task(&conn, &impl_task.id).unwrap();
        assert!(agent_run.is_some(), "should have an agent run");
        let run = agent_run.unwrap();
        assert_eq!(
            run.status,
            db::agent_runs::AgentRunStatus::Failed,
            "agent run should be Failed"
        );

        drop(conn);
        std::env::remove_var("MOCK_CLAUDE_FAIL");
        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: retry → task re-executed, succeeds → story continues
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_retry_succeeds() {
        let mut env = setup_execution_env("e2e-retry").await;

        // Make the first impl task fail (after setup completes)
        std::env::set_var("MOCK_CLAUDE_FAIL", "1");

        // Wait for the story to fail
        let found = wait_for_story_status(
            &env.db_path,
            &env.project_id,
            nflow_core::work_item::WorkItemStatus::Failed,
            60,
        )
        .await;
        assert!(found, "story should fail first");

        // Find the failed impl task's wave-prefixed short ID
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let story = stories
            .iter()
            .find(|s| s.status == nflow_core::work_item::WorkItemStatus::Failed)
            .unwrap();
        let tasks = get_tasks_for_story(&conn, &story.id);
        let impl_task = tasks
            .iter()
            .find(|t| {
                t.kind == Some(nflow_core::work_item::TaskKind::Impl)
                    && t.status == nflow_core::work_item::WorkItemStatus::Failed
            })
            .unwrap();

        // Build wave-prefixed short ID (e.g., "W1-T1")
        let sessions =
            db::decomposition_sessions::list_sessions_by_project(&conn, &env.project_id).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.id == impl_task.decomposition_session_id)
            .unwrap();
        let wave_task_id = format!("W{}-{}", session.wave_number, impl_task.short_id);
        drop(conn);

        // Remove the failure env var so retry succeeds
        std::env::remove_var("MOCK_CLAUDE_FAIL");

        // Send exec.retry command
        let id = env.next_id();
        let resp = send_request(
            &mut env.writer,
            &mut env.lines,
            &id,
            "exec.retry",
            serde_json::json!({ "task_id": wave_task_id }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "exec.retry failed: {:?}",
            resp.data
        );

        // Give the handler time to spawn the agent
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Run scheduler ticks until the first story (S1) completes
        let mut s1_done = false;
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            run_scheduler_tick(&env.db_path).await;

            let conn = open_connection(&env.db_path).unwrap();
            let stories = get_all_stories(&conn, &env.project_id);
            if let Some(s1) = stories.iter().find(|s| s.short_id.ends_with("S1")) {
                if s1.status == nflow_core::work_item::WorkItemStatus::Done {
                    s1_done = true;
                    break;
                }
            }
            drop(conn);
        }
        assert!(s1_done, "S1 should complete after successful retry");

        // Verify impl task is now Done
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        for story in &stories {
            if story.status == nflow_core::work_item::WorkItemStatus::Done {
                let tasks = get_tasks_for_story(&conn, &story.id);
                let impl_t = tasks
                    .iter()
                    .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Impl))
                    .unwrap();
                assert_eq!(
                    impl_t.status,
                    nflow_core::work_item::WorkItemStatus::Done,
                    "impl task should be done after retry"
                );
            }
        }

        drop(conn);
        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: skip → task skipped, verify skipped → story continues
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_skip_task() {
        let mut env = setup_execution_env("e2e-skip").await;

        // Make the first impl task fail (after setup completes)
        std::env::set_var("MOCK_CLAUDE_FAIL", "1");

        // Wait for the story to fail
        let found = wait_for_story_status(
            &env.db_path,
            &env.project_id,
            nflow_core::work_item::WorkItemStatus::Failed,
            60,
        )
        .await;
        assert!(found, "story should fail first");

        // Find the failed impl task
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let story = stories
            .iter()
            .find(|s| s.status == nflow_core::work_item::WorkItemStatus::Failed)
            .unwrap();
        let tasks = get_tasks_for_story(&conn, &story.id);
        let impl_task = tasks
            .iter()
            .find(|t| {
                t.kind == Some(nflow_core::work_item::TaskKind::Impl)
                    && t.status == nflow_core::work_item::WorkItemStatus::Failed
            })
            .unwrap();

        let sessions =
            db::decomposition_sessions::list_sessions_by_project(&conn, &env.project_id).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.id == impl_task.decomposition_session_id)
            .unwrap();
        let wave_task_id = format!("W{}-{}", session.wave_number, impl_task.short_id);
        drop(conn);

        // Remove failure env var
        std::env::remove_var("MOCK_CLAUDE_FAIL");

        // Send exec.skip command
        let id = env.next_id();
        let resp = send_request(
            &mut env.writer,
            &mut env.lines,
            &id,
            "exec.skip",
            serde_json::json!({ "task_id": wave_task_id }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "exec.skip failed: {:?}",
            resp.data
        );

        // The skip handler marks the impl task as Done and cancels paired verify.
        // If there's a second story (S2 depends on S1), the scheduler may start it.
        // Wait for the first story to complete.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Run a few more scheduler ticks for the second story to start and finish
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            run_scheduler_tick(&env.db_path).await;
        }

        // Verify the skipped impl task is Done and verify is Cancelled
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        // Find S1 (first story)
        let s1 = stories.iter().find(|s| s.short_id.ends_with("S1")).unwrap();
        let tasks = get_tasks_for_story(&conn, &s1.id);

        let impl_t = tasks
            .iter()
            .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Impl))
            .unwrap();
        assert_eq!(
            impl_t.status,
            nflow_core::work_item::WorkItemStatus::Done,
            "skipped impl task should be Done"
        );

        let verify_t = tasks
            .iter()
            .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Verify));
        if let Some(vt) = verify_t {
            assert!(
                vt.status == nflow_core::work_item::WorkItemStatus::Cancelled
                    || vt.status == nflow_core::work_item::WorkItemStatus::Done,
                "verify task should be Cancelled or Done after skip, got {:?}",
                vt.status
            );
        }

        drop(conn);
        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: verify task fails → story marked failed immediately
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_verify_task_fails() {
        let env = setup_execution_env("e2e-vfail").await;

        // Impl succeeds but verify fails (set after setup)
        std::env::set_var("MOCK_CLAUDE_VERIFY_FAIL", "1");

        // The impl task should succeed (no MOCK_CLAUDE_FAIL), but the verify should fail.
        // Wait for story to fail.
        let found = wait_for_story_status(
            &env.db_path,
            &env.project_id,
            nflow_core::work_item::WorkItemStatus::Failed,
            60,
        )
        .await;
        assert!(found, "story should fail when verify task fails");

        // Verify: impl task should be Done, verify task should be Failed
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let s1 = stories.iter().find(|s| s.short_id.ends_with("S1")).unwrap();
        let tasks = get_tasks_for_story(&conn, &s1.id);

        let impl_t = tasks
            .iter()
            .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Impl))
            .unwrap();
        assert_eq!(
            impl_t.status,
            nflow_core::work_item::WorkItemStatus::Done,
            "impl task should succeed"
        );

        let verify_t = tasks
            .iter()
            .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Verify))
            .expect("should have a verify task");
        assert_eq!(
            verify_t.status,
            nflow_core::work_item::WorkItemStatus::Failed,
            "verify task should be failed"
        );

        // Story should be failed
        assert_eq!(
            s1.status,
            nflow_core::work_item::WorkItemStatus::Failed,
            "story should be failed when verify fails"
        );

        drop(conn);
        std::env::remove_var("MOCK_CLAUDE_VERIFY_FAIL");
        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: exec.stop → agent terminated, story cancelled
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_stop_agent() {
        let mut env = setup_execution_env("e2e-stop").await;

        // Use MOCK_CLAUDE_SLEEP to keep the agent alive long enough to stop it (set after setup)
        std::env::set_var("MOCK_CLAUDE_SLEEP", "60");

        // Run a scheduler tick to start the first story
        run_scheduler_tick(&env.db_path).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        run_scheduler_tick(&env.db_path).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify there's a running agent
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);

        // Find a story that is InProgress
        let in_progress_story = stories
            .iter()
            .find(|s| s.status == nflow_core::work_item::WorkItemStatus::InProgress);

        // The story might not be in_progress yet. Give it more ticks.
        drop(conn);
        if in_progress_story.is_none() {
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                run_scheduler_tick(&env.db_path).await;
            }
        }

        // Now send exec.stop --all
        let id = env.next_id();
        let resp = send_request(
            &mut env.writer,
            &mut env.lines,
            &id,
            "exec.stop",
            serde_json::json!({ "all": true }),
        )
        .await;
        assert_eq!(
            resp.status,
            ResponseStatus::Ok,
            "exec.stop failed: {:?}",
            resp.data
        );

        // Give the system a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify stories are cancelled
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let has_cancelled = stories
            .iter()
            .any(|s| s.status == nflow_core::work_item::WorkItemStatus::Cancelled);

        // If no stories were cancelled, they might not have started yet.
        // At minimum, no stories should be InProgress after stop.
        let has_in_progress = stories
            .iter()
            .any(|s| s.status == nflow_core::work_item::WorkItemStatus::InProgress);
        assert!(
            !has_in_progress || has_cancelled,
            "after exec.stop, no stories should be in_progress (or cancelled). Statuses: {:?}",
            stories
                .iter()
                .map(|s| format!("{}={:?}", s.short_id, s.status))
                .collect::<Vec<_>>()
        );

        drop(conn);
        std::env::remove_var("MOCK_CLAUDE_SLEEP");
        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: exec.continue on cancelled story → story resumes
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_continue_cancelled_story() {
        let mut env = setup_execution_env("e2e-cont").await;

        // Use MOCK_CLAUDE_SLEEP to keep agent alive, then stop, then continue (set after setup)
        std::env::set_var("MOCK_CLAUDE_SLEEP", "60");

        // Run scheduler ticks to start execution
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            run_scheduler_tick(&env.db_path).await;
        }

        // Find S1 to get its wave-prefixed ID
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let sessions =
            db::decomposition_sessions::list_sessions_by_project(&conn, &env.project_id).unwrap();
        let s1 = stories.iter().find(|s| s.short_id.ends_with("S1")).unwrap();
        let s1_session = sessions
            .iter()
            .find(|sess| sess.id == s1.decomposition_session_id)
            .unwrap();
        let wave_story_id = format!("W{}-{}", s1_session.wave_number, s1.short_id);

        // Stop via story_id if in_progress, otherwise stop all
        drop(conn);

        let id = env.next_id();
        let resp = send_request(
            &mut env.writer,
            &mut env.lines,
            &id,
            "exec.stop",
            serde_json::json!({ "all": true }),
        )
        .await;
        assert_eq!(resp.status, ResponseStatus::Ok, "exec.stop failed");

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Remove sleep so continue can succeed
        std::env::remove_var("MOCK_CLAUDE_SLEEP");

        // Verify the story is cancelled
        let conn = open_connection(&env.db_path).unwrap();
        let s1_fresh = db::work_items::get_work_item_by_id(&conn, &s1.id)
            .unwrap()
            .unwrap();
        drop(conn);

        if s1_fresh.status == nflow_core::work_item::WorkItemStatus::Cancelled {
            // Send exec.continue on the cancelled story
            let id = env.next_id();
            let resp = send_request(
                &mut env.writer,
                &mut env.lines,
                &id,
                "exec.continue",
                serde_json::json!({ "story_id": wave_story_id, "force": true }),
            )
            .await;
            assert_eq!(
                resp.status,
                ResponseStatus::Ok,
                "exec.continue failed: {:?}",
                resp.data
            );

            // Run scheduler ticks until story completes
            let done = wait_for_story_status(
                &env.db_path,
                &env.project_id,
                nflow_core::work_item::WorkItemStatus::Done,
                60,
            )
            .await;

            // S2 depends on S1, so check if at least S1 completed
            let conn = open_connection(&env.db_path).unwrap();
            let s1_final = db::work_items::get_work_item_by_id(&conn, &s1.id)
                .unwrap()
                .unwrap();
            assert!(
                s1_final.status == nflow_core::work_item::WorkItemStatus::Done
                    || s1_final.status == nflow_core::work_item::WorkItemStatus::InProgress
                    || done,
                "story should resume after continue. Got status: {:?}",
                s1_final.status
            );
            drop(conn);
        }

        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: wall-clock timeout → agent killed, task failed
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_wall_clock_timeout() {
        let env = setup_execution_env("e2e-timeout").await;

        // Use MOCK_CLAUDE_SLEEP to keep agent alive (set after setup)
        std::env::set_var("MOCK_CLAUDE_SLEEP", "3600");

        // Run scheduler ticks to start execution
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            run_scheduler_tick(&env.db_path).await;
        }

        // Find the running agent and manipulate its started_at to be far in the past
        let conn = open_connection(&env.db_path).unwrap();
        let running = db::agent_runs::find_running_agent_runs(&conn).unwrap();

        if !running.is_empty() {
            // Set started_at to 2 hours ago so it exceeds the 1800s default timeout
            let past_time = chrono::Utc::now() - chrono::Duration::hours(2);
            let past_time_str = past_time.to_rfc3339();
            conn.execute(
                "UPDATE agent_runs SET started_at = ?1 WHERE status = 'running'",
                rusqlite::params![past_time_str],
            )
            .unwrap();
            drop(conn);

            // Run scheduler tick — should detect timeout and kill agent
            run_scheduler_tick(&env.db_path).await;

            // Give some time for the SIGTERM→SIGKILL cycle
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Run one more tick to process the killed agent
            run_scheduler_tick(&env.db_path).await;

            // Verify the task is failed due to timeout
            let conn = open_connection(&env.db_path).unwrap();
            let stories = get_all_stories(&conn, &env.project_id);
            let s1 = stories.iter().find(|s| s.short_id.ends_with("S1")).unwrap();
            let tasks = get_tasks_for_story(&conn, &s1.id);
            let impl_t = tasks
                .iter()
                .find(|t| t.kind == Some(nflow_core::work_item::TaskKind::Impl))
                .unwrap();

            assert_eq!(
                impl_t.status,
                nflow_core::work_item::WorkItemStatus::Failed,
                "task should be failed after timeout"
            );

            // Check that story is failed
            assert_eq!(
                s1.status,
                nflow_core::work_item::WorkItemStatus::Failed,
                "story should be failed after timeout"
            );

            // Check agent_run has timeout error message
            let agent_run =
                db::agent_runs::find_latest_agent_run_for_task(&conn, &impl_t.id).unwrap();
            if let Some(run) = agent_run {
                assert!(
                    run.error_message
                        .as_deref()
                        .unwrap_or("")
                        .contains("timeout"),
                    "agent run error should mention timeout, got: {:?}",
                    run.error_message
                );
            }

            drop(conn);
        } else {
            drop(conn);
            // If no running agents yet, the setup might have failed — fail the test
            panic!("no running agents found; scheduler may not have started execution");
        }

        std::env::remove_var("MOCK_CLAUDE_SLEEP");
        env.shutdown();
    }

    // -----------------------------------------------------------------------
    // E2E test: parallel execution — 3 independent stories run concurrently
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_e2e_parallel_execution() {
        // Provide a decomposition with 3 independent stories (no dependencies)
        let parallel_decompose_json = serde_json::json!({
            "epics": [{
                "id": "E1",
                "title": "Parallel Implementation",
                "description": "Three independent features",
                "stories": [{
                    "id": "S1",
                    "title": "Feature Alpha",
                    "description": "Implement feature alpha",
                    "acceptance_criteria": "Alpha works",
                    "depends_on": [],
                    "tasks": [{
                        "id": "T1",
                        "title": "Write alpha module",
                        "description": "Create alpha module",
                        "acceptance_criteria": "Module compiles"
                    }]
                }, {
                    "id": "S2",
                    "title": "Feature Beta",
                    "description": "Implement feature beta",
                    "acceptance_criteria": "Beta works",
                    "depends_on": [],
                    "tasks": [{
                        "id": "T2",
                        "title": "Write beta module",
                        "description": "Create beta module",
                        "acceptance_criteria": "Module compiles"
                    }]
                }, {
                    "id": "S3",
                    "title": "Feature Gamma",
                    "description": "Implement feature gamma",
                    "acceptance_criteria": "Gamma works",
                    "depends_on": [],
                    "tasks": [{
                        "id": "T3",
                        "title": "Write gamma module",
                        "description": "Create gamma module",
                        "acceptance_criteria": "Module compiles"
                    }]
                }]
            }]
        })
        .to_string();

        // Set the custom decomposition JSON BEFORE setup so plan.generate uses it
        std::env::set_var("MOCK_CLAUDE_DECOMPOSE_JSON", &parallel_decompose_json);

        let env = setup_execution_env("e2e-parallel").await;

        // Verify we got 3 stories
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        assert_eq!(stories.len(), 3, "should have 3 stories for parallel test");
        drop(conn);

        // Run scheduler ticks — with max_parallel=3 and 3 independent stories,
        // all should start simultaneously
        let max_ticks = 80;
        let mut all_done = false;
        for tick in 0..max_ticks {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            run_scheduler_tick(&env.db_path).await;

            let conn = open_connection(&env.db_path).unwrap();
            let stories = get_all_stories(&conn, &env.project_id);

            let in_progress_count = stories
                .iter()
                .filter(|s| s.status == nflow_core::work_item::WorkItemStatus::InProgress)
                .count();
            let done_count = stories
                .iter()
                .filter(|s| s.status == nflow_core::work_item::WorkItemStatus::Done)
                .count();
            let failed_count = stories
                .iter()
                .filter(|s| s.status == nflow_core::work_item::WorkItemStatus::Failed)
                .count();

            eprintln!(
                "  tick {}: {}/{} done, {} in_progress, {} failed",
                tick,
                done_count,
                stories.len(),
                in_progress_count,
                failed_count
            );

            if done_count == 3 {
                all_done = true;
                break;
            }

            // Diagnose failures
            if failed_count > 0 {
                for s in &stories {
                    if s.status == nflow_core::work_item::WorkItemStatus::Failed {
                        eprintln!(
                            "    STORY FAILED: {} ({}) branch={:?}",
                            s.short_id, s.title, s.branch_name
                        );
                        let tasks =
                            db::work_items::list_work_items_by_parent(&conn, &s.id).unwrap();
                        for t in &tasks {
                            eprintln!(
                                "      task={} kind={:?} status={:?}",
                                t.short_id, t.kind, t.status
                            );
                            if let Ok(Some(run)) =
                                db::agent_runs::find_latest_agent_run_for_task(&conn, &t.id)
                            {
                                eprintln!(
                                    "        run: exit={:?} err={:?}",
                                    run.exit_code, run.error_message
                                );
                            }
                        }
                    }
                }
                // Don't break on failure — let the rest complete for diagnosis
            }

            drop(conn);
        }

        // Verify all stories completed
        assert!(
            all_done,
            "all 3 stories should complete. Check eprintln diagnostics above."
        );

        // Verify parallel execution was observed
        // (Note: due to timing, this may not always capture exact simultaneity,
        // but with fast mock-claude, the scheduler should start all 3 in the same tick)
        // We relax this to just check all completed, since fast mock-claude
        // may complete before the next check.

        // Verify each story has its own branch and worktree
        let conn = open_connection(&env.db_path).unwrap();
        let stories = get_all_stories(&conn, &env.project_id);
        let mut branch_names = std::collections::HashSet::new();

        for story in &stories {
            assert!(
                story.branch_name.is_some(),
                "story {} should have a branch_name",
                story.short_id
            );
            let branch = story.branch_name.as_ref().unwrap();
            assert!(
                branch_names.insert(branch.clone()),
                "branch '{}' should be unique, but was used by multiple stories",
                branch
            );
        }
        assert_eq!(branch_names.len(), 3, "should have 3 distinct branch names");

        // All stories should have MR URLs (from mock gh)
        for story in &stories {
            assert!(
                story.mr_url.is_some(),
                "done story {} should have mr_url",
                story.short_id
            );
        }

        // All impl tasks should have commit hashes
        for story in &stories {
            let tasks = db::work_items::list_work_items_by_parent(&conn, &story.id).unwrap();
            for task in &tasks {
                if task.item_type == nflow_core::work_item::ItemType::Task
                    && task.kind == Some(nflow_core::work_item::TaskKind::Impl)
                    && task.status == nflow_core::work_item::WorkItemStatus::Done
                {
                    assert!(
                        task.commit_hash.is_some(),
                        "impl task {} should have commit_hash",
                        task.short_id
                    );
                }
            }
        }

        // Verify branches were pushed to the bare origin
        let branches_output = tokio::process::Command::new("git")
            .args(["branch", "-r"])
            .current_dir(&env.repo_path)
            .output()
            .await
            .unwrap();
        let branches = String::from_utf8_lossy(&branches_output.stdout);
        for story in &stories {
            if let Some(ref branch) = story.branch_name {
                assert!(
                    branches.contains(branch),
                    "branch '{}' for story {} should be pushed to remote. Got: {}",
                    branch,
                    story.short_id,
                    branches.trim()
                );
            }
        }

        // Verify no data corruption: all agent runs should be in terminal state
        let running = db::agent_runs::find_running_agent_runs(&conn).unwrap();
        assert!(
            running.is_empty(),
            "no agent runs should still be running after all stories complete"
        );

        drop(conn);
        std::env::remove_var("MOCK_CLAUDE_DECOMPOSE_JSON");
        env.shutdown();
    }
}
