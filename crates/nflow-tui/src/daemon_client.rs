use std::env;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use nix::fcntl::{Flock, FlockArg};

use crate::error::{Result, TuiError};

/// Returns the nflow home directory (~/.nflow/).
pub fn nflow_home() -> Result<PathBuf> {
    let home = env::var("HOME").map_err(|_| {
        TuiError::Io(std::io::Error::new(
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
        TuiError::Io(std::io::Error::new(
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
fn wait_for_socket(timeout: Duration) -> Result<()> {
    let sock = socket_path()?;
    let start = Instant::now();
    while start.elapsed() < timeout {
        if sock.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(TuiError::Socket(format!(
        "daemon did not start within {}s (socket not found at {})",
        timeout.as_secs(),
        sock.display()
    )))
}

/// Spawns the daemon as a background process.
fn spawn_daemon() -> Result<u32> {
    let daemon_binary = find_daemon_binary()?;

    let mut cmd = Command::new(&daemon_binary);
    cmd.env("NFLOW_DAEMON_MODE", "background")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());

    let child = cmd.spawn().map_err(|e| {
        TuiError::Io(std::io::Error::new(
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
fn is_lock_contention(errno: nix::errno::Errno) -> bool {
    errno == nix::errno::Errno::EAGAIN || errno == nix::errno::Errno::EWOULDBLOCK
}

/// Acquire an exclusive flock on ~/.nflow/daemon.lock with a timeout.
fn acquire_lock(timeout: Duration) -> Result<Option<Flock<File>>> {
    let lock_file = open_lock_file()?;

    match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => return Ok(Some(flock)),
        Err((_file, e)) if is_lock_contention(e) => {}
        Err((_file, e)) => {
            return Err(TuiError::Io(std::io::Error::other(format!(
                "flock failed: {}",
                e
            ))));
        }
    }

    let start = Instant::now();
    while start.elapsed() < timeout {
        thread::sleep(Duration::from_millis(100));

        let lock_file = open_lock_file()?;
        match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
            Ok(flock) => return Ok(Some(flock)),
            Err((_file, e)) if is_lock_contention(e) => continue,
            Err((_file, e)) => {
                return Err(TuiError::Io(std::io::Error::other(format!(
                    "flock failed: {}",
                    e
                ))));
            }
        }
    }

    Ok(None)
}

/// Ensures the daemon is running, starting it if necessary.
pub fn ensure_daemon() -> Result<()> {
    if is_daemon_running()? {
        return Ok(());
    }

    let lock_timeout = Duration::from_secs(5);
    let socket_timeout = Duration::from_secs(5);

    match acquire_lock(lock_timeout)? {
        Some(_lock) => {
            if is_daemon_running()? {
                return Ok(());
            }
            let _pid = spawn_daemon()?;
            wait_for_socket(socket_timeout)?;
            Ok(())
        }
        None => wait_for_socket(socket_timeout),
    }
}
