use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use nix::unistd::setsid;
use tracing::warn;

use crate::error::{DaemonError, Result};
use crate::platform;

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

/// Returns the path to the database file (~/.nflow/nflow.db).
pub fn db_path() -> Result<PathBuf> {
    Ok(nflow_home()?.join("nflow.db"))
}

/// Ensures the ~/.nflow/logs/ directory exists with secure permissions (0700).
pub fn ensure_log_dir() -> Result<()> {
    let log_dir = nflow_home()?.join("logs");
    if !log_dir.exists() {
        fs::create_dir_all(&log_dir)?;
        fs::set_permissions(&log_dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Ensures the ~/.nflow/ directory exists with secure permissions (0700).
///
/// If the directory exists but has permissions more open than 0700,
/// the permissions are tightened and a warning is logged.
pub fn ensure_nflow_home() -> Result<()> {
    let home = nflow_home()?;
    if home.exists() {
        enforce_dir_permissions(&home)?;
    } else {
        fs::create_dir_all(&home)?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Checks and enforces that a directory has mode 0700 (owner-only).
///
/// If the directory's group or other bits are set (mode & 0o077 != 0),
/// this function fixes the permissions to 0700 and logs a warning.
fn enforce_dir_permissions(path: &Path) -> Result<()> {
    let meta = fs::metadata(path)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        warn!(
            path = %path.display(),
            current_mode = format!("{:04o}", mode),
            "nflow home directory has too-open permissions, fixing to 0700"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
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
    let prev_handler =
        platform::ignore_signal(platform::Signal::Sighup).map_err(DaemonError::Io)?;

    // Create a new session — detach from controlling terminal
    setsid().map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("setsid failed: {}", e),
        ))
    })?;

    // Restore previous SIGHUP handler
    platform::restore_signal(platform::Signal::Sighup, prev_handler).map_err(DaemonError::Io)?;

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
    install_shutdown_handler()?;

    Ok(shutdown)
}

/// Installs SIGTERM and SIGINT handlers that set the shutdown flag.
///
/// Both signals trigger the same graceful shutdown: they set the
/// global shutdown flag to `true` via `platform::install_signal_handler`,
/// allowing the main loop to detect and initiate an orderly shutdown.
fn install_shutdown_handler() -> Result<()> {
    platform::install_signal_handler(platform::Signal::Sigterm).map_err(DaemonError::Io)?;

    // Install SIGINT handler (Ctrl+C) — same shutdown behavior
    platform::install_signal_handler(platform::Signal::Sigint).map_err(DaemonError::Io)?;

    Ok(())
}

/// Returns true if a shutdown signal has been received.
pub fn is_shutting_down() -> bool {
    platform::is_shutting_down()
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::Ordering;

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

    #[test]
    fn test_enforce_dir_permissions_fixes_too_open() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("test_perms");
        fs::create_dir(&dir).unwrap();

        // Set permissions to 0755 (too open — group/other have read+execute)
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        // enforce_dir_permissions should fix to 0700
        enforce_dir_permissions(&dir).unwrap();

        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn test_enforce_dir_permissions_leaves_correct_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("test_correct");
        fs::create_dir(&dir).unwrap();

        // Set permissions to 0700 (correct)
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        // enforce_dir_permissions should not change anything
        enforce_dir_permissions(&dir).unwrap();

        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn test_ensure_nflow_home_creates_with_secure_permissions() {
        // ensure_nflow_home creates ~/.nflow/ — on this system it should exist with 0700
        ensure_nflow_home().unwrap();
        let home = nflow_home().unwrap();
        assert!(home.exists());
        let mode = fs::metadata(&home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, ".nflow dir should be 0700");
    }

    /// Tests write_pid_file and direct PID file reads.
    #[test]
    fn test_write_pid_file() {
        // Clean up first
        let pid_path = pid_file_path().unwrap();
        let _ = fs::remove_file(&pid_path);

        // Write PID file
        write_pid_file().unwrap();

        // Read back directly
        let content = fs::read_to_string(&pid_path).unwrap();
        let pid: u32 = content.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());

        // Cleanup
        let _ = fs::remove_file(&pid_path);
    }

    #[test]
    fn test_foreground_mode_writes_pid_file_and_sets_shutdown() {
        // Clean up any existing PID file first
        let pid_path = pid_file_path().unwrap();
        let _ = fs::remove_file(&pid_path);

        // Start foreground mode
        let shutdown = foreground_mode().unwrap();

        // PID file should be written
        let content = fs::read_to_string(&pid_path).unwrap();
        let pid: u32 = content.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());

        // Shutdown flag should be false initially
        assert!(!shutdown.load(Ordering::SeqCst));
        assert!(!is_shutting_down());

        // Cleanup
        platform::reset_shutdown_flag();
        let _ = fs::remove_file(&pid_path);
    }

    #[test]
    fn test_is_shutting_down_delegates_to_platform() {
        platform::reset_shutdown_flag();
        assert!(!is_shutting_down());
    }
}
