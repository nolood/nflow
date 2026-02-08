//! E2E tests that spawn the real nflow-daemon binary as a child process.
//!
//! Unlike the in-process e2e_tests which use `start_real_server()`, these tests
//! exercise the actual binary: main.rs → foreground_mode → migrations → recovery
//! → socket server → scheduler loop. This verifies the full daemon startup sequence.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    /// Find the nflow-daemon binary in the target directory.
    /// Uses the CARGO_BIN_EXE_nflow-daemon env var if available (set by cargo test),
    /// otherwise falls back to searching target/debug/.
    fn daemon_binary_path() -> PathBuf {
        // cargo test sets CARGO_BIN_EXE_<name> for binaries in the same package
        if let Ok(path) = std::env::var("CARGO_BIN_EXE_nflow-daemon") {
            return PathBuf::from(path);
        }

        // Fallback: walk up from current exe to find target/debug/nflow-daemon
        let mut dir = std::env::current_exe()
            .expect("cannot get current exe path")
            .parent()
            .unwrap()
            .to_path_buf();

        // Walk up until we find target/debug/nflow-daemon
        loop {
            let candidate = dir.join("nflow-daemon");
            if candidate.exists() {
                return candidate;
            }
            if !dir.pop() {
                break;
            }
        }

        panic!("Could not find nflow-daemon binary. Run `cargo build` first.");
    }

    /// Create an isolated temp home directory with the expected .nflow structure.
    fn create_test_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let nflow_home = tmp.path().join(".nflow");
        fs::create_dir_all(&nflow_home).unwrap();
        fs::set_permissions(&nflow_home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(nflow_home.join("logs")).unwrap();
        (tmp, nflow_home)
    }

    /// Create a git repository in the given directory for project.init to succeed.
    fn create_git_repo(path: &Path) -> PathBuf {
        let repo_dir = path.join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo_dir)
            .output()
            .expect("failed to init git repo");

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        // Create an initial commit so the repo is valid
        let placeholder = repo_dir.join("README.md");
        fs::write(&placeholder, "# test\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        repo_dir
    }

    /// Spawn the daemon binary with HOME set to the given temp directory.
    fn spawn_daemon(home_dir: &Path) -> Child {
        let bin = daemon_binary_path();
        Command::new(bin)
            .env("HOME", home_dir)
            .env("NFLOW_DAEMON_MODE", "foreground")
            .env("RUST_LOG", "info")
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn nflow-daemon")
    }

    /// Poll for a file to appear, with timeout.
    fn wait_for_file(path: &Path, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Connect to the Unix socket and perform the NDJSON handshake.
    fn connect_and_handshake(socket_path: &Path) -> (UnixStream, BufReader<UnixStream>) {
        let stream = UnixStream::connect(socket_path).expect("failed to connect to socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut writer = stream.try_clone().unwrap();
        let reader = BufReader::new(stream);

        // Send handshake
        writeln!(writer, "{{\"protocol_version\":1}}").unwrap();
        writer.flush().unwrap();

        // Read handshake response
        let mut line = String::new();
        let mut reader_clone = reader;
        reader_clone
            .read_line(&mut line)
            .expect("failed to read handshake response");
        let resp: serde_json::Value = serde_json::from_str(&line).expect("invalid handshake JSON");
        assert_eq!(resp["protocol_version"], 1);
        assert_eq!(resp["status"], "ok");

        (writer, reader_clone)
    }

    /// Send a request and read a single response.
    fn send_request(
        writer: &mut UnixStream,
        reader: &mut BufReader<UnixStream>,
        id: &str,
        command: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let req = serde_json::json!({
            "id": id,
            "command": command,
            "params": params,
        });
        writeln!(writer, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        writer.flush().unwrap();

        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("failed to read response");
        serde_json::from_str(&line).expect("invalid response JSON")
    }

    /// Send SIGTERM to a child process.
    fn send_sigterm(child: &Child) {
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }

    /// Wait for a file to disappear, with timeout.
    fn wait_for_file_gone(path: &Path, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if !path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn test_daemon_binary_socket_lifecycle() {
        // --- Setup ---
        let (tmp, nflow_home) = create_test_home();
        let repo_dir = create_git_repo(tmp.path());
        let socket_path = nflow_home.join("nflow.sock");
        let pid_path = nflow_home.join("daemon.pid");

        // Spawn the real daemon binary
        let mut child = spawn_daemon(tmp.path());

        // --- AC1: Poll for socket file creation (10s timeout) ---
        assert!(
            wait_for_file(&socket_path, Duration::from_secs(10)),
            "socket file was not created within 10s"
        );

        // Verify socket permissions are 0600
        let socket_meta = fs::metadata(&socket_path).unwrap();
        let socket_mode = socket_meta.permissions().mode() & 0o777;
        assert_eq!(
            socket_mode, 0o600,
            "socket should have 0600 permissions, got {:04o}",
            socket_mode
        );

        // --- AC2: PID file should exist ---
        assert!(pid_path.exists(), "PID file should exist");
        let pid_content = fs::read_to_string(&pid_path).unwrap();
        let daemon_pid: u32 = pid_content.trim().parse().expect("PID file should contain a number");
        assert_eq!(daemon_pid, child.id(), "PID file should match child PID");

        // --- AC3: Perform NDJSON handshake ---
        let (mut writer, mut reader) = connect_and_handshake(&socket_path);

        // --- AC4: Send project.init and verify success ---
        let resp = send_request(
            &mut writer,
            &mut reader,
            "req-1",
            "project.init",
            serde_json::json!({
                "name": "test-project",
                "path": repo_dir.to_str().unwrap(),
                "base_branch": "main",
                "git_provider": "github"
            }),
        );
        assert_eq!(resp["id"], "req-1");
        assert_eq!(resp["status"], "ok", "project.init failed: {:?}", resp);

        // --- AC5: Send project.list and see the created project ---
        let resp = send_request(
            &mut writer,
            &mut reader,
            "req-2",
            "project.list",
            serde_json::json!({}),
        );
        assert_eq!(resp["id"], "req-2");
        assert_eq!(resp["status"], "ok", "project.list failed: {:?}", resp);

        let projects = resp["data"]["projects"]
            .as_array()
            .expect("data.projects should be an array");
        assert_eq!(projects.len(), 1, "should have exactly one project");
        assert_eq!(projects[0]["name"], "test-project");

        // --- AC6: Send SIGTERM and verify cleanup ---
        // Drop the connection first to avoid broken pipe issues
        drop(writer);
        drop(reader);

        send_sigterm(&child);

        // Wait for the process to exit
        let status = child
            .wait()
            .expect("failed to wait for daemon");

        // Process should have exited (SIGTERM causes graceful shutdown)
        assert!(
            status.success() || status.code().is_none(),
            "daemon should exit cleanly on SIGTERM, got: {:?}",
            status
        );

        // Socket file should be cleaned up
        assert!(
            wait_for_file_gone(&socket_path, Duration::from_secs(5)),
            "socket file should be removed after shutdown"
        );

        // PID file should be cleaned up
        assert!(
            !pid_path.exists(),
            "PID file should be removed after shutdown"
        );
    }

    #[test]
    fn test_daemon_binary_database_initialized() {
        // Verify that the daemon creates and migrates the database on startup
        let (tmp, nflow_home) = create_test_home();
        let repo_dir = create_git_repo(tmp.path());
        let socket_path = nflow_home.join("nflow.sock");
        let db_path = nflow_home.join("nflow.db");

        // Spawn the daemon
        let mut child = spawn_daemon(tmp.path());

        // Wait for socket (indicates full startup complete)
        assert!(
            wait_for_file(&socket_path, Duration::from_secs(10)),
            "socket file was not created within 10s"
        );

        // Database should exist and have the schema_version table
        assert!(db_path.exists(), "database file should exist");

        // Connect and verify we can create a project (proves schema is migrated)
        let (mut writer, mut reader) = connect_and_handshake(&socket_path);
        let resp = send_request(
            &mut writer,
            &mut reader,
            "req-1",
            "project.init",
            serde_json::json!({
                "name": "db-test",
                "path": repo_dir.to_str().unwrap(),
                "base_branch": "main",
                "git_provider": "github"
            }),
        );
        assert_eq!(resp["status"], "ok", "project.init should succeed: {:?}", resp);

        // Cleanup
        drop(writer);
        drop(reader);
        send_sigterm(&child);
        let _ = child.wait();
    }

    #[test]
    fn test_daemon_binary_multiple_clients() {
        // Verify that the daemon can handle multiple concurrent client connections
        let (tmp, nflow_home) = create_test_home();
        let repo_dir = create_git_repo(tmp.path());
        let socket_path = nflow_home.join("nflow.sock");

        let mut child = spawn_daemon(tmp.path());

        assert!(
            wait_for_file(&socket_path, Duration::from_secs(10)),
            "socket file was not created within 10s"
        );

        // Client 1: create a project
        let (mut w1, mut r1) = connect_and_handshake(&socket_path);
        let resp = send_request(
            &mut w1,
            &mut r1,
            "c1-req-1",
            "project.init",
            serde_json::json!({
                "name": "client1-project",
                "path": repo_dir.to_str().unwrap(),
                "base_branch": "main",
                "git_provider": "github"
            }),
        );
        assert_eq!(resp["status"], "ok");

        // Client 2: list projects (should see client1's project)
        let (mut w2, mut r2) = connect_and_handshake(&socket_path);
        let resp = send_request(
            &mut w2,
            &mut r2,
            "c2-req-1",
            "project.list",
            serde_json::json!({}),
        );
        assert_eq!(resp["status"], "ok");
        let projects = resp["data"]["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "client1-project");

        // Cleanup
        drop(w1);
        drop(r1);
        drop(w2);
        drop(r2);
        send_sigterm(&child);
        let _ = child.wait();
    }
}
