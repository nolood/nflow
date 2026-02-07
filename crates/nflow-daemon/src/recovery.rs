use std::path::Path;

use chrono::Utc;
use rusqlite::{params, Connection};
use tracing::info;
use uuid::Uuid;

use crate::db::agent_runs::{
    find_running_agent_runs, update_agent_run_status, AgentRun, AgentRunStatus,
};
use crate::db::work_items::update_work_item_status;
use crate::db::Result;
use crate::platform::{self, ProcessState};
use nflow_core::work_item::WorkItemStatus;

// Re-export platform functions for backwards compatibility with existing consumers.
pub use crate::platform::{get_pid_start_time, is_process_alive, verify_process};

/// Alias for backwards compatibility — prefer `get_pid_start_time`.
pub fn read_process_start_time(pid: u32) -> Option<i64> {
    platform::get_pid_start_time(pid)
}

/// Result of checking a single stale agent run.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentRecoveryAction {
    /// PID is dead or start time mismatched — mark as failed.
    MarkFailed { run_id: Uuid, work_item_id: Uuid },
    /// PID is alive and start time matches — adopt the process.
    Adopt {
        run_id: Uuid,
        work_item_id: Uuid,
        pid: u32,
    },
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
) -> Result<Vec<(Uuid, u32)>> {
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
pub fn recover_stale_agents(conn: &Connection) -> Result<Vec<(Uuid, u32)>> {
    let actions = detect_stale_agents(conn)?;
    execute_recovery_actions(conn, &actions)
}

// --- Session and state recovery (US-038) ---

/// Reset all specs with session_active = 1 to session_active = 0 across all projects.
///
/// Returns the number of specs reset.
pub fn reset_all_active_spec_sessions(conn: &Connection) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE specs SET session_active = 0, updated_at = ?1 WHERE session_active = 1",
        params![Utc::now().to_rfc3339()],
    )?;
    if changed > 0 {
        info!(count = changed, "reset active spec sessions to inactive");
    }
    Ok(changed as u64)
}

/// Find work items in in_progress state that have no running agent_run.
///
/// These are orphaned tasks — the daemon crashed while they were being executed
/// and no agent process is running for them anymore.
pub fn find_orphaned_in_progress_items(conn: &Connection) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT w.id FROM work_items w
         WHERE w.status = 'in_progress'
           AND w.item_type = 'task'
           AND NOT EXISTS (
             SELECT 1 FROM agent_runs a
             WHERE a.work_item_id = w.id AND a.status = 'running'
           )",
    )?;
    let rows = stmt.query_map([], |row| {
        let id_str: String = row.get(0)?;
        Ok(Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

/// Mark orphaned in-progress work items as failed.
///
/// Returns the number of items marked as failed.
pub fn fail_orphaned_in_progress_items(conn: &Connection) -> Result<u64> {
    let orphaned = find_orphaned_in_progress_items(conn)?;
    for id in &orphaned {
        update_work_item_status(conn, id, WorkItemStatus::Failed)?;
        info!(work_item_id = %id, "marked orphaned in-progress task as failed");
    }
    Ok(orphaned.len() as u64)
}

/// Find in-progress stories where all child tasks are done.
///
/// These are stories that completed while the daemon was down and need
/// completion flow triggered (rebase/push/MR).
pub fn find_completed_in_progress_stories(conn: &Connection) -> Result<Vec<Uuid>> {
    // Stories that are in_progress, have at least one child task,
    // and all child tasks are either done or cancelled.
    let mut stmt = conn.prepare(
        "SELECT w.id FROM work_items w
         WHERE w.status = 'in_progress'
           AND w.item_type = 'story'
           AND EXISTS (
             SELECT 1 FROM work_items c WHERE c.parent_id = w.id AND c.item_type = 'task'
           )
           AND NOT EXISTS (
             SELECT 1 FROM work_items c
             WHERE c.parent_id = w.id
               AND c.item_type = 'task'
               AND c.status NOT IN ('done', 'cancelled')
           )",
    )?;
    let rows = stmt.query_map([], |row| {
        let id_str: String = row.get(0)?;
        Ok(Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()))
    })?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

/// Remove a stale Unix socket file if it exists.
///
/// Returns true if a socket was removed, false if none existed.
pub fn remove_stale_socket(socket_path: &Path) -> bool {
    if socket_path.exists() {
        match std::fs::remove_file(socket_path) {
            Ok(()) => {
                info!(path = %socket_path.display(), "removed stale socket file");
                true
            }
            Err(e) => {
                info!(path = %socket_path.display(), error = %e, "failed to remove stale socket file");
                false
            }
        }
    } else {
        false
    }
}

/// Remove a stale PID file if the recorded PID doesn't match the current process.
///
/// Returns true if a stale PID file was removed, false otherwise.
pub fn cleanup_stale_pid_file(pid_file: &Path) -> bool {
    let content = match std::fs::read_to_string(pid_file) {
        Ok(c) => c,
        Err(_) => return false, // File doesn't exist or can't be read
    };

    let recorded_pid = match content.trim().parse::<u32>() {
        Ok(p) => p,
        Err(_) => {
            // Invalid PID file — remove it
            let _ = std::fs::remove_file(pid_file);
            info!(path = %pid_file.display(), "removed PID file with invalid content");
            return true;
        }
    };

    let current_pid = std::process::id();
    if recorded_pid != current_pid {
        // Check if the recorded PID is still alive
        if !is_process_alive(recorded_pid) {
            let _ = std::fs::remove_file(pid_file);
            info!(
                path = %pid_file.display(),
                recorded_pid,
                current_pid,
                "removed stale PID file (process dead)"
            );
            return true;
        }
    }
    false
}

/// Summary of all session/state recovery actions taken.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Number of spec sessions reset from active to inactive.
    pub specs_reset: u64,
    /// Number of orphaned in-progress tasks marked as failed.
    pub tasks_failed: u64,
    /// Story IDs that have all tasks done and need completion flow.
    pub stories_needing_completion: Vec<Uuid>,
    /// Number of stale agent runs handled (from recover_stale_agents).
    pub agents_adopted: Vec<(Uuid, u32)>,
    /// Whether a stale socket was removed.
    pub socket_removed: bool,
    /// Whether a stale PID file was removed.
    pub pid_file_removed: bool,
}

/// Full session and state recovery on daemon startup.
///
/// This is the main entry point for crash recovery. It:
/// 1. Removes stale socket and PID files
/// 2. Recovers stale agent processes (adopt alive ones, fail dead ones)
/// 3. Resets all active spec sessions
/// 4. Marks orphaned in-progress tasks as failed
/// 5. Identifies stories that completed while daemon was down
///
/// All actions are logged at INFO level.
pub fn recover_session_state(
    conn: &Connection,
    socket_path: &Path,
    pid_file: &Path,
) -> Result<RecoveryReport> {
    info!("starting crash recovery...");

    // 1. Clean up stale files
    let socket_removed = remove_stale_socket(socket_path);
    let pid_file_removed = cleanup_stale_pid_file(pid_file);

    // 2. Recover stale agents (from US-037)
    let agents_adopted = recover_stale_agents(conn)?;
    if !agents_adopted.is_empty() {
        info!(
            count = agents_adopted.len(),
            "adopted alive agent processes"
        );
    }

    // 3. Reset all active spec sessions
    let specs_reset = reset_all_active_spec_sessions(conn)?;

    // 4. Mark orphaned in-progress tasks as failed
    let tasks_failed = fail_orphaned_in_progress_items(conn)?;

    // 5. Find stories needing completion flow
    let stories_needing_completion = find_completed_in_progress_stories(conn)?;
    if !stories_needing_completion.is_empty() {
        info!(
            count = stories_needing_completion.len(),
            "found in-progress stories with all tasks done (need completion flow)"
        );
    }

    info!("crash recovery complete");
    Ok(RecoveryReport {
        specs_reset,
        tasks_failed,
        stories_needing_completion,
        agents_adopted,
        socket_removed,
        pid_file_removed,
    })
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

    // --- reset_all_active_spec_sessions ---

    fn make_spec(conn: &Connection, project_id: Uuid, name: &str, session_active: bool) -> Uuid {
        let spec_id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, ?3, '/tmp/spec.md', 'draft', ?4, ?5, ?5)",
            params![
                spec_id.to_string(),
                project_id.to_string(),
                name,
                session_active as i32,
                now,
            ],
        )
        .unwrap();
        spec_id
    }

    #[test]
    fn test_reset_all_active_spec_sessions_none() {
        let conn = test_conn();
        let count = reset_all_active_spec_sessions(&conn).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_reset_all_active_spec_sessions_resets_active() {
        let conn = test_conn();
        let pid1 = make_project(&conn);
        let pid2 = make_project(&conn);

        make_spec(&conn, pid1, "spec-a", true);
        make_spec(&conn, pid1, "spec-b", false);
        make_spec(&conn, pid2, "spec-c", true);

        let count = reset_all_active_spec_sessions(&conn).unwrap();
        assert_eq!(count, 2);

        // All specs should now have session_active = 0
        let active: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM specs WHERE session_active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
    }

    // --- find_orphaned_in_progress_items ---

    #[test]
    fn test_find_orphaned_in_progress_items_empty() {
        let conn = test_conn();
        let orphaned = find_orphaned_in_progress_items(&conn).unwrap();
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_find_orphaned_in_progress_items_with_running_agent() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        // Task is in_progress but has a running agent — NOT orphaned
        let run = make_running_agent(task.id, Some(std::process::id()), None);
        insert_agent_run(&conn, &run).unwrap();

        let orphaned = find_orphaned_in_progress_items(&conn).unwrap();
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_find_orphaned_in_progress_items_no_running_agent() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        // Task is in_progress with no agent_run at all — orphaned
        let orphaned = find_orphaned_in_progress_items(&conn).unwrap();
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0], task.id);
    }

    #[test]
    fn test_fail_orphaned_in_progress_items() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let task = make_work_item(&conn, sid);

        // No running agent for this in_progress task
        let count = fail_orphaned_in_progress_items(&conn).unwrap();
        assert_eq!(count, 1);

        let item = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);
    }

    // --- find_completed_in_progress_stories ---

    fn make_story_with_tasks(
        conn: &Connection,
        session_id: Uuid,
        story_status: WorkItemStatus,
        task_statuses: &[WorkItemStatus],
    ) -> Uuid {
        let epic = WorkItem::new_epic(session_id, "Epic".into(), "Desc".into(), "E1".into(), 0);
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

        // Set story to the desired status
        if story_status == WorkItemStatus::InProgress {
            // Story needs to go pending → ready → in_progress
            update_work_item_status(conn, &story.id, WorkItemStatus::Ready).unwrap();
            update_work_item_status(conn, &story.id, WorkItemStatus::InProgress).unwrap();
        }

        for (i, status) in task_statuses.iter().enumerate() {
            let task = WorkItem::new_task(
                story.id,
                session_id,
                format!("Task {}", i),
                "Desc".into(),
                "AC".into(),
                format!("T{}", i),
                (i + 2) as i32,
            );
            crate::db::work_items::insert_work_item(conn, &task).unwrap();

            match status {
                WorkItemStatus::Done => {
                    update_work_item_status(conn, &task.id, WorkItemStatus::InProgress).unwrap();
                    update_work_item_status(conn, &task.id, WorkItemStatus::Done).unwrap();
                }
                WorkItemStatus::Cancelled => {
                    update_work_item_status(conn, &task.id, WorkItemStatus::Cancelled).unwrap();
                }
                WorkItemStatus::InProgress => {
                    update_work_item_status(conn, &task.id, WorkItemStatus::InProgress).unwrap();
                }
                WorkItemStatus::Failed => {
                    update_work_item_status(conn, &task.id, WorkItemStatus::InProgress).unwrap();
                    update_work_item_status(conn, &task.id, WorkItemStatus::Failed).unwrap();
                }
                _ => {}
            }
        }

        story.id
    }

    #[test]
    fn test_find_completed_in_progress_stories_empty() {
        let conn = test_conn();
        let stories = find_completed_in_progress_stories(&conn).unwrap();
        assert!(stories.is_empty());
    }

    #[test]
    fn test_find_completed_in_progress_stories_all_done() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);

        let story_id = make_story_with_tasks(
            &conn,
            sid,
            WorkItemStatus::InProgress,
            &[WorkItemStatus::Done, WorkItemStatus::Done],
        );

        let stories = find_completed_in_progress_stories(&conn).unwrap();
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0], story_id);
    }

    #[test]
    fn test_find_completed_in_progress_stories_done_and_cancelled() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);

        let story_id = make_story_with_tasks(
            &conn,
            sid,
            WorkItemStatus::InProgress,
            &[WorkItemStatus::Done, WorkItemStatus::Cancelled],
        );

        let stories = find_completed_in_progress_stories(&conn).unwrap();
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0], story_id);
    }

    #[test]
    fn test_find_completed_in_progress_stories_not_all_done() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);

        make_story_with_tasks(
            &conn,
            sid,
            WorkItemStatus::InProgress,
            &[WorkItemStatus::Done, WorkItemStatus::InProgress],
        );

        let stories = find_completed_in_progress_stories(&conn).unwrap();
        assert!(stories.is_empty());
    }

    #[test]
    fn test_find_completed_in_progress_stories_not_in_progress() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);

        // Story is Pending, not InProgress — should not match
        make_story_with_tasks(
            &conn,
            sid,
            WorkItemStatus::Pending,
            &[WorkItemStatus::Done, WorkItemStatus::Done],
        );

        let stories = find_completed_in_progress_stories(&conn).unwrap();
        assert!(stories.is_empty());
    }

    // --- remove_stale_socket ---

    #[test]
    fn test_remove_stale_socket_exists() {
        let dir = std::env::temp_dir().join(format!("nflow_test_sock_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("nflow.sock");
        std::fs::write(&sock, "").unwrap();

        let removed = remove_stale_socket(&sock);
        assert!(removed);
        assert!(!sock.exists());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_remove_stale_socket_not_exists() {
        let sock = std::env::temp_dir().join("nflow_nonexistent.sock");
        let removed = remove_stale_socket(&sock);
        assert!(!removed);
    }

    // --- cleanup_stale_pid_file ---

    #[test]
    fn test_cleanup_stale_pid_file_dead_process() {
        let dir = std::env::temp_dir().join(format!("nflow_test_pid_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("daemon.pid");
        // Write a PID of a dead process
        std::fs::write(&pid_file, "4000000000").unwrap();

        let removed = cleanup_stale_pid_file(&pid_file);
        assert!(removed);
        assert!(!pid_file.exists());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_cleanup_stale_pid_file_current_process() {
        let dir = std::env::temp_dir().join(format!("nflow_test_pid_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("daemon.pid");
        // Write current process PID — should NOT be removed
        std::fs::write(&pid_file, format!("{}", std::process::id())).unwrap();

        let removed = cleanup_stale_pid_file(&pid_file);
        assert!(!removed);
        assert!(pid_file.exists());

        let _ = std::fs::remove_file(&pid_file);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_cleanup_stale_pid_file_invalid_content() {
        let dir = std::env::temp_dir().join(format!("nflow_test_pid_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("daemon.pid");
        std::fs::write(&pid_file, "not-a-number").unwrap();

        let removed = cleanup_stale_pid_file(&pid_file);
        assert!(removed);
        assert!(!pid_file.exists());

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_cleanup_stale_pid_file_nonexistent() {
        let pid_file = std::env::temp_dir().join("nflow_nonexistent.pid");
        let removed = cleanup_stale_pid_file(&pid_file);
        assert!(!removed);
    }

    // --- recover_session_state (integration) ---

    #[test]
    fn test_recover_session_state_empty() {
        let conn = test_conn();
        let dir = std::env::temp_dir().join(format!("nflow_test_recovery_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("nflow.sock");
        let pid = dir.join("daemon.pid");

        let report = recover_session_state(&conn, &sock, &pid).unwrap();
        assert_eq!(report.specs_reset, 0);
        assert_eq!(report.tasks_failed, 0);
        assert!(report.stories_needing_completion.is_empty());
        assert!(report.agents_adopted.is_empty());
        assert!(!report.socket_removed);
        assert!(!report.pid_file_removed);

        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_recover_session_state_full() {
        let conn = test_conn();
        let dir = std::env::temp_dir().join(format!("nflow_test_recovery_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("nflow.sock");
        let pid_path = dir.join("daemon.pid");

        // Create stale socket
        std::fs::write(&sock_path, "").unwrap();
        // Create stale PID file with dead PID
        std::fs::write(&pid_path, "4000000000").unwrap();

        // Create an active spec session
        let project_id = make_project(&conn);
        make_spec(&conn, project_id, "spec-active", true);

        // Create an orphaned in_progress task (no agent)
        let sid = make_session(&conn, project_id);
        let task = make_work_item(&conn, sid);

        let report = recover_session_state(&conn, &sock_path, &pid_path).unwrap();
        assert_eq!(report.specs_reset, 1);
        assert_eq!(report.tasks_failed, 1);
        assert!(report.socket_removed);
        assert!(report.pid_file_removed);

        // Verify the task was actually failed
        let item = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
