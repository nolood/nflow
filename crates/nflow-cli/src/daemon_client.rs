use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use nix::fcntl::{Flock, FlockArg};

use crate::error::{CliError, Result};

/// Returns the nflow home directory (~/.nflow/).
pub fn nflow_home() -> Result<PathBuf> {
    let home = env::var("HOME").map_err(|_| {
        CliError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME environment variable not set",
        ))
    })?;
    Ok(PathBuf::from(home).join(".nflow"))
}

/// Returns the path to the daemon PID file (~/.nflow/daemon.pid).
pub fn pid_file_path() -> Result<PathBuf> {
    Ok(nflow_home()?.join("daemon.pid"))
}

/// Returns the path to the daemon socket.
///
/// Uses `NFLOW_SOCKET` env var if set, otherwise `~/.nflow/nflow.sock`.
pub fn socket_path() -> Result<PathBuf> {
    if let Ok(override_path) = env::var("NFLOW_SOCKET") {
        return Ok(PathBuf::from(override_path));
    }
    Ok(nflow_home()?.join("nflow.sock"))
}

/// Returns the path to the daemon lock file (~/.nflow/daemon.lock).
pub fn lock_file_path() -> Result<PathBuf> {
    Ok(nflow_home()?.join("daemon.lock"))
}

/// Ensures the ~/.nflow/ directory exists.
fn ensure_nflow_home() -> Result<()> {
    let home = nflow_home()?;
    fs::create_dir_all(&home)?;
    Ok(())
}

/// Reads the PID from ~/.nflow/daemon.pid. Returns None if file doesn't exist.
fn read_pid_file() -> Result<Option<u32>> {
    let pid_path = pid_file_path()?;
    let content = match fs::read_to_string(&pid_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let pid = content.trim().parse::<u32>().map_err(|e| {
        CliError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid PID in daemon.pid: {}", e),
        ))
    })?;
    Ok(Some(pid))
}

/// Check if a daemon process is currently running by reading the PID file
/// and checking if the process exists.
pub fn is_daemon_running() -> Result<bool> {
    match read_pid_file()? {
        None => Ok(false),
        Some(pid) => {
            let pid = nix::unistd::Pid::from_raw(pid as i32);
            match nix::sys::signal::kill(pid, None) {
                Ok(()) => Ok(true),
                Err(nix::errno::Errno::ESRCH) => Ok(false),
                Err(nix::errno::Errno::EPERM) => Ok(true),
                Err(_) => Ok(false),
            }
        }
    }
}

/// Waits for the daemon socket to appear on disk, polling every 100ms.
/// Returns Ok(()) if the socket appears within the timeout, or an error.
fn wait_for_socket(timeout: Duration) -> Result<()> {
    let sock = socket_path()?;
    let start = Instant::now();
    while start.elapsed() < timeout {
        if sock.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(CliError::Socket(format!(
        "daemon did not start within {}s (socket not found at {})",
        timeout.as_secs(),
        sock.display()
    )))
}

/// Spawns the daemon as a background process.
///
/// Locates the nflow-daemon binary by looking next to the current executable,
/// then falls back to PATH lookup.
fn spawn_daemon() -> Result<u32> {
    let daemon_binary = find_daemon_binary()?;

    let mut cmd = Command::new(&daemon_binary);
    cmd.env("NFLOW_DAEMON_MODE", "background")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());

    let child = cmd.spawn().map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "failed to spawn daemon ({}): {}",
                daemon_binary.display(),
                e
            ),
        ))
    })?;

    Ok(child.id())
}

/// Find the nflow-daemon binary. First checks next to the current executable,
/// then falls back to "nflow-daemon" in PATH.
fn find_daemon_binary() -> Result<PathBuf> {
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("nflow-daemon");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Ok(PathBuf::from("nflow-daemon"))
}

/// Open the lock file for exclusive locking.
fn open_lock_file() -> Result<File> {
    ensure_nflow_home()?;
    let lock_path = lock_file_path()?;
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    Ok(lock_file)
}

/// Returns true if the errno indicates the lock is held by another process.
/// On Linux, EAGAIN and EWOULDBLOCK are the same value (11). On some POSIX
/// systems they may differ, so we check both via numeric comparison.
fn is_lock_contention(errno: nix::errno::Errno) -> bool {
    errno == nix::errno::Errno::EAGAIN || errno == nix::errno::Errno::EWOULDBLOCK
}

/// Acquire an exclusive flock on ~/.nflow/daemon.lock with a timeout.
///
/// Returns Some(Flock<File>) if the lock was acquired (caller must keep it alive),
/// or None if the lock could not be acquired within the timeout (another client
/// is starting the daemon).
fn acquire_lock(timeout: Duration) -> Result<Option<Flock<File>>> {
    let lock_file = open_lock_file()?;

    // Try non-blocking first
    match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => return Ok(Some(flock)),
        Err((_file, e)) if is_lock_contention(e) => {
            // Lock is held by another process — poll with timeout
        }
        Err((_file, e)) => {
            return Err(CliError::Io(std::io::Error::other(format!(
                "flock failed: {}",
                e
            ))));
        }
    }

    // Poll for the lock with timeout
    let start = Instant::now();
    while start.elapsed() < timeout {
        thread::sleep(Duration::from_millis(100));

        let lock_file = open_lock_file()?;
        match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => return Ok(Some(flock)),
            Err((_file, e)) if is_lock_contention(e) => continue,
            Err((_file, e)) => {
                return Err(CliError::Io(std::io::Error::other(format!(
                    "flock failed: {}",
                    e
                ))));
            }
        }
    }

    // Could not acquire lock — another client is starting the daemon
    Ok(None)
}

/// Ensures the daemon is running, starting it if necessary.
///
/// This is the main entry point for CLI commands that need to talk to the daemon.
/// It handles the full auto-start flow:
///
/// 1. Check if daemon is already running (PID file + process alive)
/// 2. If not running, try to acquire flock on daemon.lock
/// 3. If lock acquired: spawn daemon, wait for socket
/// 4. If lock not acquired (another CLI is starting): just wait for socket
/// 5. Error if daemon doesn't start within timeout
pub fn ensure_daemon() -> Result<()> {
    // Already running — nothing to do
    if is_daemon_running()? {
        return Ok(());
    }

    let lock_timeout = Duration::from_secs(5);
    let socket_timeout = Duration::from_secs(5);

    match acquire_lock(lock_timeout)? {
        Some(_lock) => {
            // We hold the lock — re-check after acquiring (another client may have started it)
            if is_daemon_running()? {
                // Lock is automatically released when _lock drops
                return Ok(());
            }

            // Spawn the daemon
            let _pid = spawn_daemon()?;

            // Wait for the socket to appear
            wait_for_socket(socket_timeout)?;

            // Lock is released when _lock drops
            Ok(())
        }
        None => {
            // Another client is starting the daemon — just wait for the socket
            wait_for_socket(socket_timeout)
        }
    }
}

/// Status information for the daemon process.
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub uptime: Option<Duration>,
}

impl std::fmt::Display for DaemonStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.running {
            return write!(f, "nflow daemon is not running");
        }
        let pid = self.pid.unwrap_or(0);
        match self.uptime {
            Some(uptime) => {
                let secs = uptime.as_secs();
                let hours = secs / 3600;
                let mins = (secs % 3600) / 60;
                let secs = secs % 60;
                write!(
                    f,
                    "nflow daemon is running (pid: {}, uptime: {}h {}m {}s)",
                    pid, hours, mins, secs
                )
            }
            None => write!(f, "nflow daemon is running (pid: {})", pid),
        }
    }
}

/// Check daemon status: reads PID file, checks if process is alive,
/// reports status + PID + uptime.
///
/// If the PID file exists but the process is dead, cleans up the stale PID file.
pub fn daemon_status() -> Result<DaemonStatus> {
    match read_pid_file()? {
        None => Ok(DaemonStatus {
            running: false,
            pid: None,
            uptime: None,
        }),
        Some(pid) => {
            let alive = is_process_alive(pid);
            if !alive {
                // Clean up stale PID file
                let _ = remove_pid_file();
                return Ok(DaemonStatus {
                    running: false,
                    pid: None,
                    uptime: None,
                });
            }

            let uptime = pid_file_uptime();
            Ok(DaemonStatus {
                running: true,
                pid: Some(pid),
                uptime,
            })
        }
    }
}

/// Stop the daemon by sending SIGTERM to the PID read from the PID file.
///
/// Returns the PID of the stopped daemon process, or an error if the
/// daemon is not running or the signal cannot be sent.
pub fn daemon_stop() -> Result<u32> {
    match read_pid_file()? {
        None => Err(CliError::Socket("daemon is not running".to_string())),
        Some(pid) => {
            if !is_process_alive(pid) {
                // Clean up stale PID file
                let _ = remove_pid_file();
                return Err(CliError::Socket(
                    "daemon is not running (stale PID file cleaned up)".to_string(),
                ));
            }

            // Send SIGTERM
            let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
            nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM).map_err(|e| {
                CliError::Io(std::io::Error::other(format!(
                    "failed to send SIGTERM to daemon (pid: {}): {}",
                    pid, e
                )))
            })?;

            Ok(pid)
        }
    }
}

/// Check if a process is alive by sending signal 0.
fn is_process_alive(pid: u32) -> bool {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    matches!(
        nix::sys::signal::kill(nix_pid, None),
        Ok(()) | Err(nix::errno::Errno::EPERM)
    )
}

/// Remove the PID file.
fn remove_pid_file() -> Result<()> {
    let pid_path = pid_file_path()?;
    match fs::remove_file(&pid_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Read the PID file's modification time to estimate uptime.
fn pid_file_uptime() -> Option<Duration> {
    let pid_path = pid_file_path().ok()?;
    let metadata = fs::metadata(&pid_path).ok()?;
    let modified = metadata.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nflow_home() {
        let home = nflow_home().unwrap();
        let expected = PathBuf::from(env::var("HOME").unwrap()).join(".nflow");
        assert_eq!(home, expected);
    }

    #[test]
    fn test_path_helpers() {
        let pid = pid_file_path().unwrap();
        assert!(pid.ends_with("daemon.pid"));

        let sock = socket_path().unwrap();
        assert!(sock.ends_with("nflow.sock"));

        let lock = lock_file_path().unwrap();
        assert!(lock.ends_with("daemon.lock"));
    }

    #[test]
    fn test_find_daemon_binary_fallback() {
        let binary = find_daemon_binary().unwrap();
        assert!(
            binary.to_string_lossy().contains("nflow-daemon"),
            "expected nflow-daemon in path, got: {}",
            binary.display()
        );
    }

    #[test]
    fn test_acquire_lock_success() {
        ensure_nflow_home().unwrap();
        let lock = acquire_lock(Duration::from_secs(1)).unwrap();
        assert!(lock.is_some(), "should acquire lock on first attempt");
        // Lock is released when dropped
    }

    #[test]
    fn test_acquire_lock_contention() {
        ensure_nflow_home().unwrap();

        // Hold the lock via Flock
        let held_file = open_lock_file().unwrap();
        let held_lock =
            Flock::lock(held_file, FlockArg::LockExclusiveNonblock).expect("should acquire lock");

        // Try to acquire from "another client" — should fail (timeout 0.5s)
        let start = Instant::now();
        let result = acquire_lock(Duration::from_millis(500)).unwrap();
        let elapsed = start.elapsed();

        assert!(result.is_none(), "should not acquire lock when held");
        assert!(
            elapsed >= Duration::from_millis(400),
            "should wait near timeout: {:?}",
            elapsed
        );

        // Release the held lock
        drop(held_lock);
    }

    #[test]
    fn test_wait_for_socket_timeout() {
        let start = Instant::now();
        let result = wait_for_socket(Duration::from_millis(200));
        let elapsed = start.elapsed();

        // The socket might already exist from the daemon, so only assert timeout
        // if the result is an error
        if result.is_err() {
            assert!(
                elapsed >= Duration::from_millis(150),
                "should wait near timeout: {:?}",
                elapsed
            );
            let err = result.unwrap_err().to_string();
            assert!(err.contains("did not start"), "unexpected error: {}", err);
        }
    }

    #[test]
    fn test_is_daemon_running_no_pid_file() {
        let result = is_daemon_running();
        assert!(result.is_ok());
    }

    #[test]
    fn test_ensure_daemon_smoke() {
        // Smoke test — behavior depends on whether daemon is actually running
        let result = ensure_daemon();
        let _ = result;
    }

    #[test]
    fn test_is_process_alive_current() {
        let pid = std::process::id();
        assert!(is_process_alive(pid));
    }

    #[test]
    fn test_is_process_alive_dead() {
        // Use a PID that's almost certainly not alive
        assert!(!is_process_alive(999_999_999));
    }

    #[test]
    fn test_daemon_status_running_display() {
        let status = DaemonStatus {
            running: true,
            pid: Some(1234),
            uptime: Some(Duration::from_secs(3661)),
        };
        let display = status.to_string();
        assert!(display.contains("pid: 1234"));
        assert!(display.contains("1h 1m 1s"));
    }

    #[test]
    fn test_daemon_status_running_no_uptime() {
        let status = DaemonStatus {
            running: true,
            pid: Some(5678),
            uptime: None,
        };
        let display = status.to_string();
        assert!(display.contains("pid: 5678"));
        assert!(!display.contains("uptime"));
    }

    #[test]
    fn test_remove_pid_file_idempotent() {
        let _ = remove_pid_file();
        // Second call should not fail
        assert!(remove_pid_file().is_ok());
    }

    /// Tests that share the PID file must run sequentially in a single test
    /// to avoid race conditions from parallel test execution.
    #[test]
    fn test_daemon_status_and_stop_with_pid_file() {
        ensure_nflow_home().unwrap();
        let pid_path = pid_file_path().unwrap();

        // 1. No PID file → status reports not running
        let _ = remove_pid_file();
        let status = daemon_status().unwrap();
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(status.uptime.is_none());
        assert_eq!(status.to_string(), "nflow daemon is not running");

        // 2. No PID file → stop returns error
        let result = daemon_stop();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not running"));

        // 3. Write a dead PID → status reports not running and cleans up
        fs::write(&pid_path, "999999999").unwrap();
        let status = daemon_status().unwrap();
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(!pid_path.exists());

        // 4. Write a dead PID again → stop returns error with stale message and cleans up
        fs::write(&pid_path, "999999999").unwrap();
        let result = daemon_stop();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not running"));
        assert!(err.contains("stale"));
        assert!(!pid_path.exists());

        // 5. Uptime estimation: write a PID file, check uptime is small
        fs::write(&pid_path, "12345").unwrap();
        let uptime = pid_file_uptime();
        assert!(uptime.is_some());
        assert!(uptime.unwrap() < Duration::from_secs(5));

        // Cleanup
        let _ = remove_pid_file();
    }
}
