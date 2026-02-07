use chrono::Utc;
use rusqlite::Connection;

use crate::db::agent_runs::{
    find_running_agent_runs, update_agent_run_status, AgentRun, AgentRunStatus,
};
use crate::db::work_items::update_work_item_status;
use crate::db::Result;
use nflow_core::work_item::WorkItemStatus;

/// Result of checking a single stale agent run.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentRecoveryAction {
    /// PID is dead or start time mismatched — mark as failed.
    MarkFailed {
        run_id: uuid::Uuid,
        work_item_id: uuid::Uuid,
    },
    /// PID is alive and start time matches — adopt the process.
    Adopt {
        run_id: uuid::Uuid,
        work_item_id: uuid::Uuid,
        pid: u32,
    },
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
fn is_process_alive(pid: u32) -> bool {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    match nix::sys::signal::kill(nix_pid, None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true, // exists but no permission
        _ => false,
    }
}

/// Read the process start time from /proc/{pid}/stat (field 22, starttime).
///
/// On Linux, /proc/{pid}/stat contains space-separated fields. Field 22 (1-indexed)
/// is the start time in clock ticks since boot.
///
/// Returns None if the process doesn't exist or the file can't be read.
#[cfg(target_os = "linux")]
fn read_process_start_time(pid: u32) -> Option<i64> {
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
fn read_process_start_time(pid: u32) -> Option<i64> {
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
fn read_process_start_time(_pid: u32) -> Option<i64> {
    None
}

/// Verify the state of a process given its PID and expected start time.
pub fn verify_process(pid: u32, expected_start_time: Option<i64>) -> ProcessState {
    if !is_process_alive(pid) {
        return ProcessState::Dead;
    }

    // Process is alive — check start time if we have one recorded
    match expected_start_time {
        Some(expected) => {
            match read_process_start_time(pid) {
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

/// Determine recovery actions for all stale agent runs.
///
/// Queries the DB for running agent_runs and checks each process:
/// - Dead or PID reused → MarkFailed
/// - Alive and start time matches → Adopt
pub fn detect_stale_agents(conn: &Connection) -> Result<Vec<AgentRecoveryAction>> {
    let running = find_running_agent_runs(conn)?;
    let mut actions = Vec::new();

    for run in &running {
        let action = check_agent_run(run);
        actions.push(action);
    }

    Ok(actions)
}

/// Check a single agent run and determine the recovery action.
fn check_agent_run(run: &AgentRun) -> AgentRecoveryAction {
    match run.pid {
        Some(pid) => {
            let state = verify_process(pid, run.pid_start_time);
            match state {
                ProcessState::Alive => AgentRecoveryAction::Adopt {
                    run_id: run.id,
                    work_item_id: run.work_item_id,
                    pid,
                },
                ProcessState::Dead | ProcessState::PidReused => AgentRecoveryAction::MarkFailed {
                    run_id: run.id,
                    work_item_id: run.work_item_id,
                },
            }
        }
        // No PID recorded — can't check, mark as failed
        None => AgentRecoveryAction::MarkFailed {
            run_id: run.id,
            work_item_id: run.work_item_id,
        },
    }
}

/// Execute recovery actions: mark failed agents and update work items.
///
/// Returns the list of adopted agent PIDs (for the daemon to resume reading stdout).
pub fn execute_recovery_actions(
    conn: &Connection,
    actions: &[AgentRecoveryAction],
) -> Result<Vec<(uuid::Uuid, u32)>> {
    let mut adopted = Vec::new();
    let now = Utc::now();

    for action in actions {
        match action {
            AgentRecoveryAction::MarkFailed {
                run_id,
                work_item_id,
            } => {
                update_agent_run_status(
                    conn,
                    run_id,
                    AgentRunStatus::Failed,
                    None,
                    Some("daemon crashed"),
                    Some(now),
                )?;
                update_work_item_status(conn, work_item_id, WorkItemStatus::Failed)?;
            }
            AgentRecoveryAction::Adopt { run_id, pid, .. } => {
                adopted.push((*run_id, *pid));
            }
        }
    }

    Ok(adopted)
}

/// Full crash recovery: detect stale agents and execute recovery actions.
///
/// Returns the list of (run_id, pid) pairs for agents that should be adopted
/// (i.e., the daemon should resume reading their stdout).
pub fn recover_stale_agents(conn: &Connection) -> Result<Vec<(uuid::Uuid, u32)>> {
    let actions = detect_stale_agents(conn)?;
    execute_recovery_actions(conn, &actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::agent_runs::insert_agent_run;
    use crate::db::work_items::get_work_item_by_id;
    use crate::db::{projects::insert_project, test_conn};
    use chrono::Utc;
    use nflow_core::project::{GitProvider, Project};
    use nflow_core::work_item::WorkItem;
    use uuid::Uuid;

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

    fn make_work_item(conn: &Connection, session_id: Uuid) -> WorkItem {
        let epic = WorkItem::new_epic(session_id, "Epic 1".into(), "Desc".into(), "E1".into(), 0);
        crate::db::work_items::insert_work_item(conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session_id,
            "Story".into(),
            "Desc".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        crate::db::work_items::insert_work_item(conn, &story).unwrap();

        let task = WorkItem::new_task(
            story.id,
            session_id,
            "Task".into(),
            "Desc".into(),
            "AC".into(),
            "T1".into(),
            2,
        );
        crate::db::work_items::insert_work_item(conn, &task).unwrap();

        // Set task to in_progress so it can transition to failed
        update_work_item_status(conn, &task.id, WorkItemStatus::InProgress).unwrap();
        task
    }

    fn make_running_agent(
        work_item_id: Uuid,
        pid: Option<u32>,
        start_time: Option<i64>,
    ) -> AgentRun {
        AgentRun {
            id: Uuid::new_v4(),
            work_item_id,
            pid,
            session_id: Some("claude-session-abc".to_string()),
            pid_start_time: start_time,
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: Some("/logs/task.log".to_string()),
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        }
    }

    // --- verify_process ---

    #[test]
    fn test_verify_process_dead_pid() {
        // Use a PID that almost certainly doesn't exist
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
        let actual_start_time = read_process_start_time(pid);
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
    fn test_read_process_start_time_current_process() {
        let pid = std::process::id();
        let start_time = read_process_start_time(pid);
        assert!(start_time.is_some());
        assert!(start_time.unwrap() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_read_process_start_time_nonexistent() {
        let start_time = read_process_start_time(4_000_000_000);
        assert!(start_time.is_none());
    }

    // --- check_agent_run ---

    #[test]
    fn test_check_agent_run_no_pid() {
        let run = AgentRun {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            pid: None,
            session_id: None,
            pid_start_time: None,
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: None,
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        };

        let action = check_agent_run(&run);
        assert!(matches!(action, AgentRecoveryAction::MarkFailed { .. }));
    }

    #[test]
    fn test_check_agent_run_dead_pid() {
        let run = AgentRun {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            pid: Some(4_000_000_000),
            session_id: None,
            pid_start_time: Some(12345),
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: None,
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        };

        let action = check_agent_run(&run);
        assert!(matches!(action, AgentRecoveryAction::MarkFailed { .. }));
    }

    #[test]
    fn test_check_agent_run_alive_pid_no_start_time() {
        let pid = std::process::id();
        let run = AgentRun {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            pid: Some(pid),
            session_id: None,
            pid_start_time: None,
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: None,
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        };

        let action = check_agent_run(&run);
        assert!(matches!(action, AgentRecoveryAction::Adopt { .. }));
    }

    // --- detect_stale_agents ---

    #[test]
    fn test_detect_stale_agents_empty() {
        let conn = test_conn();
        let actions = detect_stale_agents(&conn).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn test_detect_stale_agents_dead_process() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        let run = make_running_agent(task.id, Some(4_000_000_000), Some(12345));
        insert_agent_run(&conn, &run).unwrap();

        let actions = detect_stale_agents(&conn).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], AgentRecoveryAction::MarkFailed { run_id, .. } if *run_id == run.id)
        );
    }

    #[test]
    fn test_detect_stale_agents_alive_process() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        // Use current process PID — it's alive, no start time to mismatch
        let run = make_running_agent(task.id, Some(std::process::id()), None);
        insert_agent_run(&conn, &run).unwrap();

        let actions = detect_stale_agents(&conn).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], AgentRecoveryAction::Adopt { run_id, .. } if *run_id == run.id)
        );
    }

    // --- execute_recovery_actions ---

    #[test]
    fn test_execute_recovery_mark_failed() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        let run = make_running_agent(task.id, Some(4_000_000_000), Some(12345));
        insert_agent_run(&conn, &run).unwrap();

        let actions = vec![AgentRecoveryAction::MarkFailed {
            run_id: run.id,
            work_item_id: task.id,
        }];

        let adopted = execute_recovery_actions(&conn, &actions).unwrap();
        assert!(adopted.is_empty());

        // Agent run should be marked as failed
        let running = find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());

        // Work item should be marked as failed
        let item = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);
    }

    #[test]
    fn test_execute_recovery_adopt() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        let run = make_running_agent(task.id, Some(std::process::id()), None);
        insert_agent_run(&conn, &run).unwrap();

        let actions = vec![AgentRecoveryAction::Adopt {
            run_id: run.id,
            work_item_id: task.id,
            pid: std::process::id(),
        }];

        let adopted = execute_recovery_actions(&conn, &actions).unwrap();
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0], (run.id, std::process::id()));

        // Agent run should still be running (adopted, not failed)
        let running = find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, run.id);
    }

    // --- recover_stale_agents (integration) ---

    #[test]
    fn test_recover_stale_agents_mixed() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);

        // Create two tasks
        let epic = WorkItem::new_epic(sid, "Epic".into(), "Desc".into(), "E1".into(), 0);
        crate::db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            sid,
            "Story".into(),
            "Desc".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        crate::db::work_items::insert_work_item(&conn, &story).unwrap();

        let task1 = WorkItem::new_task(
            story.id,
            sid,
            "Task1".into(),
            "Desc".into(),
            "AC".into(),
            "T1".into(),
            2,
        );
        crate::db::work_items::insert_work_item(&conn, &task1).unwrap();
        update_work_item_status(&conn, &task1.id, WorkItemStatus::InProgress).unwrap();

        let task2 = WorkItem::new_task(
            story.id,
            sid,
            "Task2".into(),
            "Desc".into(),
            "AC".into(),
            "T2".into(),
            3,
        );
        crate::db::work_items::insert_work_item(&conn, &task2).unwrap();
        update_work_item_status(&conn, &task2.id, WorkItemStatus::InProgress).unwrap();

        // Dead agent (PID doesn't exist)
        let dead_run = make_running_agent(task1.id, Some(4_000_000_000), Some(12345));
        insert_agent_run(&conn, &dead_run).unwrap();

        // Alive agent (current process PID, no start time check)
        let alive_run = make_running_agent(task2.id, Some(std::process::id()), None);
        insert_agent_run(&conn, &alive_run).unwrap();

        let adopted = recover_stale_agents(&conn).unwrap();

        // Dead run's task should be failed
        let item1 = get_work_item_by_id(&conn, &task1.id).unwrap().unwrap();
        assert_eq!(item1.status, WorkItemStatus::Failed);

        // Alive run's task should still be in_progress
        let item2 = get_work_item_by_id(&conn, &task2.id).unwrap().unwrap();
        assert_eq!(item2.status, WorkItemStatus::InProgress);

        // One agent should be adopted
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].0, alive_run.id);
    }

    #[test]
    fn test_recover_no_stale_agents() {
        let conn = test_conn();
        let adopted = recover_stale_agents(&conn).unwrap();
        assert!(adopted.is_empty());
    }

    #[test]
    fn test_recover_agent_no_pid_marked_failed() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        // Agent with no PID recorded
        let run = make_running_agent(task.id, None, None);
        insert_agent_run(&conn, &run).unwrap();

        let adopted = recover_stale_agents(&conn).unwrap();
        assert!(adopted.is_empty());

        // Agent should be marked as failed
        let running = find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());

        // Work item should be marked as failed
        let item = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);
    }
}
