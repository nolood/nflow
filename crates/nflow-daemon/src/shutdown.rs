use std::path::Path;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tracing::info;

use crate::db::agent_runs::{find_running_agent_runs, AgentRun};
use crate::recovery::is_process_alive;

/// Default grace period for agents to finish naturally after shutdown signal.
const AGENT_GRACE_PERIOD: Duration = Duration::from_secs(60);

/// Time to wait after SIGTERM before escalating to SIGKILL.
const SIGTERM_TO_SIGKILL_WAIT: Duration = Duration::from_secs(10);

/// Poll interval for checking agent status during shutdown.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Outcome of the graceful shutdown process.
#[derive(Debug)]
pub struct ShutdownReport {
    /// Number of agents that finished naturally within the grace period.
    pub agents_finished_naturally: u32,
    /// Number of agents terminated via SIGTERM.
    pub agents_sigtermed: u32,
    /// Number of agents killed via SIGKILL.
    pub agents_sigkilled: u32,
    /// Whether the socket file was removed.
    pub socket_removed: bool,
    /// Whether the PID file was removed.
    pub pid_file_removed: bool,
}

/// Collect PIDs of all currently running agent processes from the database.
///
/// Returns a list of (run_id, pid) pairs for agents with recorded PIDs.
pub fn collect_running_agent_pids(
    conn: &rusqlite::Connection,
) -> crate::db::Result<Vec<(uuid::Uuid, u32)>> {
    let runs = find_running_agent_runs(conn)?;
    Ok(runs
        .iter()
        .filter_map(|r: &AgentRun| r.pid.map(|pid| (r.id, pid)))
        .collect())
}

/// Wait for all agent processes to exit, up to the specified timeout.
///
/// Returns the number of agents that finished within the timeout and
/// the PIDs of agents still alive after the timeout.
pub fn wait_for_agents(pids: &[(uuid::Uuid, u32)], timeout: Duration) -> (u32, Vec<u32>) {
    if pids.is_empty() {
        return (0, Vec::new());
    }

    let start = Instant::now();
    let mut alive: Vec<u32> = pids.iter().map(|(_, pid)| *pid).collect();
    let mut finished = 0u32;

    while !alive.is_empty() && start.elapsed() < timeout {
        alive.retain(|&pid| {
            if is_process_alive(pid) {
                true
            } else {
                finished += 1;
                false
            }
        });
        if !alive.is_empty() {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    (finished, alive)
}

/// Send SIGTERM to a list of agent PIDs.
///
/// Returns the number of processes that were successfully signaled.
pub fn sigterm_agents(pids: &[u32]) -> u32 {
    let mut signaled = 0u32;
    for &pid in pids {
        let nix_pid = Pid::from_raw(pid as i32);
        match kill(nix_pid, Signal::SIGTERM) {
            Ok(()) => {
                info!(pid, "sent SIGTERM to agent process");
                signaled += 1;
            }
            Err(nix::errno::Errno::ESRCH) => {
                // Process already dead — that's fine
            }
            Err(e) => {
                info!(pid, error = %e, "failed to send SIGTERM to agent process");
            }
        }
    }
    signaled
}

/// Send SIGKILL to a list of agent PIDs.
///
/// Returns the number of processes that were successfully signaled.
pub fn sigkill_agents(pids: &[u32]) -> u32 {
    let mut killed = 0u32;
    for &pid in pids {
        let nix_pid = Pid::from_raw(pid as i32);
        match kill(nix_pid, Signal::SIGKILL) {
            Ok(()) => {
                info!(pid, "sent SIGKILL to agent process");
                killed += 1;
            }
            Err(nix::errno::Errno::ESRCH) => {
                // Process already dead
            }
            Err(e) => {
                info!(pid, error = %e, "failed to send SIGKILL to agent process");
            }
        }
    }
    killed
}

/// Remove the Unix socket file if it exists.
///
/// Returns true if the file was removed.
pub fn remove_socket_file(socket_path: &Path) -> bool {
    match std::fs::remove_file(socket_path) {
        Ok(()) => {
            info!(path = %socket_path.display(), "removed socket file");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            info!(path = %socket_path.display(), error = %e, "failed to remove socket file");
            false
        }
    }
}

/// Remove the PID file if it exists.
///
/// Returns true if the file was removed.
pub fn remove_pid_file(pid_file_path: &Path) -> bool {
    match std::fs::remove_file(pid_file_path) {
        Ok(()) => {
            info!(path = %pid_file_path.display(), "removed PID file");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            info!(path = %pid_file_path.display(), error = %e, "failed to remove PID file");
            false
        }
    }
}

/// Execute the full graceful shutdown sequence.
///
/// This is the main entry point for daemon shutdown. It:
/// 1. Collects running agent PIDs from the database
/// 2. Waits up to 60 seconds for agents to finish naturally
/// 3. Sends SIGTERM to remaining agents, waits 10 more seconds
/// 4. Sends SIGKILL to any survivors
/// 5. Removes socket file and PID file
///
/// The caller is responsible for:
/// - Setting the shutdown flag (stop scheduler, reject new connections)
/// - Closing the database connection after this returns
pub fn graceful_shutdown(
    conn: &rusqlite::Connection,
    socket_path: &Path,
    pid_file_path: &Path,
) -> ShutdownReport {
    info!("beginning graceful shutdown sequence...");

    // 1. Collect running agent PIDs
    let agent_pids = collect_running_agent_pids(conn).unwrap_or_default();
    let total_agents = agent_pids.len();
    info!(count = total_agents, "running agent processes at shutdown");

    // 2. Wait for agents to finish naturally (up to 60s)
    let (finished_naturally, still_alive) = wait_for_agents(&agent_pids, AGENT_GRACE_PERIOD);
    if finished_naturally > 0 {
        info!(
            count = finished_naturally,
            "agent processes finished naturally during grace period"
        );
    }

    // 3. Send SIGTERM to remaining agents
    let mut sigtermed = 0u32;
    let mut sigkilled = 0u32;

    if !still_alive.is_empty() {
        info!(
            count = still_alive.len(),
            "agent processes still alive after grace period, sending SIGTERM"
        );
        sigterm_agents(&still_alive);

        // 4. Wait 10 more seconds, then SIGKILL survivors
        let (finished_after_term, survivors) =
            wait_for_agents_by_pid(&still_alive, SIGTERM_TO_SIGKILL_WAIT);
        sigtermed = finished_after_term;

        if !survivors.is_empty() {
            info!(
                count = survivors.len(),
                "agent processes survived SIGTERM, sending SIGKILL"
            );
            sigkilled = sigkill_agents(&survivors);
        }
    }

    // 5. Remove socket and PID files
    let socket_removed = remove_socket_file(socket_path);
    let pid_removed = remove_pid_file(pid_file_path);

    info!("graceful shutdown complete");
    ShutdownReport {
        agents_finished_naturally: finished_naturally,
        agents_sigtermed: sigtermed,
        agents_sigkilled: sigkilled,
        socket_removed,
        pid_file_removed: pid_removed,
    }
}

/// Wait for a list of PIDs to exit, up to the specified timeout.
///
/// Unlike `wait_for_agents`, this takes bare PIDs (no run IDs needed).
fn wait_for_agents_by_pid(pids: &[u32], timeout: Duration) -> (u32, Vec<u32>) {
    if pids.is_empty() {
        return (0, Vec::new());
    }

    let start = Instant::now();
    let mut alive: Vec<u32> = pids.to_vec();
    let mut finished = 0u32;

    while !alive.is_empty() && start.elapsed() < timeout {
        alive.retain(|&pid| {
            if is_process_alive(pid) {
                true
            } else {
                finished += 1;
                false
            }
        });
        if !alive.is_empty() {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    (finished, alive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Reap a child process after signaling it so it doesn't remain a zombie.
    /// Uses waitpid with WNOHANG in a loop to collect the zombie.
    fn reap_child(mut child: std::process::Child) {
        // try_wait reaps the zombie via the Rust Child handle
        for _ in 0..40 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return,
            }
        }
        // Force kill if still around
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_wait_for_agents_empty() {
        let (finished, alive) = wait_for_agents(&[], Duration::from_secs(1));
        assert_eq!(finished, 0);
        assert!(alive.is_empty());
    }

    #[test]
    fn test_wait_for_agents_dead_pid() {
        // Use a PID that doesn't exist
        let pids = vec![(uuid::Uuid::new_v4(), 4_000_000_000u32)];
        let (finished, alive) = wait_for_agents(&pids, Duration::from_secs(5));
        assert_eq!(finished, 1);
        assert!(alive.is_empty());
    }

    #[test]
    fn test_wait_for_agents_timeout() {
        // Spawn a process that sleeps for a long time
        let mut child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let pid = child.id();

        let pids = vec![(uuid::Uuid::new_v4(), pid)];
        // Very short timeout — process won't finish
        let (finished, alive) = wait_for_agents(&pids, Duration::from_millis(200));
        assert_eq!(finished, 0);
        assert_eq!(alive.len(), 1);
        assert_eq!(alive[0], pid);

        // Clean up
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_wait_for_agents_process_exits() {
        // Spawn a short-lived process, then wait for it to exit first
        let mut child = Command::new("true").spawn().expect("failed to spawn true");
        let pid = child.id();

        // Reap the child via wait() so it's fully dead (not zombie)
        let _ = child.wait();

        let pids = vec![(uuid::Uuid::new_v4(), pid)];
        let (finished, alive) = wait_for_agents(&pids, Duration::from_secs(5));
        assert_eq!(finished, 1);
        assert!(alive.is_empty());
    }

    #[test]
    fn test_sigterm_agents_dead_pid() {
        // SIGTERM to a dead PID should not error
        let signaled = sigterm_agents(&[4_000_000_000]);
        assert_eq!(signaled, 0);
    }

    #[test]
    fn test_sigterm_agents_alive_process() {
        let child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let pid = child.id();

        let signaled = sigterm_agents(&[pid]);
        assert_eq!(signaled, 1);

        // Reap the child — this also confirms it died from SIGTERM
        reap_child(child);
    }

    #[test]
    fn test_sigkill_agents_dead_pid() {
        let killed = sigkill_agents(&[4_000_000_000]);
        assert_eq!(killed, 0);
    }

    #[test]
    fn test_sigkill_agents_alive_process() {
        // Spawn a process that traps SIGTERM (only SIGKILL can kill it)
        let child = Command::new("bash")
            .arg("-c")
            .arg("trap '' TERM; sleep 300")
            .spawn()
            .expect("failed to spawn bash");
        let pid = child.id();

        let killed = sigkill_agents(&[pid]);
        assert_eq!(killed, 1);

        // Reap the child — confirms it died from SIGKILL
        reap_child(child);
    }

    #[test]
    fn test_remove_socket_file_exists() {
        let dir =
            std::env::temp_dir().join(format!("nflow_test_shutdown_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("nflow.sock");
        std::fs::write(&sock, "").unwrap();

        let removed = remove_socket_file(&sock);
        assert!(removed);
        assert!(!sock.exists());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_remove_socket_file_not_exists() {
        let sock = std::env::temp_dir().join("nflow_nonexistent_shutdown.sock");
        let removed = remove_socket_file(&sock);
        assert!(!removed);
    }

    #[test]
    fn test_remove_pid_file_exists() {
        let dir =
            std::env::temp_dir().join(format!("nflow_test_shutdown_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid = dir.join("daemon.pid");
        std::fs::write(&pid, "12345").unwrap();

        let removed = remove_pid_file(&pid);
        assert!(removed);
        assert!(!pid.exists());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_remove_pid_file_not_exists() {
        let pid = std::env::temp_dir().join("nflow_nonexistent_shutdown.pid");
        let removed = remove_pid_file(&pid);
        assert!(!removed);
    }

    #[test]
    fn test_sigterm_then_sigkill_escalation() {
        // Spawn a process that traps SIGTERM (requires SIGKILL to die)
        let child = Command::new("bash")
            .arg("-c")
            .arg("trap '' TERM; sleep 300")
            .spawn()
            .expect("failed to spawn bash");
        let pid = child.id();

        // Give the trap handler time to be installed
        std::thread::sleep(Duration::from_millis(500));

        // SIGTERM won't kill it (trap ignores TERM)
        sigterm_agents(&[pid]);
        std::thread::sleep(Duration::from_secs(1));
        // Process should still be alive (trap catches TERM)
        assert!(is_process_alive(pid));

        // SIGKILL will kill it
        sigkill_agents(&[pid]);
        reap_child(child);
    }

    #[test]
    fn test_wait_for_agents_by_pid_empty() {
        let (finished, alive) = wait_for_agents_by_pid(&[], Duration::from_secs(1));
        assert_eq!(finished, 0);
        assert!(alive.is_empty());
    }

    #[test]
    fn test_collect_running_agent_pids_empty() {
        let conn = crate::db::test_conn();
        let pids = collect_running_agent_pids(&conn).unwrap();
        assert!(pids.is_empty());
    }

    #[test]
    fn test_graceful_shutdown_no_agents() {
        let conn = crate::db::test_conn();
        let dir =
            std::env::temp_dir().join(format!("nflow_test_shutdown_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("nflow.sock");
        let pid = dir.join("daemon.pid");

        // Create socket and PID files
        std::fs::write(&sock, "").unwrap();
        std::fs::write(&pid, "12345").unwrap();

        let report = graceful_shutdown(&conn, &sock, &pid);
        assert_eq!(report.agents_finished_naturally, 0);
        assert_eq!(report.agents_sigtermed, 0);
        assert_eq!(report.agents_sigkilled, 0);
        assert!(report.socket_removed);
        assert!(report.pid_file_removed);
        assert!(!sock.exists());
        assert!(!pid.exists());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_shutdown_report_debug() {
        let report = ShutdownReport {
            agents_finished_naturally: 2,
            agents_sigtermed: 1,
            agents_sigkilled: 0,
            socket_removed: true,
            pid_file_removed: true,
        };
        // Verify Debug impl works
        let debug_str = format!("{:?}", report);
        assert!(debug_str.contains("agents_finished_naturally: 2"));
    }
}
