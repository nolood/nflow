//! Platform-abstracted PID detection for crash recovery.
//!
//! Provides cross-platform functions for checking process liveness,
//! reading process start times, and verifying process identity.
//! Uses compile-time platform selection via `#[cfg(target_os)]`.

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
}
