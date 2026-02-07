use std::collections::HashMap;

use rusqlite::Connection;
use tracing::{debug, warn};

use nflow_core::decomposition::DecompositionStatus;
use nflow_core::scheduler::{schedule, SchedulerAction, SchedulerState, SessionStatus};

use crate::db;

/// Runs a single scheduler tick for all projects.
///
/// For each project with `execution_enabled = true`, this function:
/// 1. Loads all decomposition sessions and their work items + dependencies
/// 2. Counts running agents
/// 3. Builds a `SchedulerState` snapshot
/// 4. Calls the pure `schedule()` algorithm
/// 5. Logs all returned actions at DEBUG level
///
/// Returns the collected actions for all projects (project_id, action).
/// The caller (daemon main loop) is responsible for executing these actions
/// (updating DB, spawning agents, etc.).
pub fn scheduler_tick(conn: &Connection) -> Vec<(uuid::Uuid, SchedulerAction)> {
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
        let session_statuses: HashMap<uuid::Uuid, SessionStatus> = sessions
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
    use crate::db::test_conn;
    use chrono::Utc;
    use nflow_core::decomposition::DecompositionSession;
    use nflow_core::project::{GitProvider, Project};
    use nflow_core::work_item::WorkItem;
    use uuid::Uuid;

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

    #[test]
    fn tick_returns_empty_with_no_projects() {
        let conn = test_conn();
        let actions = scheduler_tick(&conn);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_skips_projects_without_execution_enabled() {
        let conn = test_conn();
        let project = make_project("disabled", false);
        db::projects::insert_project(&conn, &project).unwrap();

        let actions = scheduler_tick(&conn);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_skips_projects_with_no_sessions() {
        let conn = test_conn();
        let mut project = make_project("enabled", true);
        project.execution_enabled = true;
        db::projects::insert_project(&conn, &project).unwrap();

        let actions = scheduler_tick(&conn);
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

        let actions = scheduler_tick(&conn);

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

        let actions = scheduler_tick(&conn);

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
}
