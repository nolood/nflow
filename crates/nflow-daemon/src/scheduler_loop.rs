use std::collections::HashMap;

use chrono::Utc;
use rusqlite::Connection;
use tracing::{debug, info, warn};
use uuid::Uuid;

use nflow_core::decomposition::DecompositionStatus;
use nflow_core::scheduler::{schedule, SchedulerAction, SchedulerState, SessionStatus};
use nflow_core::work_item::{ItemType, TaskKind, WorkItemStatus};

use crate::db;
use crate::db::agent_runs::{AgentRun, AgentRunStatus};
use crate::events::{Event, SharedEventBus};
use crate::recovery::{verify_process, ProcessState};

/// Result of evaluating a finished agent's output.
#[derive(Debug, PartialEq, Eq)]
pub struct ReapResult {
    /// Whether the task succeeded.
    pub succeeded: bool,
    /// The Claude session_id extracted from the final result event.
    pub session_id: Option<String>,
    /// The result text from the final result event.
    pub result_text: Option<String>,
    /// Commit hash extracted from impl task result (if present).
    pub commit_hash: Option<String>,
    /// Error message if the task failed.
    pub error_message: Option<String>,
}

/// Evaluate an impl task result from the agent's log file.
///
/// Parses the log for the final result event, extracts commit hash from tool results,
/// then delegates to `nflow_claude::evaluate::evaluate_impl_result` for the actual
/// success criteria check:
/// - exit_code == 0
/// - A new commit was produced (head_after != head_before)
/// - Commit message contains [short_id] tag
///
/// For reaping (where we may not have head_before), a present commit_hash implies
/// a new commit was made.
pub fn evaluate_impl_result(
    exit_code: Option<i32>,
    log_path: Option<&str>,
    short_id: &str,
) -> ReapResult {
    let (session_id, result_text, commit_hash, commit_message) = parse_log_file(log_path);

    let code = exit_code.unwrap_or(1);

    // Use the existing evaluate module from nflow-claude.
    // For reaping, we use "" as head_before and the commit hash (if found) as head_after.
    // This way, if a commit was produced, head_before != head_after.
    let head_before = "";
    let head_after = commit_hash.as_deref().unwrap_or("");
    let msg = commit_message.as_deref().unwrap_or("");

    let task_result =
        nflow_claude::evaluate::evaluate_impl_result(code, head_before, head_after, msg, short_id);

    match task_result {
        nflow_claude::evaluate::TaskResult::Success => ReapResult {
            succeeded: true,
            session_id,
            result_text,
            commit_hash,
            error_message: None,
        },
        nflow_claude::evaluate::TaskResult::Failed { reason } => ReapResult {
            succeeded: false,
            session_id,
            result_text,
            commit_hash: None,
            error_message: Some(reason),
        },
    }
}

/// Evaluate a verify task result from the agent's log file.
///
/// Delegates to `nflow_claude::evaluate::evaluate_verify_result` which checks:
/// - exit_code == 0
/// - result_text contains "VERIFICATION PASSED"
pub fn evaluate_verify_result(exit_code: Option<i32>, log_path: Option<&str>) -> ReapResult {
    let (session_id, result_text, _, _) = parse_log_file(log_path);

    let code = exit_code.unwrap_or(1);
    let text = result_text.as_deref().unwrap_or("");

    let task_result = nflow_claude::evaluate::evaluate_verify_result(code, text);

    match task_result {
        nflow_claude::evaluate::TaskResult::Success => ReapResult {
            succeeded: true,
            session_id,
            result_text,
            commit_hash: None,
            error_message: None,
        },
        nflow_claude::evaluate::TaskResult::Failed { reason } => ReapResult {
            succeeded: false,
            session_id,
            result_text,
            commit_hash: None,
            error_message: Some(reason),
        },
    }
}

/// Parse an agent log file for the final result event.
///
/// Reads the log file line-by-line looking for the last `{"type":"result",...}` line.
/// Returns (session_id, result_text, commit_hash, commit_message).
fn parse_log_file(
    log_path: Option<&str>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let path = match log_path {
        Some(p) => p,
        None => return (None, None, None, None),
    };

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None, None, None),
    };

    let mut session_id = None;
    let mut result_text = None;
    let mut commit_hash = None;
    let mut commit_message = None;

    // Parse each line as stream-json, looking for the last Result event
    for line in content.lines() {
        let event = nflow_claude::stream::parse_line(line);
        match event {
            nflow_claude::stream::StreamEvent::Result {
                text,
                session_id: sid,
            } => {
                if !sid.is_empty() {
                    session_id = Some(sid);
                }
                if !text.is_empty() {
                    result_text = Some(text);
                }
            }
            nflow_claude::stream::StreamEvent::ToolResult { content } => {
                // Look for commit hashes and messages in tool results (git commit output)
                if let Some((hash, msg)) = extract_commit_info(&content) {
                    commit_hash = Some(hash);
                    commit_message = Some(msg);
                }
            }
            _ => {}
        }
    }

    // Also check result text for commit info if we didn't find one in tool results
    if commit_hash.is_none() {
        if let Some(ref text) = result_text {
            if let Some((hash, msg)) = extract_commit_info(text) {
                commit_hash = Some(hash);
                commit_message = Some(msg);
            }
        }
    }

    (session_id, result_text, commit_hash, commit_message)
}

/// Extract a git commit hash and commit message from text.
///
/// Looks for patterns like:
/// - `[main abc1234] feat: [T1] implement login` (git commit output)
///
/// Returns (hash, full_commit_message) if found.
fn extract_commit_info(text: &str) -> Option<(String, String)> {
    // Pattern: git commit output like "[branch abc1234] message"
    for line in text.lines() {
        let trimmed = line.trim();
        // Match: [branch hash] message
        if trimmed.starts_with('[') {
            if let Some(bracket_end) = trimmed.find(']') {
                let inside = &trimmed[1..bracket_end];
                let parts: Vec<&str> = inside.split_whitespace().collect();
                if parts.len() >= 2 {
                    let candidate = parts[1];
                    if is_hex_hash(candidate) {
                        let message = trimmed[bracket_end + 1..].trim().to_string();
                        return Some((candidate.to_string(), message));
                    }
                }
            }
        }
    }
    None
}

/// Check if a string looks like a git short or full hash (7-40 hex chars).
fn is_hex_hash(s: &str) -> bool {
    let len = s.len();
    (7..=40).contains(&len) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Reap finished agent processes.
///
/// Checks all running agent_runs, verifies their PIDs, and for dead processes:
/// 1. Reads exit code (from waitpid or assumes failure)
/// 2. Parses the log file for session_id and result text
/// 3. Evaluates the result (impl vs verify)
/// 4. Updates agent_run status in DB
/// 5. Updates work_item status in DB
/// 6. Stores commit_hash on successful impl tasks
/// 7. Broadcasts status change events
///
/// Returns the number of agents reaped.
pub fn reap_finished_agents(conn: &Connection, event_bus: Option<&SharedEventBus>) -> u32 {
    let running = match db::agent_runs::find_running_agent_runs(conn) {
        Ok(r) => r,
        Err(e) => {
            warn!("reap: failed to find running agent runs: {}", e);
            return 0;
        }
    };

    if running.is_empty() {
        return 0;
    }

    let mut reaped = 0;

    for run in &running {
        let state = match run.pid {
            Some(pid) => verify_process(pid, run.pid_start_time),
            None => ProcessState::Dead, // No PID = assume dead
        };

        if state == ProcessState::Alive {
            continue; // Still running
        }

        // Process is dead or PID reused — reap it
        debug!(
            "reap: agent {} (pid={:?}) for task {} is {:?}",
            run.id, run.pid, run.work_item_id, state
        );

        let exit_code = if state == ProcessState::Dead {
            // Try to get exit code via waitpid for dead processes
            // For processes we didn't spawn in this daemon lifetime, waitpid won't work.
            // Use the exit code from the agent run if available, otherwise assume failure.
            run.exit_code.or(Some(1)) // Default to exit code 1 for dead processes
        } else {
            // PID reused — the original process is gone, treat as failure
            None
        };

        // Look up the work item to determine task kind
        let work_item = match db::work_items::get_work_item_by_id(conn, &run.work_item_id) {
            Ok(Some(item)) => item,
            Ok(None) => {
                warn!(
                    "reap: work item {} not found for agent {}",
                    run.work_item_id, run.id
                );
                mark_agent_failed(conn, run, "work item not found");
                reaped += 1;
                continue;
            }
            Err(e) => {
                warn!("reap: failed to load work item {}: {}", run.work_item_id, e);
                continue;
            }
        };

        // Evaluate based on task kind
        let result = if work_item.kind == Some(TaskKind::Verify) {
            evaluate_verify_result(exit_code, run.log_path.as_deref())
        } else {
            evaluate_impl_result(exit_code, run.log_path.as_deref(), &work_item.short_id)
        };

        let now = Utc::now();
        let old_status = work_item.status.to_string();

        if result.succeeded {
            // Update agent run as succeeded
            if let Err(e) = db::agent_runs::update_agent_run_status(
                conn,
                &run.id,
                AgentRunStatus::Succeeded,
                exit_code,
                None,
                Some(now),
            ) {
                warn!("reap: failed to update agent run {}: {}", run.id, e);
                continue;
            }

            // Store session_id on agent run if available
            if let Some(ref sid) = result.session_id {
                let _ = db::agent_runs::update_agent_run_session_id(conn, &run.id, sid);
            }

            // Store commit_hash on impl tasks
            if let Some(ref hash) = result.commit_hash {
                if work_item.kind == Some(TaskKind::Impl) {
                    let _ = db::work_items::update_work_item_commit(conn, &run.work_item_id, hash);
                    info!(
                        "reap: stored commit_hash={} for task {}",
                        hash, run.work_item_id
                    );
                }
            }

            // Mark work item as done
            if let Err(e) = db::work_items::update_work_item_status(
                conn,
                &run.work_item_id,
                WorkItemStatus::Done,
            ) {
                warn!(
                    "reap: failed to update work item {}: {}",
                    run.work_item_id, e
                );
            } else {
                info!(
                    "reap: task {} succeeded (agent {})",
                    run.work_item_id, run.id
                );
                broadcast_status_change(
                    event_bus,
                    &run.work_item_id,
                    &work_item,
                    &old_status,
                    "done",
                    conn,
                );
            }
        } else {
            // Update agent run as failed
            let error_msg = result.error_message.as_deref().unwrap_or("unknown error");
            if let Err(e) = db::agent_runs::update_agent_run_status(
                conn,
                &run.id,
                AgentRunStatus::Failed,
                exit_code,
                Some(error_msg),
                Some(now),
            ) {
                warn!("reap: failed to update agent run {}: {}", run.id, e);
                continue;
            }

            // Store session_id on agent run if available
            if let Some(ref sid) = result.session_id {
                let _ = db::agent_runs::update_agent_run_session_id(conn, &run.id, sid);
            }

            // Mark work item as failed
            if let Err(e) = db::work_items::update_work_item_status(
                conn,
                &run.work_item_id,
                WorkItemStatus::Failed,
            ) {
                warn!(
                    "reap: failed to update work item {}: {}",
                    run.work_item_id, e
                );
            } else {
                info!(
                    "reap: task {} failed (agent {}): {}",
                    run.work_item_id, run.id, error_msg
                );
                broadcast_status_change(
                    event_bus,
                    &run.work_item_id,
                    &work_item,
                    &old_status,
                    "failed",
                    conn,
                );
            }
        }

        reaped += 1;
    }

    reaped
}

/// Helper to mark an agent as failed with an error message.
fn mark_agent_failed(conn: &Connection, run: &AgentRun, error: &str) {
    let _ = db::agent_runs::update_agent_run_status(
        conn,
        &run.id,
        AgentRunStatus::Failed,
        None,
        Some(error),
        Some(Utc::now()),
    );
}

/// Broadcast a status change event for a work item.
fn broadcast_status_change(
    event_bus: Option<&SharedEventBus>,
    _item_id: &Uuid,
    item: &nflow_core::work_item::WorkItem,
    old_status: &str,
    new_status: &str,
    conn: &Connection,
) {
    let bus = match event_bus {
        Some(b) => b,
        None => return,
    };

    // Find the project_id by traversing: task → story → epic → session → project
    let project_id = find_project_id_for_item(conn, item);

    let item_type = match item.item_type {
        ItemType::Epic => "epic",
        ItemType::Story => "story",
        ItemType::Task => "task",
    };

    bus.broadcast(Event::StatusChange {
        item_id: item.id.to_string(),
        item_type: item_type.to_string(),
        old_status: old_status.to_string(),
        new_status: new_status.to_string(),
        project_id: project_id.map(|id| id.to_string()).unwrap_or_default(),
    });
}

/// Find the project_id for a work item by looking up its decomposition session.
fn find_project_id_for_item(
    conn: &Connection,
    item: &nflow_core::work_item::WorkItem,
) -> Option<Uuid> {
    let session =
        db::decomposition_sessions::get_decomposition_session(conn, &item.decomposition_session_id)
            .ok()??;
    Some(session.project_id)
}

/// Runs a single scheduler tick for all projects.
///
/// For each project with `execution_enabled = true`, this function:
/// 1. Reaps finished agent processes (US-053)
/// 2. Loads all decomposition sessions and their work items + dependencies
/// 3. Counts running agents
/// 4. Builds a `SchedulerState` snapshot
/// 5. Calls the pure `schedule()` algorithm
/// 6. Logs all returned actions at DEBUG level
///
/// Returns the collected actions for all projects (project_id, action).
/// The caller (daemon main loop) is responsible for executing these actions
/// (updating DB, spawning agents, etc.).
pub fn scheduler_tick(
    conn: &Connection,
    event_bus: Option<&SharedEventBus>,
) -> Vec<(Uuid, SchedulerAction)> {
    // Phase 0: Reap finished agent processes before scheduling
    let reaped = reap_finished_agents(conn, event_bus);
    if reaped > 0 {
        debug!("scheduler: reaped {} finished agent(s)", reaped);
    }

    let projects = match db::projects::list_projects(conn) {
        Ok(p) => p,
        Err(e) => {
            warn!("scheduler: failed to list projects: {}", e);
            return Vec::new();
        }
    };

    // Skip if no projects have execution enabled
    let enabled_projects: Vec<_> = projects.iter().filter(|p| p.execution_enabled).collect();

    if enabled_projects.is_empty() {
        debug!("scheduler: no projects with execution_enabled, skipping tick");
        return Vec::new();
    }

    let mut all_actions = Vec::new();

    for project in &enabled_projects {
        let sessions = match db::decomposition_sessions::list_sessions_by_project(conn, &project.id)
        {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "scheduler: failed to list sessions for project {}: {}",
                    project.name, e
                );
                continue;
            }
        };

        if sessions.is_empty() {
            continue;
        }

        // Build session statuses map
        let session_statuses: HashMap<Uuid, SessionStatus> = sessions
            .iter()
            .map(|s| {
                let status = match s.status {
                    DecompositionStatus::InProgress => SessionStatus::InProgress,
                    DecompositionStatus::Approved => SessionStatus::Approved,
                    DecompositionStatus::Discarded => SessionStatus::Discarded,
                };
                (s.id, status)
            })
            .collect();

        // Collect all work items and dependencies across sessions
        let mut work_items = Vec::new();
        let mut dependencies = Vec::new();

        for session in &sessions {
            match db::work_items::list_work_items_by_session(conn, &session.id) {
                Ok(items) => work_items.extend(items),
                Err(e) => {
                    warn!(
                        "scheduler: failed to list work items for session {}: {}",
                        session.id, e
                    );
                    continue;
                }
            }

            match db::work_items::list_dependencies_by_session(conn, &session.id) {
                Ok(deps) => dependencies.extend(deps),
                Err(e) => {
                    warn!(
                        "scheduler: failed to list dependencies for session {}: {}",
                        session.id, e
                    );
                    continue;
                }
            }
        }

        // Count running agents for this project
        let running_count =
            match db::agent_runs::find_running_agent_runs_by_project(conn, &project.id) {
                Ok(runs) => runs.len() as u32,
                Err(e) => {
                    warn!(
                        "scheduler: failed to count running agents for project {}: {}",
                        project.name, e
                    );
                    0
                }
            };

        // Build scheduler state
        let state = SchedulerState {
            work_items,
            dependencies,
            running_count,
            max_parallel: 3, // TODO: read from project config (US for config integration)
            execution_enabled: project.execution_enabled,
            session_statuses,
        };

        let actions = schedule(&state);

        for action in &actions {
            debug!("scheduler: project={} action={:?}", project.name, action);
        }

        all_actions.extend(actions.into_iter().map(|a| (project.id, a)));
    }

    all_actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::agent_runs::{insert_agent_run, AgentRun, AgentRunStatus};
    use crate::db::test_conn;
    use chrono::Utc;
    use nflow_core::decomposition::DecompositionSession;
    use nflow_core::project::{GitProvider, Project};
    use nflow_core::work_item::WorkItem;

    fn make_project(name: &str, execution_enabled: bool) -> Project {
        let now = Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            path: format!("/tmp/{}", name),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_approved_session(project_id: Uuid, wave: u32) -> DecompositionSession {
        let now = Utc::now();
        DecompositionSession {
            id: Uuid::new_v4(),
            project_id,
            wave_number: wave,
            status: DecompositionStatus::Approved,
            claude_session_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_running_agent(work_item_id: Uuid, pid: Option<u32>) -> AgentRun {
        AgentRun {
            id: Uuid::new_v4(),
            work_item_id,
            pid,
            session_id: None,
            pid_start_time: None,
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: None,
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        }
    }

    // --- scheduler_tick tests ---

    #[test]
    fn tick_returns_empty_with_no_projects() {
        let conn = test_conn();
        let actions = scheduler_tick(&conn, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_skips_projects_without_execution_enabled() {
        let conn = test_conn();
        let project = make_project("disabled", false);
        db::projects::insert_project(&conn, &project).unwrap();

        let actions = scheduler_tick(&conn, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_skips_projects_with_no_sessions() {
        let conn = test_conn();
        let mut project = make_project("enabled", true);
        project.execution_enabled = true;
        db::projects::insert_project(&conn, &project).unwrap();

        let actions = scheduler_tick(&conn, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_produces_mark_ready_for_pending_story() {
        let conn = test_conn();

        // Create project with execution enabled
        let project = make_project("myproject", true);
        db::projects::insert_project(&conn, &project).unwrap();

        // Create approved session
        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        // Create epic and story
        let epic = WorkItem::new_epic(session.id, "Epic 1".into(), "Desc".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "Story 1".into(),
            "Desc".into(),
            "AC".into(),
            "S1".into(),
            0,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let actions = scheduler_tick(&conn, None);

        // Should have MarkStoryReady for the pending story (no blockers)
        let mark_ready: Vec<_> = actions
            .iter()
            .filter(|(_, a)| {
                matches!(a, SchedulerAction::MarkStoryReady { story_id } if *story_id == story.id)
            })
            .collect();
        assert_eq!(mark_ready.len(), 1);
        assert_eq!(mark_ready[0].0, project.id);
    }

    #[test]
    fn tick_returns_empty_for_unapproved_session() {
        let conn = test_conn();

        let project = make_project("proj", true);
        db::projects::insert_project(&conn, &project).unwrap();

        // Create in_progress (not approved) session
        let now = Utc::now();
        let session = DecompositionSession {
            id: Uuid::new_v4(),
            project_id: project.id,
            wave_number: 1,
            status: DecompositionStatus::InProgress,
            claude_session_id: None,
            created_at: now,
            updated_at: now,
        };
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "Epic".into(), "Desc".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "Story".into(),
            "Desc".into(),
            "AC".into(),
            "S1".into(),
            0,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let actions = scheduler_tick(&conn, None);

        // No actions for stories in unapproved sessions
        let story_actions: Vec<_> = actions
            .iter()
            .filter(|(_, a)| {
                matches!(
                    a,
                    SchedulerAction::MarkStoryReady { .. } | SchedulerAction::StartStory { .. }
                )
            })
            .collect();
        assert!(story_actions.is_empty());
    }

    // --- evaluate_impl_result tests ---

    #[test]
    fn evaluate_impl_exit_code_nonzero_fails() {
        let result = evaluate_impl_result(Some(1), None, "T1");
        assert!(!result.succeeded);
        assert!(result.error_message.is_some());
    }

    #[test]
    fn evaluate_impl_no_exit_code_fails() {
        let result = evaluate_impl_result(None, None, "T1");
        assert!(!result.succeeded);
        assert!(result.error_message.is_some());
    }

    #[test]
    fn evaluate_verify_exit_code_nonzero_fails() {
        let result = evaluate_verify_result(Some(1), None);
        assert!(!result.succeeded);
        assert!(result.error_message.is_some());
    }

    // --- extract_commit_info tests ---

    #[test]
    fn extract_info_from_git_output() {
        let text = "[main abc1234] Add feature\n";
        let info = extract_commit_info(text);
        assert_eq!(
            info,
            Some(("abc1234".to_string(), "Add feature".to_string()))
        );
    }

    #[test]
    fn extract_info_from_branch_output() {
        let text = "[feature/s1 deadbeef] Implement story\n";
        let info = extract_commit_info(text);
        assert_eq!(
            info,
            Some(("deadbeef".to_string(), "Implement story".to_string()))
        );
    }

    #[test]
    fn no_info_in_plain_text() {
        let text = "This is just plain text\nwith no commit hash\n";
        let info = extract_commit_info(text);
        assert!(info.is_none());
    }

    #[test]
    fn is_hex_hash_valid_short() {
        assert!(is_hex_hash("abc1234"));
    }

    #[test]
    fn is_hex_hash_valid_full() {
        assert!(is_hex_hash("abc1234567890def1234567890abcdef12345678"));
    }

    #[test]
    fn is_hex_hash_too_short() {
        assert!(!is_hex_hash("abc12"));
    }

    #[test]
    fn is_hex_hash_non_hex() {
        assert!(!is_hex_hash("ghijklm"));
    }

    // --- reap_finished_agents tests ---

    #[test]
    fn reap_no_running_agents() {
        let conn = test_conn();
        let reaped = reap_finished_agents(&conn, None);
        assert_eq!(reaped, 0);
    }

    #[test]
    fn reap_alive_agent_not_reaped() {
        let conn = test_conn();
        let project = make_project("proj", true);
        db::projects::insert_project(&conn, &project).unwrap();

        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "E".into(), "D".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "S".into(),
            "D".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let task = WorkItem::new_task(
            story.id,
            session.id,
            "T".into(),
            "D".into(),
            "AC".into(),
            "T1".into(),
            2,
        );
        db::work_items::insert_work_item(&conn, &task).unwrap();
        db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::InProgress)
            .unwrap();

        // Agent with current process PID — it's alive
        let run = AgentRun {
            pid: Some(std::process::id()),
            ..make_running_agent(task.id, Some(std::process::id()))
        };
        insert_agent_run(&conn, &run).unwrap();

        let reaped = reap_finished_agents(&conn, None);
        assert_eq!(reaped, 0);

        // Agent should still be running
        let running = db::agent_runs::find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn reap_dead_agent_marks_task_failed() {
        let conn = test_conn();
        let project = make_project("proj", true);
        db::projects::insert_project(&conn, &project).unwrap();

        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "E".into(), "D".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "S".into(),
            "D".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let task = WorkItem::new_task(
            story.id,
            session.id,
            "T".into(),
            "D".into(),
            "AC".into(),
            "T1".into(),
            2,
        );
        db::work_items::insert_work_item(&conn, &task).unwrap();
        db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::InProgress)
            .unwrap();

        // Dead agent (PID doesn't exist)
        let run = make_running_agent(task.id, Some(4_000_000_000));
        insert_agent_run(&conn, &run).unwrap();

        let reaped = reap_finished_agents(&conn, None);
        assert_eq!(reaped, 1);

        // Agent run should be marked as failed
        let running = db::agent_runs::find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());

        // Work item should be failed
        let item = db::work_items::get_work_item_by_id(&conn, &task.id)
            .unwrap()
            .unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);
    }

    #[test]
    fn reap_agent_no_pid_marks_failed() {
        let conn = test_conn();
        let project = make_project("proj", true);
        db::projects::insert_project(&conn, &project).unwrap();

        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "E".into(), "D".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "S".into(),
            "D".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let task = WorkItem::new_task(
            story.id,
            session.id,
            "T".into(),
            "D".into(),
            "AC".into(),
            "T1".into(),
            2,
        );
        db::work_items::insert_work_item(&conn, &task).unwrap();
        db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::InProgress)
            .unwrap();

        // Agent with no PID
        let run = make_running_agent(task.id, None);
        insert_agent_run(&conn, &run).unwrap();

        let reaped = reap_finished_agents(&conn, None);
        assert_eq!(reaped, 1);

        // Work item should be failed
        let item = db::work_items::get_work_item_by_id(&conn, &task.id)
            .unwrap()
            .unwrap();
        assert_eq!(item.status, WorkItemStatus::Failed);
    }

    #[test]
    fn reap_broadcasts_status_change_event() {
        let conn = test_conn();
        let project = make_project("proj", true);
        db::projects::insert_project(&conn, &project).unwrap();

        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "E".into(), "D".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "S".into(),
            "D".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story).unwrap();

        let task = WorkItem::new_task(
            story.id,
            session.id,
            "T".into(),
            "D".into(),
            "AC".into(),
            "T1".into(),
            2,
        );
        db::work_items::insert_work_item(&conn, &task).unwrap();
        db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::InProgress)
            .unwrap();

        // Dead agent
        let run = make_running_agent(task.id, Some(4_000_000_000));
        insert_agent_run(&conn, &run).unwrap();

        // Create event bus and subscribe
        let bus = crate::events::new_event_bus(16);
        let client = crate::events::ClientId::new();
        let mut rx = bus.subscribe(client);

        let reaped = reap_finished_agents(&conn, Some(&bus));
        assert_eq!(reaped, 1);

        // Should have received a status change event
        let event = rx.try_recv().unwrap();
        match event {
            Event::StatusChange {
                item_id,
                new_status,
                ..
            } => {
                assert_eq!(item_id, task.id.to_string());
                assert_eq!(new_status, "failed");
            }
            _ => panic!("expected StatusChange event"),
        }
    }

    // --- parse_log_file with actual file ---

    #[test]
    fn parse_log_file_with_result_event() {
        let dir = std::env::temp_dir().join(format!("nflow_test_log_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("agent.log");

        let content = r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"working..."}}}
{"type":"tool_use","name":"Bash","input":{"command":"git commit -m 'feat: add feature'"}}
{"type":"tool_result","content":"[main abc1234f] feat: add feature\n 1 file changed"}
{"type":"result","result":"Task completed successfully.","session_id":"sess-42"}
"#;
        std::fs::write(&log_file, content).unwrap();

        let (sid, text, hash, msg) = parse_log_file(Some(log_file.to_str().unwrap()));
        assert_eq!(sid, Some("sess-42".to_string()));
        assert_eq!(text, Some("Task completed successfully.".to_string()));
        assert_eq!(hash, Some("abc1234f".to_string()));
        assert!(msg.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_log_file_nonexistent() {
        let (sid, text, hash, msg) = parse_log_file(Some("/nonexistent/path/log.log"));
        assert!(sid.is_none());
        assert!(text.is_none());
        assert!(hash.is_none());
        assert!(msg.is_none());
    }

    #[test]
    fn parse_log_file_none_path() {
        let (sid, text, hash, msg) = parse_log_file(None);
        assert!(sid.is_none());
        assert!(text.is_none());
        assert!(hash.is_none());
        assert!(msg.is_none());
    }

    #[test]
    fn evaluate_impl_with_log_file_success() {
        let dir = std::env::temp_dir().join(format!("nflow_test_eval_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("agent.log");

        // Commit message contains [T1] tag — matches the short_id
        let content = r#"{"type":"tool_result","content":"[feature/s1 deadbeef] feat: [T1] Add feature"}
{"type":"result","result":"Done.","session_id":"s1"}
"#;
        std::fs::write(&log_file, content).unwrap();

        let result = evaluate_impl_result(Some(0), Some(log_file.to_str().unwrap()), "T1");
        assert!(result.succeeded);
        assert_eq!(result.session_id, Some("s1".to_string()));
        assert_eq!(result.commit_hash, Some("deadbeef".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_verify_with_log_file_success() {
        let dir = std::env::temp_dir().join(format!("nflow_test_verify_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("agent.log");

        let content = r#"{"type":"result","result":"All checks passed. VERIFICATION PASSED","session_id":"v1"}"#;
        std::fs::write(&log_file, format!("{}\n", content)).unwrap();

        let result = evaluate_verify_result(Some(0), Some(log_file.to_str().unwrap()));
        assert!(result.succeeded);
        assert_eq!(result.session_id, Some("v1".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
