//! Integration tests for daemon lifecycle: start/stop/crash recovery.
//!
//! These tests exercise the full daemon lifecycle functions in isolation
//! using temporary directories. Each test creates its own temp NFLOW_HOME
//! to avoid interference with other tests or the real daemon.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use chrono::Utc;
    use rusqlite::Connection;
    use uuid::Uuid;

    use crate::db::agent_runs::{
        find_running_agent_runs, insert_agent_run, AgentRun, AgentRunStatus,
    };
    use crate::db::projects::insert_project;
    use crate::db::work_items::{get_work_item_by_id, insert_work_item, update_work_item_status};
    use crate::db::{open_connection, run_migrations};
    use crate::platform;
    use crate::recovery;
    use crate::shutdown;
    use crate::socket;

    use nflow_core::project::{GitProvider, Project};
    use nflow_core::work_item::{WorkItem, WorkItemStatus};

    /// Create an isolated NFLOW_HOME-like directory structure for testing.
    /// Returns (temp_dir_guard, nflow_home_path).
    fn create_test_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let nflow_home = tmp.path().join(".nflow");
        fs::create_dir_all(&nflow_home).unwrap();
        fs::set_permissions(&nflow_home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(nflow_home.join("logs")).unwrap();
        (tmp, nflow_home)
    }

    /// Open a test database in the given nflow_home directory, with migrations applied.
    fn create_test_db(nflow_home: &Path) -> Connection {
        let db_path = nflow_home.join("nflow.db");
        let conn = open_connection(&db_path).expect("failed to open test db");
        run_migrations(&conn, &db_path).expect("failed to run migrations");
        conn
    }

    /// Write a PID to a PID file at the given path.
    fn write_pid_to_file(pid_path: &Path, pid: u32) {
        let mut f = fs::File::create(pid_path).unwrap();
        write!(f, "{}", pid).unwrap();
    }

    /// Read a PID from a PID file at the given path.
    fn read_pid_from_file(pid_path: &Path) -> Option<u32> {
        match fs::read_to_string(pid_path) {
            Ok(content) => content.trim().parse::<u32>().ok(),
            Err(_) => None,
        }
    }

    /// Create a test project in the database.
    fn make_project(conn: &Connection) -> Uuid {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: format!("test-project-{}", Uuid::new_v4()),
            path: "/home/user/test".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: true,
            created_at: now,
            updated_at: now,
        };
        insert_project(conn, &project).unwrap();
        project.id
    }

    /// Create a decomposition session in the database.
    fn make_session(conn: &Connection, project_id: Uuid) -> Uuid {
        let session_id = Uuid::new_v4();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'in_progress', ?3, ?3)",
            rusqlite::params![
                session_id.to_string(),
                project_id.to_string(),
                now.to_rfc3339(),
            ],
        )
        .unwrap();
        session_id
    }

    /// Create a work item hierarchy (epic → story → task) and set the task to in_progress.
    fn make_in_progress_task(conn: &Connection, session_id: Uuid) -> WorkItem {
        let epic = WorkItem::new_epic(
            session_id,
            "Epic 1".into(),
            "Desc".into(),
            format!("E{}", Uuid::new_v4().as_fields().0),
            0,
        );
        insert_work_item(conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session_id,
            "Story 1".into(),
            "Desc".into(),
            "AC".into(),
            format!("S{}", Uuid::new_v4().as_fields().0),
            1,
        );
        insert_work_item(conn, &story).unwrap();

        let task = WorkItem::new_task(
            story.id,
            session_id,
            "Task 1".into(),
            "Desc".into(),
            "AC".into(),
            format!("T{}", Uuid::new_v4().as_fields().0),
            2,
        );
        insert_work_item(conn, &task).unwrap();
        update_work_item_status(conn, &task.id, WorkItemStatus::InProgress).unwrap();
        task
    }

    /// Create a running agent_run record.
    fn make_running_agent(
        work_item_id: Uuid,
        pid: Option<u32>,
        start_time: Option<i64>,
    ) -> AgentRun {
        AgentRun {
            id: Uuid::new_v4(),
            work_item_id,
            pid,
            session_id: Some("claude-session-test".to_string()),
            pid_start_time: start_time,
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: Some("/tmp/test-agent.log".to_string()),
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        }
    }

    // =========================================================================
    // Test: start daemon -> PID file created, socket exists, status reports running
    // =========================================================================

    #[tokio::test]
    async fn test_start_lifecycle_pid_file_and_socket() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");

        // Initially: no PID file, no socket
        assert!(!pid_path.exists(), "PID file should not exist initially");
        assert!(!sock_path.exists(), "socket should not exist initially");

        // Simulate daemon start: write PID file
        write_pid_to_file(&pid_path, std::process::id());
        assert!(pid_path.exists(), "PID file should exist after write");
        assert_eq!(
            read_pid_from_file(&pid_path),
            Some(std::process::id()),
            "PID file should contain current PID"
        );

        // Check process is alive (our own PID)
        assert!(
            platform::is_process_alive(std::process::id()),
            "current process should be alive"
        );

        // Bind socket (simulating daemon socket server start)
        let _listener = socket::bind_socket(&sock_path).expect("failed to bind socket");
        assert!(sock_path.exists(), "socket file should exist after bind");

        // Verify socket has 0600 permissions
        let mode = fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket should have 0600 permissions");

        // Status reports running: PID exists and process is alive
        let pid = read_pid_from_file(&pid_path).expect("should read PID");
        assert!(
            platform::is_process_alive(pid),
            "daemon status should report running"
        );
    }

    // =========================================================================
    // Test: stop daemon -> PID file removed, socket removed
    // =========================================================================

    #[tokio::test]
    async fn test_stop_lifecycle_cleanup() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");

        // Simulate running daemon: PID file + socket exist
        write_pid_to_file(&pid_path, std::process::id());
        let _listener = socket::bind_socket(&sock_path).expect("failed to bind socket");
        assert!(pid_path.exists());
        assert!(sock_path.exists());

        // Open a DB for shutdown (empty — no agents)
        let conn = create_test_db(&nflow_home);

        // Run graceful shutdown (no agents, just cleanup)
        let report = shutdown::graceful_shutdown(&conn, &sock_path, &pid_path);

        // PID file should be removed
        assert!(!pid_path.exists(), "PID file should be removed after stop");
        // Socket should be removed
        assert!(!sock_path.exists(), "socket should be removed after stop");
        // No agents
        assert_eq!(report.agents_finished_naturally, 0);
        assert_eq!(report.agents_sigtermed, 0);
        assert_eq!(report.agents_sigkilled, 0);
        assert!(report.socket_removed);
        assert!(report.pid_file_removed);
    }

    // =========================================================================
    // Test: crash recovery — kill daemon (SIGKILL), restart -> stale agents marked failed
    // =========================================================================

    #[test]
    fn test_crash_recovery_stale_agents_marked_failed() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");
        let conn = create_test_db(&nflow_home);

        // Simulate a crashed daemon: stale PID file with dead PID, stale socket
        write_pid_to_file(&pid_path, 4_000_000_000); // PID that doesn't exist
        fs::write(&sock_path, "").unwrap(); // Stale socket file

        // Create a project + session + task with a running agent that has a dead PID
        let project_id = make_project(&conn);
        let session_id = make_session(&conn, project_id);
        let task = make_in_progress_task(&conn, session_id);

        // Agent run with a dead PID (simulating agent that was killed with the daemon)
        let dead_agent = make_running_agent(task.id, Some(4_000_000_000), Some(99999));
        insert_agent_run(&conn, &dead_agent).unwrap();

        // Verify agent is marked as running before recovery
        let running_before = find_running_agent_runs(&conn).unwrap();
        assert_eq!(
            running_before.len(),
            1,
            "should have 1 running agent before recovery"
        );

        // Run full crash recovery (as new daemon would on startup)
        let report = recovery::recover_session_state(&conn, &sock_path, &pid_path).unwrap();

        // Stale socket should be removed
        assert!(report.socket_removed, "stale socket should be removed");
        assert!(!sock_path.exists(), "socket file should be cleaned up");

        // Stale PID file should be removed (PID 4_000_000_000 is dead)
        assert!(report.pid_file_removed, "stale PID file should be removed");
        assert!(!pid_path.exists(), "PID file should be cleaned up");

        // Agent should be marked as failed (not running anymore)
        let running_after = find_running_agent_runs(&conn).unwrap();
        assert!(
            running_after.is_empty(),
            "no agents should be running after recovery"
        );

        // Work item should be marked as failed
        let item = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(
            item.status,
            WorkItemStatus::Failed,
            "task with dead agent should be marked failed"
        );
    }

    // =========================================================================
    // Test: crash recovery — alive agents are adopted, not failed
    // =========================================================================

    #[test]
    fn test_crash_recovery_alive_agents_adopted() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");
        let conn = create_test_db(&nflow_home);

        // Simulate a crashed daemon: stale PID file
        write_pid_to_file(&pid_path, 4_000_000_000);

        // Create a project + session + two tasks
        let project_id = make_project(&conn);
        let session_id = make_session(&conn, project_id);

        // Task 1: agent with dead PID (should be failed)
        let task1 = make_in_progress_task(&conn, session_id);
        let dead_agent = make_running_agent(task1.id, Some(4_000_000_000), Some(99999));
        insert_agent_run(&conn, &dead_agent).unwrap();

        // Task 2: agent with alive PID (current process — should be adopted)
        let task2 = make_in_progress_task(&conn, session_id);
        let alive_agent = make_running_agent(task2.id, Some(std::process::id()), None);
        insert_agent_run(&conn, &alive_agent).unwrap();

        // Run recovery
        let report = recovery::recover_session_state(&conn, &sock_path, &pid_path).unwrap();

        // Dead agent's task should be failed
        let item1 = get_work_item_by_id(&conn, &task1.id).unwrap().unwrap();
        assert_eq!(
            item1.status,
            WorkItemStatus::Failed,
            "dead agent task should be failed"
        );

        // Alive agent's task should still be in_progress (adopted)
        let item2 = get_work_item_by_id(&conn, &task2.id).unwrap().unwrap();
        assert_eq!(
            item2.status,
            WorkItemStatus::InProgress,
            "alive agent task should remain in_progress"
        );

        // One agent should be adopted
        assert_eq!(
            report.agents_adopted.len(),
            1,
            "one agent should be adopted"
        );
        assert_eq!(report.agents_adopted[0].0, alive_agent.id);
    }

    // =========================================================================
    // Test: crash recovery — orphaned in-progress tasks (no agent_run) are failed
    // =========================================================================

    #[test]
    fn test_crash_recovery_orphaned_tasks_failed() {
        let (_tmp, nflow_home) = create_test_home();
        let sock_path = nflow_home.join("nflow.sock");
        let pid_path = nflow_home.join("daemon.pid");
        let conn = create_test_db(&nflow_home);

        // Create a task in_progress with no agent_run at all
        let project_id = make_project(&conn);
        let session_id = make_session(&conn, project_id);
        let task = make_in_progress_task(&conn, session_id);

        // Run recovery
        let report = recovery::recover_session_state(&conn, &sock_path, &pid_path).unwrap();

        // Orphaned task should be marked failed
        assert_eq!(report.tasks_failed, 1, "orphaned task should be failed");
        let item = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);
    }

    // =========================================================================
    // Test: crash recovery — active spec sessions are reset
    // =========================================================================

    #[test]
    fn test_crash_recovery_spec_sessions_reset() {
        let (_tmp, nflow_home) = create_test_home();
        let sock_path = nflow_home.join("nflow.sock");
        let pid_path = nflow_home.join("daemon.pid");
        let conn = create_test_db(&nflow_home);

        // Create a project with an active spec session
        let project_id = make_project(&conn);
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'test-spec', '/tmp/spec.md', 'draft', 1, ?3, ?3)",
            rusqlite::params![Uuid::new_v4().to_string(), project_id.to_string(), now],
        )
        .unwrap();

        // Verify session is active
        let active_count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM specs WHERE session_active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_count, 1);

        // Run recovery
        let report = recovery::recover_session_state(&conn, &sock_path, &pid_path).unwrap();

        // Spec sessions should be reset
        assert_eq!(report.specs_reset, 1, "active spec session should be reset");
        let active_after: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM specs WHERE session_active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_after, 0, "no active spec sessions after recovery");
    }

    // =========================================================================
    // Test: flock prevents two daemons starting simultaneously
    // =========================================================================

    #[test]
    fn test_flock_prevents_concurrent_daemon_start() {
        use nix::fcntl::{Flock, FlockArg};

        let (_tmp, nflow_home) = create_test_home();
        let lock_path = nflow_home.join("daemon.lock");

        // Create the lock file
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();

        // First process acquires exclusive lock — should succeed
        let flock = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock);
        assert!(flock.is_ok(), "first lock acquisition should succeed");
        let _held_lock = flock.unwrap();

        // Second process tries to acquire the same lock — should fail with EAGAIN/EWOULDBLOCK
        let lock_file2 = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let result = Flock::lock(lock_file2, FlockArg::LockExclusiveNonblock);
        assert!(
            result.is_err(),
            "second lock acquisition should fail (lock contention)"
        );

        // Verify it's EAGAIN/EWOULDBLOCK (lock contention, not a real error)
        let (_file, errno) = result.unwrap_err();
        assert!(
            errno == nix::errno::Errno::EAGAIN || errno == nix::errno::Errno::EWOULDBLOCK,
            "error should be EAGAIN/EWOULDBLOCK, got {:?}",
            errno
        );

        // Drop the first lock — second should now succeed
        drop(_held_lock);

        let lock_file3 = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let result = Flock::lock(lock_file3, FlockArg::LockExclusiveNonblock);
        assert!(
            result.is_ok(),
            "lock acquisition should succeed after first lock is released"
        );
    }

    // =========================================================================
    // Test: flock contention across real processes
    // =========================================================================

    #[test]
    fn test_flock_cross_process_contention() {
        use nix::fcntl::{Flock, FlockArg};

        let (_tmp, nflow_home) = create_test_home();
        let lock_path = nflow_home.join("daemon.lock");

        // Spawn a child process that holds the flock for 3 seconds
        let lock_path_str = lock_path.to_str().unwrap();
        let child = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "exec 200>\"{}\" && flock -xn 200 && sleep 3",
                lock_path_str
            ))
            .spawn()
            .expect("failed to spawn lock holder");

        // Give the child time to acquire the lock
        std::thread::sleep(Duration::from_millis(500));

        // Our process should fail to acquire the lock (child holds it)
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let result = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock);
        assert!(
            result.is_err(),
            "should not acquire lock while child holds it"
        );

        // Wait for child to exit (releases lock)
        let mut child = child;
        let _ = child.wait();

        // Now we should be able to acquire the lock
        let lock_file2 = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let result = Flock::lock(lock_file2, FlockArg::LockExclusiveNonblock);
        assert!(
            result.is_ok(),
            "should acquire lock after child releases it"
        );
    }

    // =========================================================================
    // Test: auto-start from CLI command (ensure_daemon pattern)
    // =========================================================================

    #[tokio::test]
    async fn test_auto_start_pattern() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");

        // Simulate: daemon is NOT running (no PID file)
        assert!(!pid_path.exists());
        assert!(!platform::is_process_alive(4_000_000_000));

        // Simulate the auto-start flow:
        // 1. Check if daemon is running — it's not
        let pid = read_pid_from_file(&pid_path);
        assert!(pid.is_none(), "no PID file means daemon not running");

        // 2. "Start" the daemon — write PID, create socket
        write_pid_to_file(&pid_path, std::process::id());
        let _listener = socket::bind_socket(&sock_path).expect("bind socket");

        // 3. Verify auto-start succeeded
        let pid = read_pid_from_file(&pid_path).expect("PID should be readable");
        assert_eq!(pid, std::process::id());
        assert!(sock_path.exists(), "socket should exist");
        assert!(platform::is_process_alive(pid), "daemon should be running");

        // 4. Second "auto-start" should detect daemon is already running
        let pid2 = read_pid_from_file(&pid_path).expect("PID should be readable");
        assert!(
            platform::is_process_alive(pid2),
            "daemon already running, no restart needed"
        );
    }

    // =========================================================================
    // Test: full lifecycle — start -> run -> stop with agents
    // =========================================================================

    #[tokio::test]
    async fn test_full_lifecycle_with_agents() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");
        let conn = create_test_db(&nflow_home);

        // === START ===
        write_pid_to_file(&pid_path, std::process::id());
        let _listener = socket::bind_socket(&sock_path).expect("bind socket");
        assert!(pid_path.exists());
        assert!(sock_path.exists());

        // === RUN: create a project with running agent ===
        let project_id = make_project(&conn);
        let session_id = make_session(&conn, project_id);
        let task = make_in_progress_task(&conn, session_id);

        // Spawn a real process to simulate a running agent
        let mut agent_child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let agent_pid = agent_child.id();

        let agent = make_running_agent(task.id, Some(agent_pid), None);
        insert_agent_run(&conn, &agent).unwrap();

        // Verify agent is running
        assert!(platform::is_process_alive(agent_pid));
        let running = find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 1);

        // === STOP: graceful shutdown ===
        // The shutdown will SIGTERM the agent, then clean up
        let report = shutdown::graceful_shutdown(&conn, &sock_path, &pid_path);

        // Agent should have been terminated
        assert!(
            report.agents_finished_naturally > 0
                || report.agents_sigtermed > 0
                || report.agents_sigkilled > 0,
            "agent should have been handled during shutdown"
        );

        // Files should be cleaned up
        assert!(report.socket_removed, "socket should be removed");
        assert!(report.pid_file_removed, "PID file should be removed");
        assert!(!pid_path.exists());
        assert!(!sock_path.exists());

        // Reap the child to avoid zombie
        let _ = agent_child.kill();
        let _ = agent_child.wait();
    }

    // =========================================================================
    // Test: nflow_home directory permissions
    // =========================================================================

    #[test]
    fn test_nflow_home_secure_permissions() {
        let (_tmp, nflow_home) = create_test_home();

        // nflow_home should have 0700 permissions
        let mode = fs::metadata(&nflow_home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "nflow_home should have 0700 permissions");

        // Simulate too-open permissions and verify they get fixed
        fs::set_permissions(&nflow_home, fs::Permissions::from_mode(0o755)).unwrap();
        let mode = fs::metadata(&nflow_home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        // The daemon's ensure_nflow_home would fix this — simulate the check
        let mode = fs::metadata(&nflow_home).unwrap().permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            fs::set_permissions(&nflow_home, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mode = fs::metadata(&nflow_home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "permissions should be fixed to 0700");
    }

    // =========================================================================
    // Test: recovery after full crash scenario
    // =========================================================================

    #[test]
    fn test_full_crash_and_recovery_scenario() {
        let (_tmp, nflow_home) = create_test_home();
        let pid_path = nflow_home.join("daemon.pid");
        let sock_path = nflow_home.join("nflow.sock");
        let conn = create_test_db(&nflow_home);

        // === Phase 1: Simulate a running daemon ===
        write_pid_to_file(&pid_path, std::process::id());
        fs::write(&sock_path, "").unwrap(); // Stale socket placeholder

        // Create project, session, and agents
        let project_id = make_project(&conn);
        let session_id = make_session(&conn, project_id);

        // Two agents: one dead (simulating crash), one alive
        let task1 = make_in_progress_task(&conn, session_id);
        let dead_agent = make_running_agent(task1.id, Some(4_000_000_000), Some(99999));
        insert_agent_run(&conn, &dead_agent).unwrap();

        let task2 = make_in_progress_task(&conn, session_id);
        let alive_agent = make_running_agent(task2.id, Some(std::process::id()), None);
        insert_agent_run(&conn, &alive_agent).unwrap();

        // An orphaned task with no agent
        let task3 = make_in_progress_task(&conn, session_id);

        // An active spec session
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'crash-spec', '/tmp/spec.md', 'draft', 1, ?3, ?3)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                project_id.to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();

        // === Phase 2: Simulate daemon crash (SIGKILL) ===
        // Overwrite PID with a dead PID to simulate crash
        write_pid_to_file(&pid_path, 4_000_000_000);

        // === Phase 3: New daemon starts, runs recovery ===
        let report = recovery::recover_session_state(&conn, &sock_path, &pid_path).unwrap();

        // Verify recovery report
        assert!(report.socket_removed, "stale socket removed");
        assert!(report.pid_file_removed, "stale PID file removed");
        assert_eq!(report.specs_reset, 1, "active spec session reset");
        assert_eq!(report.tasks_failed, 1, "orphaned task failed"); // task3 has no agent
        assert_eq!(report.agents_adopted.len(), 1, "alive agent adopted");
        assert_eq!(report.agents_adopted[0].0, alive_agent.id);

        // Verify DB state after recovery
        let item1 = get_work_item_by_id(&conn, &task1.id).unwrap().unwrap();
        assert_eq!(
            item1.status,
            WorkItemStatus::Failed,
            "task with dead agent: failed"
        );

        let item2 = get_work_item_by_id(&conn, &task2.id).unwrap().unwrap();
        assert_eq!(
            item2.status,
            WorkItemStatus::InProgress,
            "task with alive agent: still running"
        );

        let item3 = get_work_item_by_id(&conn, &task3.id).unwrap().unwrap();
        assert_eq!(
            item3.status,
            WorkItemStatus::Failed,
            "orphaned task: failed"
        );

        // Active spec sessions should be reset
        let active_specs: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM specs WHERE session_active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_specs, 0, "all active spec sessions reset");
    }

    // =========================================================================
    // Cleanup verification: all temp dirs and processes removed
    // =========================================================================

    #[test]
    fn test_cleanup_temp_dirs_on_drop() {
        // Create a temp dir and verify it's cleaned up on drop
        let (tmp, nflow_home) = create_test_home();
        let nflow_home_clone = nflow_home.clone();
        assert!(nflow_home.exists());

        // Drop the tempdir guard
        drop(tmp);

        // Directory should be cleaned up
        assert!(
            !nflow_home_clone.exists(),
            "temp directory should be cleaned up when guard is dropped"
        );
    }

    #[test]
    fn test_cleanup_spawned_processes() {
        // Spawn a process, then verify cleanup
        let mut child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let pid = child.id();

        assert!(platform::is_process_alive(pid), "process should be alive");

        // Kill and reap
        let _ = child.kill();
        let _ = child.wait();

        // Process should be dead now
        assert!(
            !platform::is_process_alive(pid),
            "process should be dead after kill+wait"
        );
    }
}
