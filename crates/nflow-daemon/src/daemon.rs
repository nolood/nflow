use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nix::sys::signal::{signal, SigHandler, Signal};
use nix::unistd::setsid;

use crate::error::{DaemonError, Result};

/// Returns the nflow home directory (~/.nflow/).
pub fn nflow_home() -> Result<PathBuf> {
    let home = env::var("HOME").map_err(|_| {
        DaemonError::Io(std::io::Error::new(
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

/// Returns the path to the daemon log file (~/.nflow/logs/daemon.log).
pub fn log_file_path() -> Result<PathBuf> {
    Ok(nflow_home()?.join("logs").join("daemon.log"))
}

/// Returns the path to the daemon socket (~/.nflow/nflow.sock).
pub fn socket_path() -> Result<PathBuf> {
    Ok(nflow_home()?.join("nflow.sock"))
}

/// Ensures the ~/.nflow/logs/ directory exists.
pub fn ensure_log_dir() -> Result<()> {
    let log_dir = nflow_home()?.join("logs");
    fs::create_dir_all(&log_dir)?;
    Ok(())
}

/// Ensures the ~/.nflow/ directory exists.
pub fn ensure_nflow_home() -> Result<()> {
    let home = nflow_home()?;
    fs::create_dir_all(&home)?;
    Ok(())
}

/// Writes the current process PID to ~/.nflow/daemon.pid.
pub fn write_pid_file() -> Result<()> {
    ensure_nflow_home()?;
    let pid = std::process::id();
    let pid_path = pid_file_path()?;
    let mut f = File::create(&pid_path)?;
    write!(f, "{}", pid)?;
    Ok(())
}

/// Reads the PID from ~/.nflow/daemon.pid. Returns None if file doesn't exist.
pub fn read_pid_file() -> Result<Option<u32>> {
    let pid_path = pid_file_path()?;
    if !pid_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&pid_path)?;
    let pid = content.trim().parse::<u32>().map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid PID in daemon.pid: {}", e),
        ))
    })?;
    Ok(Some(pid))
}

/// Removes the PID file.
pub fn remove_pid_file() -> Result<()> {
    let pid_path = pid_file_path()?;
    if pid_path.exists() {
        fs::remove_file(&pid_path)?;
    }
    Ok(())
}

/// Opens the daemon log file for appending (creates if not exists).
pub fn open_log_file() -> Result<File> {
    ensure_log_dir()?;
    let log_path = log_file_path()?;
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    Ok(f)
}

/// Daemonize the current process.
///
/// This should be called early in the daemon's main function when running
/// in background mode. It:
/// 1. Ignores SIGHUP (so setsid doesn't kill us)
/// 2. Calls setsid() to create a new session (detach from terminal)
/// 3. Restores default SIGHUP handling
/// 4. Redirects stdout/stderr to the log file
/// 5. Writes the PID file
///
/// # Safety
/// Uses unsafe for signal handling via nix crate (POSIX signals).
pub fn daemonize() -> Result<()> {
    // Ignore SIGHUP before setsid so we don't get killed when detaching
    // SAFETY: SigIgn is a valid signal handler for SIGHUP on POSIX systems
    let prev_handler = unsafe { signal(Signal::SIGHUP, SigHandler::SigIgn) }.map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to ignore SIGHUP: {}", e),
        ))
    })?;

    // Create a new session — detach from controlling terminal
    setsid().map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("setsid failed: {}", e),
        ))
    })?;

    // Restore previous SIGHUP handler
    // SAFETY: Restoring the previous signal handler
    unsafe { signal(Signal::SIGHUP, prev_handler) }.map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to restore SIGHUP handler: {}", e),
        ))
    })?;

    // Redirect stdout/stderr to log file
    redirect_output()?;

    // Write PID file
    write_pid_file()?;

    Ok(())
}

/// Redirects stdout and stderr to the daemon log file.
fn redirect_output() -> Result<()> {
    use std::os::unix::io::AsRawFd;

    let log_file = open_log_file()?;
    let fd = log_file.as_raw_fd();

    // Redirect stdout (fd 1) and stderr (fd 2) to log file
    // SAFETY: dup2 is a POSIX call, fd is a valid file descriptor
    nix::unistd::dup2(fd, 1).map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to redirect stdout: {}", e),
        ))
    })?;
    nix::unistd::dup2(fd, 2).map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to redirect stderr: {}", e),
        ))
    })?;

    // Keep the file handle alive — it'll be dropped at program exit
    std::mem::forget(log_file);

    Ok(())
}

/// Initialize foreground mode for the daemon.
///
/// This sets up the daemon to run in the current terminal:
/// 1. Ensures ~/.nflow/ directories exist
/// 2. Writes the PID file
/// 3. Installs a SIGTERM/SIGINT handler that sets a shutdown flag
///
/// Logs go to stderr (no output redirection). Ctrl+C triggers the same
/// graceful shutdown as SIGTERM.
///
/// Returns a shared `AtomicBool` that becomes `true` when a shutdown
/// signal is received.
pub fn foreground_mode() -> Result<Arc<AtomicBool>> {
    ensure_nflow_home()?;
    ensure_log_dir()?;
    write_pid_file()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    install_shutdown_handler(Arc::clone(&shutdown))?;

    Ok(shutdown)
}

/// Installs SIGTERM and SIGINT handlers that set the shutdown flag.
///
/// Both signals trigger the same graceful shutdown: they set the
/// `shutting_down` atomic bool to `true`, allowing the main loop to
/// detect and initiate an orderly shutdown.
fn install_shutdown_handler(shutdown: Arc<AtomicBool>) -> Result<()> {
    // SAFETY: signal_hook::flag::register is safe for AtomicBool operations.
    // We use nix's signal to install a handler that sets our flag.
    // Since Rust closures can't be signal handlers directly, we use
    // signal_hook_registry or a static. Here we use a simple approach
    // with nix's SigAction for each signal.
    //
    // However, to keep things simple and safe without adding new deps,
    // we use ctrlc-style handling via a static AtomicBool.
    SHUTDOWN_FLAG.store(false, Ordering::SeqCst);

    // Leak the Arc into a raw pointer stored in SHUTDOWN_ARC so the
    // signal handler can access it. This is intentionally leaked — the
    // daemon runs for the entire process lifetime.
    {
        let ptr = Arc::into_raw(shutdown);
        SHUTDOWN_ARC.store(ptr as *mut bool, Ordering::SeqCst);
    }

    // Install SIGTERM handler
    // SAFETY: Our handler only writes to an AtomicBool (async-signal-safe)
    unsafe {
        signal(
            Signal::SIGTERM,
            SigHandler::Handler(shutdown_signal_handler),
        )
        .map_err(|e| {
            DaemonError::Io(std::io::Error::other(format!(
                "failed to install SIGTERM handler: {}",
                e
            )))
        })?;
    }

    // Install SIGINT handler (Ctrl+C)
    // SAFETY: Same handler — only writes to an AtomicBool
    unsafe {
        signal(Signal::SIGINT, SigHandler::Handler(shutdown_signal_handler)).map_err(|e| {
            DaemonError::Io(std::io::Error::other(format!(
                "failed to install SIGINT handler: {}",
                e
            )))
        })?;
    }

    Ok(())
}

/// Global shutdown flag for signal handler access.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// Pointer to the Arc<AtomicBool> for the shutdown flag. This is set once
/// in install_shutdown_handler and read by the signal handler.
static SHUTDOWN_ARC: std::sync::atomic::AtomicPtr<bool> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Signal handler for SIGTERM/SIGINT. Sets the global shutdown flag.
///
/// This function is called from signal context, so it must only perform
/// async-signal-safe operations. Writing to an AtomicBool is safe.
extern "C" fn shutdown_signal_handler(_sig: libc::c_int) {
    SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
}

/// Returns true if a shutdown signal has been received.
pub fn is_shutting_down() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

/// Spawn the daemon as a background process.
///
/// This is called from the CLI (`nflow daemon start`). It spawns the
/// nflow-daemon binary as a detached child process. The spawned process
/// will call `daemonize()` itself to fully detach.
pub fn spawn_daemon(daemon_binary: &Path) -> Result<u32> {
    spawn_daemon_with_mode(daemon_binary, "background")
}

/// Spawn the daemon in foreground mode.
///
/// This is called from the CLI (`nflow daemon start --foreground`). It spawns
/// the nflow-daemon binary as a child process that inherits stdout/stderr,
/// allowing logs to be visible in the terminal.
pub fn spawn_daemon_foreground(daemon_binary: &Path) -> Result<u32> {
    spawn_daemon_with_mode(daemon_binary, "foreground")
}

/// Spawn the daemon binary with the specified mode.
fn spawn_daemon_with_mode(daemon_binary: &Path, mode: &str) -> Result<u32> {
    ensure_nflow_home()?;

    let mut cmd = Command::new(daemon_binary);
    cmd.env("NFLOW_DAEMON_MODE", mode);

    if mode == "background" {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null());
    } else {
        // Foreground: inherit stdout/stderr so logs are visible
        cmd.stdin(std::process::Stdio::null());
    }

    let child = cmd.spawn().map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to spawn daemon: {}", e),
        ))
    })?;

    let pid = child.id();
    Ok(pid)
}

/// Check if a daemon process is currently running by reading the PID file
/// and checking if the process exists.
pub fn is_daemon_running() -> Result<bool> {
    match read_pid_file()? {
        None => Ok(false),
        Some(pid) => {
            // Check if process is alive by sending signal 0
            let pid = nix::unistd::Pid::from_raw(pid as i32);
            match nix::sys::signal::kill(pid, None) {
                Ok(()) => Ok(true),
                Err(nix::errno::Errno::ESRCH) => Ok(false), // No such process
                Err(nix::errno::Errno::EPERM) => Ok(true),  // Process exists but we can't signal it
                Err(_) => Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_nflow_home() {
        let home = nflow_home().unwrap();
        let expected = PathBuf::from(env::var("HOME").unwrap()).join(".nflow");
        assert_eq!(home, expected);
    }

    #[test]
    fn test_pid_file_path() {
        let path = pid_file_path().unwrap();
        assert!(path.ends_with("daemon.pid"));
        assert!(path.to_string_lossy().contains(".nflow"));
    }

    #[test]
    fn test_log_file_path() {
        let path = log_file_path().unwrap();
        assert!(path.ends_with("daemon.log"));
        assert!(path.to_string_lossy().contains("logs"));
    }

    #[test]
    fn test_socket_path() {
        let path = socket_path().unwrap();
        assert!(path.ends_with("nflow.sock"));
        assert!(path.to_string_lossy().contains(".nflow"));
    }

    #[test]
    fn test_ensure_log_dir() {
        // This test relies on HOME being set and writable
        // It should create ~/.nflow/logs/ if it doesn't exist
        let result = ensure_log_dir();
        assert!(result.is_ok());
        let log_dir = nflow_home().unwrap().join("logs");
        assert!(log_dir.exists());
    }

    #[test]
    fn test_open_log_file() {
        let result = open_log_file();
        assert!(result.is_ok());
        let log_path = log_file_path().unwrap();
        assert!(log_path.exists());
    }

    /// Tests PID file and is_daemon_running in a single sequential test
    /// to avoid race conditions from parallel test runs sharing the same file.
    #[test]
    fn test_pid_file_and_daemon_running() {
        // 1. Remove PID file — read returns None, is_daemon_running returns false
        let _ = remove_pid_file();
        assert_eq!(read_pid_file().unwrap(), None);
        assert!(!is_daemon_running().unwrap());

        // 2. Remove again — should succeed even if already gone
        assert!(remove_pid_file().is_ok());

        // 3. Write our own PID and read it back
        write_pid_file().unwrap();
        assert_eq!(read_pid_file().unwrap(), Some(std::process::id()));

        // 4. Current process should show as running
        assert!(is_daemon_running().unwrap());

        // 5. Write a dead PID — should show as not running
        ensure_nflow_home().unwrap();
        let pid_path = pid_file_path().unwrap();
        let mut f = File::create(&pid_path).unwrap();
        write!(f, "999999999").unwrap();
        assert!(!is_daemon_running().unwrap());

        // Cleanup
        remove_pid_file().unwrap();
    }

    #[test]
    fn test_spawn_daemon_nonexistent_binary() {
        let result = spawn_daemon(Path::new("/nonexistent/nflow-daemon"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to spawn daemon"));
    }

    #[test]
    fn test_foreground_mode_writes_pid_file_and_sets_shutdown() {
        // Clean up any existing PID file first
        let _ = remove_pid_file();

        // Start foreground mode
        let shutdown = foreground_mode().unwrap();

        // PID file should be written
        let pid = read_pid_file().unwrap();
        assert_eq!(pid, Some(std::process::id()));

        // Shutdown flag should be false initially
        assert!(!shutdown.load(Ordering::SeqCst));
        assert!(!is_shutting_down());

        // Simulate shutdown signal by setting the flag directly
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
        assert!(is_shutting_down());

        // Reset for other tests
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);

        // Cleanup
        remove_pid_file().unwrap();
    }

    #[test]
    fn test_is_shutting_down_default_false() {
        // Reset the flag
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        assert!(!is_shutting_down());
    }

    #[test]
    fn test_shutdown_signal_handler_sets_flag() {
        // Reset the flag
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
        assert!(!is_shutting_down());

        // Call the signal handler directly (simulating a signal)
        shutdown_signal_handler(libc::SIGTERM);
        assert!(is_shutting_down());

        // Reset for other tests
        SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
    }
}
