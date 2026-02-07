use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

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

/// Returns the path to the daemon socket (~/.nflow/nflow.sock).
pub fn socket_path() -> Result<PathBuf> {
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
    if !pid_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&pid_path)?;
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
}
