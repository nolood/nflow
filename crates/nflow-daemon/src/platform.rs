//! Platform-abstracted process management.
//!
//! Provides cross-platform functions for:
//! - Checking process liveness and verifying process identity
//! - Sending signals to processes (SIGTERM, SIGKILL, SIGHUP)
//! - Installing signal handlers for daemon shutdown
//!
//! Uses compile-time platform selection via `#[cfg(target_os)]`.
//! All functions work identically on Linux and macOS (both POSIX).

use std::sync::atomic::{AtomicBool, Ordering};

/// Cross-platform signal abstraction.
///
/// Wraps the subset of POSIX signals used by nflow for process management
/// and daemon lifecycle. Works identically on Linux and macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Graceful termination request.
    Sigterm,
    /// Immediate, unblockable kill.
    Sigkill,
    /// Hangup — used during daemonization.
    Sighup,
    /// Interrupt (Ctrl+C) — used for foreground daemon shutdown.
    Sigint,
}

impl Signal {
    /// Convert to the nix Signal type.
    fn to_nix(self) -> nix::sys::signal::Signal {
        match self {
            Signal::Sigterm => nix::sys::signal::Signal::SIGTERM,
            Signal::Sigkill => nix::sys::signal::Signal::SIGKILL,
            Signal::Sighup => nix::sys::signal::Signal::SIGHUP,
            Signal::Sigint => nix::sys::signal::Signal::SIGINT,
        }
    }
}

/// Result of sending a signal to a process.
#[derive(Debug, PartialEq, Eq)]
pub enum SendSignalResult {
    /// Signal was delivered successfully.
    Sent,
    /// Process does not exist (ESRCH).
    NoSuchProcess,
    /// Permission denied (EPERM).
    PermissionDenied,
}

/// Send a signal to a process by PID.
///
/// Wraps `nix::sys::signal::kill` with a clean return type that
/// distinguishes between success, no-such-process, and permission errors.
pub fn send_signal(pid: u32, signal: Signal) -> SendSignalResult {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    match nix::sys::signal::kill(nix_pid, signal.to_nix()) {
        Ok(()) => SendSignalResult::Sent,
        Err(nix::errno::Errno::ESRCH) => SendSignalResult::NoSuchProcess,
        Err(nix::errno::Errno::EPERM) => SendSignalResult::PermissionDenied,
        Err(_) => SendSignalResult::NoSuchProcess,
    }
}

/// Install a signal handler that sets an `AtomicBool` flag when triggered.
///
/// This is the standard pattern for daemon shutdown: install handlers for
/// SIGTERM and SIGINT that set a shared flag, then check the flag in the
/// main loop.
///
/// The handler is async-signal-safe (only writes to an AtomicBool).
///
/// # Safety
/// Uses `nix::sys::signal::signal` to install a C-level signal handler.
/// The handler function only performs atomic store operations, which are
/// async-signal-safe.
pub fn install_signal_handler(signal: Signal) -> std::io::Result<()> {
    // SAFETY: Our handler only writes to an AtomicBool (async-signal-safe)
    unsafe {
        nix::sys::signal::signal(
            signal.to_nix(),
            nix::sys::signal::SigHandler::Handler(signal_handler),
        )
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to install {:?} handler: {}", signal, e),
            )
        })?;
    }
    Ok(())
}

/// Global shutdown flag for signal handler access.
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// Signal handler for SIGTERM/SIGINT/SIGHUP. Sets the global shutdown flag.
///
/// This function is called from signal context, so it must only perform
/// async-signal-safe operations. Writing to an AtomicBool is safe.
extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
}

/// Returns true if a shutdown signal has been received.
pub fn is_shutting_down() -> bool {
    SHUTDOWN_FLAG.load(Ordering::SeqCst)
}

/// Reset the shutdown flag (for testing).
#[cfg(test)]
pub fn reset_shutdown_flag() {
    SHUTDOWN_FLAG.store(false, Ordering::SeqCst);
}

/// Ignore a signal (set its handler to SIG_IGN).
///
/// Returns the previous handler so it can be restored later.
/// Used during daemonization to ignore SIGHUP before `setsid()`.
///
/// # Safety
/// Uses `nix::sys::signal::signal` to change the signal disposition.
pub fn ignore_signal(signal: Signal) -> std::io::Result<nix::sys::signal::SigHandler> {
    // SAFETY: SigIgn is a valid signal handler
    unsafe {
        nix::sys::signal::signal(signal.to_nix(), nix::sys::signal::SigHandler::SigIgn).map_err(
            |e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to ignore {:?}: {}", signal, e),
                )
            },
        )
    }
}

/// Restore a previous signal handler.
///
/// # Safety
/// Uses `nix::sys::signal::signal` to restore a previously saved handler.
pub fn restore_signal(
    signal: Signal,
    handler: nix::sys::signal::SigHandler,
) -> std::io::Result<()> {
    // SAFETY: Restoring the previous signal handler
    unsafe {
        nix::sys::signal::signal(signal.to_nix(), handler).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to restore {:?} handler: {}", signal, e),
            )
        })?;
    }
    Ok(())
}

/// State of a process after verification.
#[derive(Debug, PartialEq, Eq)]
pub enum ProcessState {
    /// Process is alive and start time matches the recorded value.
    Alive,
    /// Process is dead (no such process).
    Dead,
    /// PID exists but start time doesn't match (PID was reused by a different process).
    PidReused,
}

/// Check if a process is alive by sending signal 0.
///
/// Returns `true` if the process exists (even without permission to signal it).
pub fn is_process_alive(pid: u32) -> bool {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    matches!(
        nix::sys::signal::kill(nix_pid, None),
        Ok(()) | Err(nix::errno::Errno::EPERM)
    )
}

/// Read the process start time from /proc/{pid}/stat (field 22, starttime).
///
/// On Linux, /proc/{pid}/stat contains space-separated fields. Field 22 (1-indexed)
/// is the start time in clock ticks since boot.
///
/// Returns None if the process doesn't exist or the file can't be read.
#[cfg(target_os = "linux")]
pub fn get_pid_start_time(pid: u32) -> Option<i64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(&stat_path).ok()?;

    // The comm field (field 2) can contain spaces and parentheses,
    // so we find the last ')' to skip past it.
    let after_comm = content.rfind(')')? + 1;
    let rest = &content[after_comm..];

    // Fields after comm start at field 3. Field 22 is starttime,
    // which is at index 22 - 3 = 19 in the remaining fields.
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 20 {
        return None;
    }
    // fields[0] = state (field 3), fields[19] = starttime (field 22)
    fields[19].parse::<i64>().ok()
}

/// Read the process start time on macOS using sysctl.
///
/// Returns None if the process doesn't exist or info can't be read.
#[cfg(target_os = "macos")]
pub fn get_pid_start_time(pid: u32) -> Option<i64> {
    use std::mem;

    // Use sysctl kern.proc.pid.{pid} to get process info
    let mut mib: [libc::c_int; 4] = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    let mut info: libc::kinfo_proc = unsafe { mem::zeroed() };
    let mut size = mem::size_of::<libc::kinfo_proc>();

    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            &mut info as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };

    if ret != 0 || size == 0 {
        return None;
    }

    // p_starttime is a timeval struct — return tv_sec as the start time
    Some(info.kp_proc.p_starttime.tv_sec as i64)
}

/// Fallback for unsupported platforms — always returns None.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn get_pid_start_time(_pid: u32) -> Option<i64> {
    None
}

/// Verify the state of a process given its PID and expected start time.
///
/// Returns:
/// - `ProcessState::Dead` if the process doesn't exist
/// - `ProcessState::PidReused` if the PID exists but start time doesn't match
/// - `ProcessState::Alive` if the process is alive and start time matches (or can't be verified)
pub fn verify_process(pid: u32, expected_start_time: Option<i64>) -> ProcessState {
    if !is_process_alive(pid) {
        return ProcessState::Dead;
    }

    // Process is alive — check start time if we have one recorded
    match expected_start_time {
        Some(expected) => {
            match get_pid_start_time(pid) {
                Some(actual) if actual == expected => ProcessState::Alive,
                Some(_) => ProcessState::PidReused,
                // Can't read start time — assume alive (conservative)
                None => ProcessState::Alive,
            }
        }
        // No recorded start time — process is alive, can't verify identity
        None => ProcessState::Alive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current() {
        let pid = std::process::id();
        assert!(is_process_alive(pid));
    }

    #[test]
    fn test_is_process_alive_dead() {
        assert!(!is_process_alive(4_000_000_000));
    }

    #[test]
    fn test_verify_process_dead_pid() {
        let state = verify_process(4_000_000_000, Some(12345));
        assert_eq!(state, ProcessState::Dead);
    }

    #[test]
    fn test_verify_process_alive_current_process() {
        let pid = std::process::id();
        // Without a start time to compare, should return Alive
        let state = verify_process(pid, None);
        assert_eq!(state, ProcessState::Alive);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_verify_process_alive_matching_start_time() {
        let pid = std::process::id();
        let actual_start_time = get_pid_start_time(pid);
        assert!(
            actual_start_time.is_some(),
            "should read own process start time"
        );

        let state = verify_process(pid, actual_start_time);
        assert_eq!(state, ProcessState::Alive);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_verify_process_pid_reused() {
        let pid = std::process::id();
        // Use a bogus start time that definitely won't match
        let state = verify_process(pid, Some(-999));
        assert_eq!(state, ProcessState::PidReused);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_get_pid_start_time_current_process() {
        let pid = std::process::id();
        let start_time = get_pid_start_time(pid);
        assert!(start_time.is_some());
        assert!(start_time.unwrap() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_get_pid_start_time_nonexistent() {
        let start_time = get_pid_start_time(4_000_000_000);
        assert!(start_time.is_none());
    }

    #[test]
    fn test_signal_to_nix() {
        assert_eq!(Signal::Sigterm.to_nix(), nix::sys::signal::Signal::SIGTERM);
        assert_eq!(Signal::Sigkill.to_nix(), nix::sys::signal::Signal::SIGKILL);
        assert_eq!(Signal::Sighup.to_nix(), nix::sys::signal::Signal::SIGHUP);
    }

    #[test]
    fn test_send_signal_dead_pid() {
        let result = send_signal(4_000_000_000, Signal::Sigterm);
        assert_eq!(result, SendSignalResult::NoSuchProcess);
    }

    #[test]
    fn test_send_signal_sigterm_alive_process() {
        let child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let pid = child.id();

        let result = send_signal(pid, Signal::Sigterm);
        assert_eq!(result, SendSignalResult::Sent);

        // Reap the child
        let mut child = child;
        let _ = child.wait();
    }

    #[test]
    fn test_send_signal_sigkill_alive_process() {
        let child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let pid = child.id();

        let result = send_signal(pid, Signal::Sigkill);
        assert_eq!(result, SendSignalResult::Sent);

        // Reap the child
        let mut child = child;
        let _ = child.wait();
    }

    #[test]
    fn test_send_signal_result_debug() {
        assert_eq!(format!("{:?}", SendSignalResult::Sent), "Sent");
        assert_eq!(
            format!("{:?}", SendSignalResult::NoSuchProcess),
            "NoSuchProcess"
        );
        assert_eq!(
            format!("{:?}", SendSignalResult::PermissionDenied),
            "PermissionDenied"
        );
    }

    /// Tests install_signal_handler, is_shutting_down, and reset in one
    /// sequential test to avoid parallel race conditions on global state.
    #[test]
    fn test_signal_handler_and_shutdown_flag() {
        // 1. Reset flag
        reset_shutdown_flag();
        assert!(!is_shutting_down());

        // 2. Install handler for SIGTERM
        install_signal_handler(Signal::Sigterm).expect("failed to install SIGTERM handler");

        // 3. Directly call the handler function (simulating a signal)
        signal_handler(libc::SIGTERM);
        assert!(is_shutting_down());

        // 4. Reset again
        reset_shutdown_flag();
        assert!(!is_shutting_down());
    }

    #[test]
    fn test_ignore_and_restore_signal() {
        // Ignore SIGHUP
        let prev = ignore_signal(Signal::Sighup).expect("failed to ignore SIGHUP");

        // Restore it
        restore_signal(Signal::Sighup, prev).expect("failed to restore SIGHUP");
    }

    #[test]
    fn test_signal_clone_copy() {
        let s = Signal::Sigterm;
        let s2 = s;
        let s3 = s.clone();
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }
}
