//! End-to-end tests for the full happy-path workflow.
//!
//! These tests exercise the complete nflow lifecycle:
//! project init → spec creation → spec approval → plan generation →
//! plan approval → execution → story completion → status verification.
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
}
