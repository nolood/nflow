use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::Connection;
use tracing::{debug, info, warn};
use uuid::Uuid;

use nflow_core::config::{self, Config};
use nflow_core::decomposition::DecompositionStatus;
use nflow_core::project::Project;
use nflow_core::scheduler::{schedule, SchedulerAction, SchedulerState, SessionStatus};
use nflow_core::work_item::{ItemType, TaskKind, WorkItem, WorkItemStatus};

use crate::db;
use crate::db::agent_runs::{AgentRun, AgentRunStatus};
use crate::events::{Event, SharedEventBus};
use crate::recovery::{read_process_start_time, verify_process, ProcessState};

/// An action to progress a story after a task completes.
///
/// Returned by `reap_finished_agents` for async execution by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoryProgressAction {
    /// Start the next task in the story (either paired verify or next impl).
    StartNextTask {
        task_id: Uuid,
        story_id: Uuid,
        project_id: Uuid,
    },
    /// Mark the story as failed (task or verify failed).
    FailStory { story_id: Uuid },
    /// Mark the story as done (all tasks completed).
    CompleteStory { story_id: Uuid },
}

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
/// 8. Determines story progression actions (start next task, fail/complete story)
///
/// Returns (reaped_count, progress_actions) where progress_actions describe
/// async follow-up work (starting next tasks, completing stories).
pub fn reap_finished_agents(
    conn: &Connection,
    event_bus: Option<&SharedEventBus>,
) -> (u32, Vec<StoryProgressAction>) {
    let running = match db::agent_runs::find_running_agent_runs(conn) {
        Ok(r) => r,
        Err(e) => {
            warn!("reap: failed to find running agent runs: {}", e);
            return (0, Vec::new());
        }
    };

    if running.is_empty() {
        return (0, Vec::new());
    }

    let mut reaped = 0;
    let mut progress_actions = Vec::new();

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

                // Determine story progression after task success
                if let Some(action) = determine_story_progress(conn, &work_item, true, event_bus) {
                    progress_actions.push(action);
                }
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

                // Determine story progression after task failure
                if let Some(action) = determine_story_progress(conn, &work_item, false, event_bus) {
                    progress_actions.push(action);
                }
            }
        }

        reaped += 1;
    }

    (reaped, progress_actions)
}

/// Determine the next story progression action after a task completes.
///
/// Logic:
/// - On impl task success: start paired verify task (if exists), otherwise next impl task
/// - On verify task success: start next pending impl task
/// - On any task failure (impl or verify): fail the story
/// - When all tasks are done: complete the story
fn determine_story_progress(
    conn: &Connection,
    completed_task: &WorkItem,
    succeeded: bool,
    event_bus: Option<&SharedEventBus>,
) -> Option<StoryProgressAction> {
    let story_id = completed_task.parent_id?;
    let is_verify = completed_task.kind == Some(TaskKind::Verify);

    // On failure: always fail the story immediately
    if !succeeded {
        info!(
            "progress: task {} failed — failing story {}",
            completed_task.short_id, story_id
        );

        // Update story status to Failed
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &story_id, WorkItemStatus::Failed)
        {
            warn!("progress: failed to fail story {}: {}", story_id, e);
        } else if let Ok(Some(story)) = db::work_items::get_work_item_by_id(conn, &story_id) {
            broadcast_status_change(event_bus, &story_id, &story, "in_progress", "failed", conn);
        }

        return Some(StoryProgressAction::FailStory { story_id });
    }

    // On success: determine next task
    let tasks = match db::work_items::list_work_items_by_parent(conn, &story_id) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                "progress: failed to list tasks for story {}: {}",
                story_id, e
            );
            return None;
        }
    };

    let next_task = if is_verify {
        // Verify succeeded: find the next pending task (next impl by sort_order)
        nflow_core::work_item::get_next_pending_task(story_id, &tasks)
    } else {
        // Impl succeeded: find paired verify task first
        let verify = tasks.iter().find(|t| {
            t.item_type == ItemType::Task
                && t.kind == Some(TaskKind::Verify)
                && t.parent_id == completed_task.parent_id
                && t.sort_order == completed_task.sort_order + 1
                && t.status == WorkItemStatus::Pending
        });

        if verify.is_some() {
            verify
        } else {
            // No verify task — find next pending task
            nflow_core::work_item::get_next_pending_task(story_id, &tasks)
        }
    };

    if let Some(task) = next_task {
        // Mark the next task as InProgress
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &task.id, WorkItemStatus::InProgress)
        {
            warn!(
                "progress: failed to mark task {} as in_progress: {}",
                task.id, e
            );
            return None;
        }
        info!(
            "progress: starting next task {} ({}) for story {}",
            task.short_id, task.id, story_id
        );
        broadcast_status_change(event_bus, &task.id, task, "pending", "in_progress", conn);

        // Find the project_id for spawning the agent
        let project_id = find_project_id_for_item(conn, task)?;

        return Some(StoryProgressAction::StartNextTask {
            task_id: task.id,
            story_id,
            project_id,
        });
    }

    // No more pending tasks — check if all tasks are done (or done+cancelled/skipped)
    let all_finished = tasks
        .iter()
        .filter(|t| t.item_type == ItemType::Task)
        .all(|t| t.status == WorkItemStatus::Done || t.status == WorkItemStatus::Cancelled);

    if all_finished {
        info!(
            "progress: all tasks done for story {} — completing",
            story_id
        );

        // Mark story as Done
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &story_id, WorkItemStatus::Done)
        {
            warn!("progress: failed to complete story {}: {}", story_id, e);
        } else if let Ok(Some(story)) = db::work_items::get_work_item_by_id(conn, &story_id) {
            broadcast_status_change(event_bus, &story_id, &story, "in_progress", "done", conn);
        }

        return Some(StoryProgressAction::CompleteStory { story_id });
    }

    None
}

/// Execute story progress actions that were collected during reaping.
///
/// These actions require async operations (spawning Claude agents).
pub async fn execute_story_progress_actions(
    conn: &Connection,
    actions: &[StoryProgressAction],
    event_bus: Option<&SharedEventBus>,
) {
    for action in actions {
        match action {
            StoryProgressAction::StartNextTask {
                task_id,
                story_id,
                project_id,
            } => {
                // Load project, story, and task for agent spawning
                let project = match db::projects::get_project_by_id(conn, project_id) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        warn!(
                            "progress_exec: project {} not found for task {}",
                            project_id, task_id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            "progress_exec: failed to load project {}: {}",
                            project_id, e
                        );
                        continue;
                    }
                };

                let story = match db::work_items::get_work_item_by_id(conn, story_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        warn!("progress_exec: story {} not found", story_id);
                        continue;
                    }
                    Err(e) => {
                        warn!("progress_exec: failed to load story {}: {}", story_id, e);
                        continue;
                    }
                };

                let task = match db::work_items::get_work_item_by_id(conn, task_id) {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        warn!("progress_exec: task {} not found", task_id);
                        continue;
                    }
                    Err(e) => {
                        warn!("progress_exec: failed to load task {}: {}", task_id, e);
                        continue;
                    }
                };

                start_task_execution(conn, &task, &project, &story, event_bus).await;
            }
            StoryProgressAction::FailStory { .. } | StoryProgressAction::CompleteStory { .. } => {
                // These are already handled synchronously in determine_story_progress
                // (DB updates and broadcasts done inline).
                // Listed here for completeness — no async follow-up needed.
            }
        }
    }
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

/// Convert a story title to a URL-safe slug for use in branch names.
///
/// Lowercases, replaces non-alphanumeric characters with hyphens,
/// collapses consecutive hyphens, and trims leading/trailing hyphens.
/// Truncates to 50 characters to keep branch names reasonable.
fn slugify(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens
    let mut result = String::with_capacity(slug.len());
    let mut prev_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    // Trim hyphens and truncate
    let trimmed = result.trim_matches('-');
    if trimmed.len() > 50 {
        trimmed[..50].trim_end_matches('-').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Start execution of a task by spawning a Claude agent process.
///
/// This function:
/// 1. Records the head_before commit hash (for impl tasks)
/// 2. Loads and renders the appropriate prompt template (task_execution.md or verify_task.md)
/// 3. Writes the rendered context to a temp file for `--append-system-prompt-file`
/// 4. Spawns a Claude process via `ClaudeRunner::spawn()`
/// 5. Creates an `agent_run` record with PID, pid_start_time, log_path, status=running
/// 6. Spawns a background task to pipe agent stdout to a log file and broadcast output to clients
pub async fn start_task_execution(
    conn: &Connection,
    task: &WorkItem,
    project: &Project,
    story: &WorkItem,
    event_bus: Option<&SharedEventBus>,
) {
    let task_id = task.id;
    let short_id = task.short_id.clone();
    let is_verify = task.kind == Some(TaskKind::Verify);

    // 1. Record head_before commit hash (for impl tasks only)
    let worktree_path = match &story.worktree_path {
        Some(p) => PathBuf::from(p),
        None => {
            warn!(
                "start_task: story {} has no worktree_path, cannot start task {}",
                story.id, task_id
            );
            return;
        }
    };

    let head_before = if !is_verify {
        match nflow_git::branch::get_head_commit(&worktree_path).await {
            Ok(hash) => Some(hash),
            Err(e) => {
                warn!(
                    "start_task: failed to get HEAD commit for task {}: {}",
                    task_id, e
                );
                None
            }
        }
    } else {
        None
    };

    // Store head_before on the work item if available (for later comparison during reaping)
    // We store it in commit_hash temporarily; reaping will compare against the post-run hash.
    // Actually, we don't store head_before on the work item — the reap logic uses "" as head_before
    // when it doesn't know the pre-run state. The presence of a commit hash post-run is sufficient.
    let _ = head_before; // Used for logging only at this point

    // 2. Load and render prompt template
    let nflow_home = match crate::daemon::nflow_home() {
        Ok(h) => h,
        Err(e) => {
            warn!(
                "start_task: failed to get nflow home for task {}: {}",
                task_id, e
            );
            return;
        }
    };

    let override_dir = nflow_home.join("prompts");
    let override_path = if override_dir.is_dir() {
        Some(override_dir)
    } else {
        None
    };

    let template_name = if is_verify {
        "verify_task"
    } else {
        "task_execution"
    };

    let template =
        match nflow_claude::prompt::load_template(template_name, override_path.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    "start_task: failed to load template '{}' for task {}: {}",
                    template_name, task_id, e
                );
                return;
            }
        };

    // Build context variables for the template
    let tasks_for_story = match db::work_items::list_work_items_by_parent(conn, &story.id) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                "start_task: failed to list tasks for story {}: {}",
                story.id, e
            );
            Vec::new()
        }
    };

    let vars_map = if is_verify {
        // Find paired impl task for verify context
        let impl_task = tasks_for_story
            .iter()
            .find(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Impl)
                    && t.parent_id == task.parent_id
                    && t.sort_order == task.sort_order - 1
            })
            .unwrap_or(task);
        nflow_claude::context::build_verify_context(task, impl_task, &project.name)
    } else {
        // Get completed tasks for context (impl tasks that are done)
        let completed: Vec<_> = tasks_for_story
            .iter()
            .filter(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Impl)
                    && t.status == WorkItemStatus::Done
            })
            .cloned()
            .collect();

        // Check if this is a retry — look for previous failed agent runs
        let previous_error = match db::agent_runs::count_agent_runs_for_task(conn, &task_id) {
            Ok(count) if count > 0 => {
                // Look for the last agent run's error message
                match db::agent_runs::find_latest_agent_run_for_task(conn, &task_id) {
                    Ok(Some(run)) => run.error_message.unwrap_or_default(),
                    _ => String::new(),
                }
            }
            _ => String::new(),
        };

        nflow_claude::context::build_task_context(task, &project.name, &completed, &previous_error)
    };

    // Convert HashMap<String, String> to HashMap<&str, &str> for render_template
    let vars_ref: HashMap<&str, &str> = vars_map
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let rendered_prompt = match nflow_claude::prompt::render_template(&template, &vars_ref) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "start_task: failed to render template for task {}: {}",
                task_id, e
            );
            return;
        }
    };

    // 3. Write rendered context to temp file for --append-system-prompt-file
    let agent_logs_dir = nflow_home
        .join("projects")
        .join(&project.name)
        .join("agent-logs");
    if let Err(e) = std::fs::create_dir_all(&agent_logs_dir) {
        warn!(
            "start_task: failed to create agent-logs dir for task {}: {}",
            task_id, e
        );
        return;
    }

    // Write the rendered prompt to a temp file for system prompt
    let prompt_file_path = agent_logs_dir.join(format!("{}.prompt.md", short_id));
    if let Err(e) = std::fs::write(&prompt_file_path, &rendered_prompt) {
        warn!(
            "start_task: failed to write prompt file for task {}: {}",
            task_id, e
        );
        return;
    }

    // 4. Build RunConfig and spawn Claude process
    let task_prompt = format!(
        "Implement the task as described in the system prompt file. Task: [{}] {}",
        short_id, task.title
    );

    let mut run_config = if is_verify {
        nflow_claude::runner::RunConfig::for_verify_task(task_prompt)
    } else {
        nflow_claude::runner::RunConfig::for_impl_task(task_prompt)
    };
    run_config.working_dir = Some(worktree_path.clone());
    run_config.system_prompt_file = Some(prompt_file_path);

    let runner = nflow_claude::runner::ClaudeRunner::new();
    let process = match runner.spawn(&run_config) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "start_task: failed to spawn Claude for task {}: {}",
                task_id, e
            );
            // Mark task as failed since we couldn't start the agent
            let _ = db::work_items::update_work_item_status(conn, &task_id, WorkItemStatus::Failed);
            if let Ok(Some(item)) = db::work_items::get_work_item_by_id(conn, &task_id) {
                broadcast_status_change(event_bus, &task_id, &item, "in_progress", "failed", conn);
            }
            return;
        }
    };

    let pid = process.pid;
    let pid_start_time = read_process_start_time(pid);

    // 5. Create agent_run record
    let log_path = agent_logs_dir.join(format!("{}.log", short_id));
    let log_path_str = log_path.to_string_lossy().to_string();

    let agent_run = AgentRun {
        id: Uuid::new_v4(),
        work_item_id: task_id,
        pid: Some(pid),
        session_id: None,
        pid_start_time,
        status: AgentRunStatus::Running,
        exit_code: None,
        log_path: Some(log_path_str.clone()),
        error_message: None,
        started_at: Utc::now(),
        finished_at: None,
    };

    if let Err(e) = db::agent_runs::insert_agent_run(conn, &agent_run) {
        warn!(
            "start_task: failed to insert agent_run for task {}: {}",
            task_id, e
        );
        return;
    }

    info!(
        "start_task: spawned Claude agent for task {} (pid={}, log={})",
        short_id, pid, log_path_str
    );

    // 6. Spawn background task to pipe stdout to log file and broadcast AgentOutput events
    let event_bus_clone = event_bus.cloned();
    let task_id_str = task_id.to_string();
    let mut stdout = process.stdout;
    let _stderr = process.stderr;
    // We take ownership of the child but don't wait on it —
    // the reaper (reap_finished_agents) will detect when the process dies.
    let _child = process.child;

    tokio::spawn(async move {
        let log_file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                warn!(
                    "start_task: failed to open log file {}: {}",
                    log_path.display(),
                    e
                );
                return;
            }
        };
        let mut writer = tokio::io::BufWriter::new(log_file);

        while let Ok(Some(line)) = stdout.next_line().await {
            // Write to log file
            use tokio::io::AsyncWriteExt;
            let _ = writer.write_all(line.as_bytes()).await;
            let _ = writer.write_all(b"\n").await;
            let _ = writer.flush().await;

            // Broadcast to subscribed clients
            if let Some(ref bus) = event_bus_clone {
                bus.broadcast(Event::AgentOutput {
                    task_id: task_id_str.clone(),
                    line: line.clone(),
                });
            }
        }
    });
}

/// Execute scheduler actions by updating the DB and creating worktrees.
///
/// Handles three action types:
/// - `MarkStoryReady`: Sets story status to Ready in DB
/// - `UpdateEpicStatus`: Sets epic status in DB
/// - `StartStory`: Creates git worktree, sets story to InProgress,
///   stores branch_name/worktree_path, and marks the first pending task as InProgress
///
/// The `StartStory` action requires async git operations (worktree creation).
/// If worktree creation fails, the story is set to Failed.
pub async fn execute_actions(
    conn: &Connection,
    actions: &[(Uuid, SchedulerAction)],
    event_bus: Option<&SharedEventBus>,
) {
    for (project_id, action) in actions {
        match action {
            SchedulerAction::MarkStoryReady { story_id } => {
                if let Err(e) =
                    db::work_items::update_work_item_status(conn, story_id, WorkItemStatus::Ready)
                {
                    warn!("execute: failed to mark story {} as ready: {}", story_id, e);
                    continue;
                }
                info!("execute: story {} marked as ready", story_id);

                if let Ok(Some(item)) = db::work_items::get_work_item_by_id(conn, story_id) {
                    broadcast_status_change(event_bus, story_id, &item, "pending", "ready", conn);
                }
            }

            SchedulerAction::UpdateEpicStatus {
                epic_id,
                new_status,
            } => {
                let old_status = db::work_items::get_work_item_by_id(conn, epic_id)
                    .ok()
                    .flatten()
                    .map(|i| i.status.to_string())
                    .unwrap_or_default();

                if let Err(e) = db::work_items::update_work_item_status(conn, epic_id, *new_status)
                {
                    warn!("execute: failed to update epic {} status: {}", epic_id, e);
                    continue;
                }
                info!(
                    "execute: epic {} status updated to {:?}",
                    epic_id, new_status
                );

                if let Ok(Some(item)) = db::work_items::get_work_item_by_id(conn, epic_id) {
                    let new_str = match new_status {
                        WorkItemStatus::Pending => "pending",
                        WorkItemStatus::Ready => "ready",
                        WorkItemStatus::InProgress => "in_progress",
                        WorkItemStatus::Done => "done",
                        WorkItemStatus::Failed => "failed",
                        WorkItemStatus::Cancelled => "cancelled",
                    };
                    broadcast_status_change(event_bus, epic_id, &item, &old_status, new_str, conn);
                }
            }

            SchedulerAction::StartStory { story_id } => {
                execute_start_story(conn, project_id, story_id, event_bus).await;
            }
        }
    }
}

/// Execute the StartStory action: create worktree, update story, find first task.
async fn execute_start_story(
    conn: &Connection,
    project_id: &Uuid,
    story_id: &Uuid,
    event_bus: Option<&SharedEventBus>,
) {
    // 1. Load the project
    let project = match db::projects::get_project_by_id(conn, project_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            warn!(
                "execute: project {} not found for story {}",
                project_id, story_id
            );
            return;
        }
        Err(e) => {
            warn!("execute: failed to load project {}: {}", project_id, e);
            return;
        }
    };

    // 2. Load the story
    let story = match db::work_items::get_work_item_by_id(conn, story_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            warn!("execute: story {} not found", story_id);
            return;
        }
        Err(e) => {
            warn!("execute: failed to load story {}: {}", story_id, e);
            return;
        }
    };

    // 3. Generate branch name from template
    let cfg = Config::default();
    let story_slug = slugify(&story.title);
    let branch_name = config::render_branch_template(
        &cfg.branch_template,
        &project.name,
        &story.short_id,
        &story_slug,
    );

    // 4. Construct worktree path: {project_path}/{worktree_dir}/{branch_name}
    let repo_path = Path::new(&project.path);
    let worktree_path: PathBuf = repo_path.join(&cfg.worktree_dir).join(&branch_name);

    // 5. Create worktree (async git operation)
    info!(
        "execute: starting story {} — creating worktree at {}",
        story_id,
        worktree_path.display()
    );

    if let Err(e) =
        nflow_git::worktree::create_worktree(repo_path, &worktree_path, &project.base_branch).await
    {
        warn!(
            "execute: worktree creation failed for story {}: {}",
            story_id, e
        );
        // Set story to failed
        let _ = db::work_items::update_work_item_status(conn, story_id, WorkItemStatus::Failed);
        if let Ok(Some(item)) = db::work_items::get_work_item_by_id(conn, story_id) {
            broadcast_status_change(event_bus, story_id, &item, "ready", "failed", conn);
        }
        return;
    }

    // 6. Store worktree_path and branch_name on story
    if let Err(e) = db::work_items::update_story_worktree(
        conn,
        story_id,
        &branch_name,
        &worktree_path.to_string_lossy(),
    ) {
        warn!(
            "execute: failed to store worktree info for story {}: {}",
            story_id, e
        );
        return;
    }

    // 7. Set story status to InProgress
    if let Err(e) =
        db::work_items::update_work_item_status(conn, story_id, WorkItemStatus::InProgress)
    {
        warn!(
            "execute: failed to set story {} to in_progress: {}",
            story_id, e
        );
        return;
    }
    info!(
        "execute: story {} started — branch={}, worktree={}",
        story_id,
        branch_name,
        worktree_path.display()
    );

    if let Ok(Some(item)) = db::work_items::get_work_item_by_id(conn, story_id) {
        broadcast_status_change(event_bus, story_id, &item, "ready", "in_progress", conn);
    }

    // 8. Find the first pending task (lowest sort_order)
    let tasks = match db::work_items::list_work_items_by_parent(conn, story_id) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                "execute: failed to list tasks for story {}: {}",
                story_id, e
            );
            return;
        }
    };

    let first_task = tasks
        .iter()
        .filter(|t| t.item_type == ItemType::Task && t.status == WorkItemStatus::Pending)
        .min_by_key(|t| t.sort_order);

    if let Some(task) = first_task {
        // Mark first task as InProgress
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &task.id, WorkItemStatus::InProgress)
        {
            warn!(
                "execute: failed to set task {} to in_progress: {}",
                task.id, e
            );
        } else {
            info!(
                "execute: task {} ({}) marked in_progress for story {}",
                task.short_id, task.id, story_id
            );
            broadcast_status_change(event_bus, &task.id, task, "pending", "in_progress", conn);

            // Spawn Claude agent for this task
            // Re-load the story to get the worktree_path that was just set
            let updated_story = db::work_items::get_work_item_by_id(conn, story_id)
                .ok()
                .flatten();
            if let Some(ref story_with_worktree) = updated_story {
                start_task_execution(conn, task, &project, story_with_worktree, event_bus).await;
            }
        }
    } else {
        warn!(
            "execute: no pending tasks found for story {} — story may already be complete",
            story_id
        );
    }
}

/// Runs a single scheduler tick for all projects.
///
/// For each project with `execution_enabled = true`, this function:
/// 1. Reaps finished agent processes (US-053)
/// 2. Determines story progression actions (US-056)
/// 3. Loads all decomposition sessions and their work items + dependencies
/// 4. Counts running agents
/// 5. Builds a `SchedulerState` snapshot
/// 6. Calls the pure `schedule()` algorithm
/// 7. Logs all returned actions at DEBUG level
///
/// Returns (scheduler_actions, story_progress_actions).
/// The caller (daemon main loop) is responsible for executing both sets of actions.
pub fn scheduler_tick(
    conn: &Connection,
    event_bus: Option<&SharedEventBus>,
) -> (Vec<(Uuid, SchedulerAction)>, Vec<StoryProgressAction>) {
    // Phase 0: Reap finished agent processes before scheduling
    let (reaped, progress_actions) = reap_finished_agents(conn, event_bus);
    if reaped > 0 {
        debug!("scheduler: reaped {} finished agent(s)", reaped);
    }
    if !progress_actions.is_empty() {
        debug!(
            "scheduler: {} story progress action(s) from reaping",
            progress_actions.len()
        );
    }

    let projects = match db::projects::list_projects(conn) {
        Ok(p) => p,
        Err(e) => {
            warn!("scheduler: failed to list projects: {}", e);
            return (Vec::new(), progress_actions);
        }
    };

    // Skip if no projects have execution enabled
    let enabled_projects: Vec<_> = projects.iter().filter(|p| p.execution_enabled).collect();

    if enabled_projects.is_empty() {
        debug!("scheduler: no projects with execution_enabled, skipping tick");
        return (Vec::new(), progress_actions);
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

    (all_actions, progress_actions)
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
        let (actions, _progress) = scheduler_tick(&conn, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_skips_projects_without_execution_enabled() {
        let conn = test_conn();
        let project = make_project("disabled", false);
        db::projects::insert_project(&conn, &project).unwrap();

        let (actions, _progress) = scheduler_tick(&conn, None);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_skips_projects_with_no_sessions() {
        let conn = test_conn();
        let mut project = make_project("enabled", true);
        project.execution_enabled = true;
        db::projects::insert_project(&conn, &project).unwrap();

        let (actions, _progress) = scheduler_tick(&conn, None);
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

        let (actions, _progress) = scheduler_tick(&conn, None);

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

        let (actions, _progress) = scheduler_tick(&conn, None);

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
        let (reaped, _progress) = reap_finished_agents(&conn, None);
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

        let (reaped, _progress) = reap_finished_agents(&conn, None);
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

        let (reaped, _progress) = reap_finished_agents(&conn, None);
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

        let (reaped, _progress) = reap_finished_agents(&conn, None);
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

        let (reaped, _progress) = reap_finished_agents(&conn, Some(&bus));
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

    // --- slugify tests ---

    #[test]
    fn slugify_simple_title() {
        assert_eq!(
            slugify("Add user authentication"),
            "add-user-authentication"
        );
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(slugify("Fix bug: login/signup"), "fix-bug-login-signup");
    }

    #[test]
    fn slugify_collapses_hyphens() {
        assert_eq!(slugify("hello   world---test"), "hello-world-test");
    }

    #[test]
    fn slugify_trims_hyphens() {
        assert_eq!(slugify("--hello--"), "hello");
    }

    #[test]
    fn slugify_truncates_long_titles() {
        let long_title = "a".repeat(100);
        let slug = slugify(&long_title);
        assert!(slug.len() <= 50);
    }

    #[test]
    fn slugify_empty_string() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_unicode() {
        // All non-ASCII chars become hyphens, which collapse and get trimmed to empty
        assert_eq!(slugify("Добавить фичу"), "");
    }

    // --- execute_actions tests ---

    #[tokio::test]
    async fn execute_mark_story_ready() {
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

        let actions = vec![(
            project.id,
            SchedulerAction::MarkStoryReady { story_id: story.id },
        )];

        execute_actions(&conn, &actions, None).await;

        let updated = db::work_items::get_work_item_by_id(&conn, &story.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, WorkItemStatus::Ready);
    }

    #[tokio::test]
    async fn execute_update_epic_status() {
        let conn = test_conn();
        let project = make_project("proj", true);
        db::projects::insert_project(&conn, &project).unwrap();

        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "E".into(), "D".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let actions = vec![(
            project.id,
            SchedulerAction::UpdateEpicStatus {
                epic_id: epic.id,
                new_status: WorkItemStatus::InProgress,
            },
        )];

        execute_actions(&conn, &actions, None).await;

        let updated = db::work_items::get_work_item_by_id(&conn, &epic.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, WorkItemStatus::InProgress);
    }

    #[tokio::test]
    async fn execute_mark_ready_broadcasts_event() {
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

        let bus = crate::events::new_event_bus(16);
        let client = crate::events::ClientId::new();
        let mut rx = bus.subscribe(client);

        let actions = vec![(
            project.id,
            SchedulerAction::MarkStoryReady { story_id: story.id },
        )];

        execute_actions(&conn, &actions, Some(&bus)).await;

        let event = rx.try_recv().unwrap();
        match event {
            Event::StatusChange {
                item_id,
                new_status,
                old_status,
                ..
            } => {
                assert_eq!(item_id, story.id.to_string());
                assert_eq!(old_status, "pending");
                assert_eq!(new_status, "ready");
            }
            _ => panic!("expected StatusChange event"),
        }
    }

    // --- story progression tests (US-056) ---

    /// Helper to set up a project + session + epic + story with tasks.
    /// Returns (project, session, epic, story, impl_task, verify_task).
    fn setup_story_with_tasks(
        conn: &Connection,
    ) -> (
        Project,
        DecompositionSession,
        WorkItem,
        WorkItem,
        WorkItem,
        WorkItem,
    ) {
        let project = make_project("proj", true);
        db::projects::insert_project(conn, &project).unwrap();

        let session = make_approved_session(project.id, 1);
        db::decomposition_sessions::insert_decomposition_session(conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "E".into(), "D".into(), "E1".into(), 0);
        db::work_items::insert_work_item(conn, &epic).unwrap();

        let story = WorkItem::new_story(
            epic.id,
            session.id,
            "S".into(),
            "D".into(),
            "AC".into(),
            "S1".into(),
            0,
        );
        db::work_items::insert_work_item(conn, &story).unwrap();

        // Impl task at sort_order 0, verify at sort_order 1
        let impl_task = WorkItem::new_task(
            story.id,
            session.id,
            "Implement feature".into(),
            "D".into(),
            "AC".into(),
            "T1".into(),
            0,
        );
        db::work_items::insert_work_item(conn, &impl_task).unwrap();

        let mut verify_task = WorkItem::new_task(
            story.id,
            session.id,
            "Verify feature".into(),
            "D".into(),
            "AC".into(),
            "T1v".into(),
            1,
        );
        verify_task.kind = Some(TaskKind::Verify);
        db::work_items::insert_work_item(conn, &verify_task).unwrap();

        (project, session, epic, story, impl_task, verify_task)
    }

    #[test]
    fn progress_impl_success_starts_verify_task() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, verify_task) =
            setup_story_with_tasks(&conn);

        // Set story and impl task to in_progress
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::InProgress)
            .unwrap();

        // Mark impl task as done (simulating successful reap)
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Done)
            .unwrap();

        // Now determine progression
        let impl_item = db::work_items::get_work_item_by_id(&conn, &impl_task.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &impl_item, true, None);

        // Should start the verify task
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::StartNextTask {
                task_id, story_id, ..
            } => {
                assert_eq!(task_id, verify_task.id);
                assert_eq!(story_id, story.id);
            }
            other => panic!("expected StartNextTask, got {:?}", other),
        }

        // Verify task should be marked in_progress
        let updated_verify = db::work_items::get_work_item_by_id(&conn, &verify_task.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated_verify.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn progress_verify_success_starts_next_impl_task() {
        let conn = test_conn();
        let (_project, session, _epic, story, impl_task, verify_task) =
            setup_story_with_tasks(&conn);

        // Add a second impl+verify pair
        let impl_task2 = WorkItem::new_task(
            story.id,
            session.id,
            "Implement feature 2".into(),
            "D".into(),
            "AC".into(),
            "T2".into(),
            2,
        );
        db::work_items::insert_work_item(&conn, &impl_task2).unwrap();

        let mut verify_task2 = WorkItem::new_task(
            story.id,
            session.id,
            "Verify feature 2".into(),
            "D".into(),
            "AC".into(),
            "T2v".into(),
            3,
        );
        verify_task2.kind = Some(TaskKind::Verify);
        db::work_items::insert_work_item(&conn, &verify_task2).unwrap();

        // Mark story in_progress, first impl done, first verify done
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Done)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::Done)
            .unwrap();

        // Determine progression after verify task succeeds
        let verify_item = db::work_items::get_work_item_by_id(&conn, &verify_task.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &verify_item, true, None);

        // Should start the second impl task
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::StartNextTask {
                task_id, story_id, ..
            } => {
                assert_eq!(task_id, impl_task2.id);
                assert_eq!(story_id, story.id);
            }
            other => panic!("expected StartNextTask, got {:?}", other),
        }

        // Second impl task should be marked in_progress
        let updated = db::work_items::get_work_item_by_id(&conn, &impl_task2.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn progress_verify_failure_fails_story() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, verify_task) =
            setup_story_with_tasks(&conn);

        // Story in_progress, impl done, verify in_progress
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Done)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::Failed)
            .unwrap();

        let verify_item = db::work_items::get_work_item_by_id(&conn, &verify_task.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &verify_item, false, None);

        // Should fail the story
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::FailStory { story_id } => {
                assert_eq!(story_id, story.id);
            }
            other => panic!("expected FailStory, got {:?}", other),
        }

        // Story should be failed in DB
        let updated_story = db::work_items::get_work_item_by_id(&conn, &story.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated_story.status, WorkItemStatus::Failed);
    }

    #[test]
    fn progress_impl_failure_fails_story() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, _verify_task) =
            setup_story_with_tasks(&conn);

        // Story in_progress, impl in_progress then failed
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Failed)
            .unwrap();

        let impl_item = db::work_items::get_work_item_by_id(&conn, &impl_task.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &impl_item, false, None);

        // Should fail the story
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::FailStory { story_id } => {
                assert_eq!(story_id, story.id);
            }
            other => panic!("expected FailStory, got {:?}", other),
        }

        // Story should be failed
        let updated_story = db::work_items::get_work_item_by_id(&conn, &story.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated_story.status, WorkItemStatus::Failed);
    }

    #[test]
    fn progress_all_tasks_done_completes_story() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, verify_task) =
            setup_story_with_tasks(&conn);

        // Story in_progress, impl done, verify done
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Done)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::Done)
            .unwrap();

        // Determine progression after last verify task succeeds
        let verify_item = db::work_items::get_work_item_by_id(&conn, &verify_task.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &verify_item, true, None);

        // Should complete the story
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::CompleteStory { story_id } => {
                assert_eq!(story_id, story.id);
            }
            other => panic!("expected CompleteStory, got {:?}", other),
        }

        // Story should be done in DB
        let updated_story = db::work_items::get_work_item_by_id(&conn, &story.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated_story.status, WorkItemStatus::Done);
    }

    #[test]
    fn progress_mix_done_and_cancelled_completes_story() {
        let conn = test_conn();
        let (_project, session, _epic, story, impl_task, verify_task) =
            setup_story_with_tasks(&conn);

        // Add a second impl+verify pair that will be cancelled
        let impl_task2 = WorkItem::new_task(
            story.id,
            session.id,
            "Skipped feature".into(),
            "D".into(),
            "AC".into(),
            "T2".into(),
            2,
        );
        db::work_items::insert_work_item(&conn, &impl_task2).unwrap();

        let mut verify_task2 = WorkItem::new_task(
            story.id,
            session.id,
            "Skipped verify".into(),
            "D".into(),
            "AC".into(),
            "T2v".into(),
            3,
        );
        verify_task2.kind = Some(TaskKind::Verify);
        db::work_items::insert_work_item(&conn, &verify_task2).unwrap();

        // Story in_progress, first pair done, second pair cancelled
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Done)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::Done)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task2.id, WorkItemStatus::Cancelled)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task2.id, WorkItemStatus::Cancelled)
            .unwrap();

        // Determine progression after last verify done
        let verify_item = db::work_items::get_work_item_by_id(&conn, &verify_task.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &verify_item, true, None);

        // Should complete the story (mix of done + cancelled)
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::CompleteStory { story_id } => {
                assert_eq!(story_id, story.id);
            }
            other => panic!("expected CompleteStory, got {:?}", other),
        }
    }

    #[test]
    fn progress_impl_success_no_verify_starts_next_impl() {
        let conn = test_conn();
        let (_project, session, _epic, _story, ..) = setup_story_with_tasks(&conn);

        // Remove the verify task (story with impl only — no auto-generated verifies)
        // Create a new story with just two impl tasks, no verifies
        let story2 = WorkItem::new_story(
            _epic.id,
            session.id,
            "S2".into(),
            "D".into(),
            "AC".into(),
            "S2".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story2).unwrap();

        let task_a = WorkItem::new_task(
            story2.id,
            session.id,
            "Task A".into(),
            "D".into(),
            "AC".into(),
            "T3".into(),
            0,
        );
        db::work_items::insert_work_item(&conn, &task_a).unwrap();

        let task_b = WorkItem::new_task(
            story2.id,
            session.id,
            "Task B".into(),
            "D".into(),
            "AC".into(),
            "T4".into(),
            2, // Gap in sort_order (no verify at 1)
        );
        db::work_items::insert_work_item(&conn, &task_b).unwrap();

        // Story in_progress, task_a done
        db::work_items::update_work_item_status(&conn, &story2.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &task_a.id, WorkItemStatus::Done).unwrap();

        let task_a_item = db::work_items::get_work_item_by_id(&conn, &task_a.id)
            .unwrap()
            .unwrap();
        let action = determine_story_progress(&conn, &task_a_item, true, None);

        // Should start task B (next pending task)
        assert!(action.is_some());
        match action.unwrap() {
            StoryProgressAction::StartNextTask {
                task_id, story_id, ..
            } => {
                assert_eq!(task_id, task_b.id);
                assert_eq!(story_id, story2.id);
            }
            other => panic!("expected StartNextTask, got {:?}", other),
        }
    }

    #[test]
    fn reap_dead_impl_triggers_story_fail_action() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, _verify_task) =
            setup_story_with_tasks(&conn);

        // Story and impl task in_progress
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::InProgress)
            .unwrap();

        // Dead agent for impl task
        let run = make_running_agent(impl_task.id, Some(4_000_000_000));
        insert_agent_run(&conn, &run).unwrap();

        let (reaped, progress) = reap_finished_agents(&conn, None);
        assert_eq!(reaped, 1);

        // Should have a FailStory action
        assert_eq!(progress.len(), 1);
        match &progress[0] {
            StoryProgressAction::FailStory { story_id } => {
                assert_eq!(*story_id, story.id);
            }
            other => panic!("expected FailStory, got {:?}", other),
        }

        // Story should be marked as failed
        let updated_story = db::work_items::get_work_item_by_id(&conn, &story.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated_story.status, WorkItemStatus::Failed);
    }

    #[test]
    fn progress_broadcasts_story_failure_event() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, _verify_task) =
            setup_story_with_tasks(&conn);

        // Story and impl in_progress
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Failed)
            .unwrap();

        let bus = crate::events::new_event_bus(16);
        let client = crate::events::ClientId::new();
        let mut rx = bus.subscribe(client);

        let impl_item = db::work_items::get_work_item_by_id(&conn, &impl_task.id)
            .unwrap()
            .unwrap();
        let _action = determine_story_progress(&conn, &impl_item, false, Some(&bus));

        // Should broadcast story failure
        let event = rx.try_recv().unwrap();
        match event {
            Event::StatusChange {
                item_id,
                new_status,
                ..
            } => {
                assert_eq!(item_id, story.id.to_string());
                assert_eq!(new_status, "failed");
            }
            _ => panic!("expected StatusChange event for story failure"),
        }
    }

    #[test]
    fn progress_broadcasts_story_completion_event() {
        let conn = test_conn();
        let (_project, _session, _epic, story, impl_task, verify_task) =
            setup_story_with_tasks(&conn);

        // All tasks done
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &impl_task.id, WorkItemStatus::Done)
            .unwrap();
        db::work_items::update_work_item_status(&conn, &verify_task.id, WorkItemStatus::Done)
            .unwrap();

        let bus = crate::events::new_event_bus(16);
        let client = crate::events::ClientId::new();
        let mut rx = bus.subscribe(client);

        let verify_item = db::work_items::get_work_item_by_id(&conn, &verify_task.id)
            .unwrap()
            .unwrap();
        let _action = determine_story_progress(&conn, &verify_item, true, Some(&bus));

        // Should broadcast story completion
        let event = rx.try_recv().unwrap();
        match event {
            Event::StatusChange {
                item_id,
                new_status,
                ..
            } => {
                assert_eq!(item_id, story.id.to_string());
                assert_eq!(new_status, "done");
            }
            _ => panic!("expected StatusChange event for story completion"),
        }
    }
}
