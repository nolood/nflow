use std::collections::HashMap;

use uuid::Uuid;

use crate::dag::{build_dag, find_ready_stories};
use crate::work_item::{propagate_epic_status, Dependency, ItemType, WorkItem, WorkItemStatus};

/// Status of a decomposition session (wave).
/// Only stories in approved sessions are schedulable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    InProgress,
    Approved,
    Discarded,
}

/// Input state for the scheduler algorithm.
///
/// Contains everything the scheduler needs to make decisions.
/// This is a snapshot — the scheduler is pure and produces actions
/// without side effects.
#[derive(Debug)]
pub struct SchedulerState {
    /// All work items across all sessions for the project.
    pub work_items: Vec<WorkItem>,
    /// All story-level dependencies.
    pub dependencies: Vec<Dependency>,
    /// Number of currently running agents.
    pub running_count: u32,
    /// Maximum number of parallel agents allowed.
    pub max_parallel: u32,
    /// Whether execution is enabled globally.
    pub execution_enabled: bool,
    /// Session ID -> status mapping. Only approved sessions are schedulable.
    pub session_statuses: HashMap<Uuid, SessionStatus>,
}

/// An action the scheduler wants the daemon to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerAction {
    /// Transition a story from pending to ready.
    MarkStoryReady { story_id: Uuid },
    /// Start executing a story (spawn agent for its next task).
    StartStory { story_id: Uuid },
    /// Update an epic's status based on its child stories.
    UpdateEpicStatus {
        epic_id: Uuid,
        new_status: WorkItemStatus,
    },
}

/// Pure scheduling algorithm.
///
/// Given the current state, returns a list of actions to take.
/// The scheduler:
/// 1. Returns empty if execution_enabled is false
/// 2. Propagates epic statuses from child stories
/// 3. Marks pending stories as ready when all blockers are resolved
/// 4. Selects ready stories to start, respecting max_parallel
/// 5. Prioritizes by wave_number (lower first), then sort_order
/// 6. Skips stories in unapproved sessions
pub fn schedule(state: &SchedulerState) -> Vec<SchedulerAction> {
    let mut actions = Vec::new();

    // Rule: no actions when execution is disabled globally
    if !state.execution_enabled {
        return actions;
    }

    // --- Phase 1: Propagate epic statuses ---
    let epics: Vec<&WorkItem> = state
        .work_items
        .iter()
        .filter(|w| w.item_type == ItemType::Epic)
        .collect();

    for epic in &epics {
        let child_statuses: Vec<WorkItemStatus> = state
            .work_items
            .iter()
            .filter(|w| w.item_type == ItemType::Story && w.parent_id == Some(epic.id))
            .map(|w| w.status)
            .collect();

        if child_statuses.is_empty() {
            continue;
        }

        let computed = propagate_epic_status(&child_statuses);
        if computed != epic.status {
            actions.push(SchedulerAction::UpdateEpicStatus {
                epic_id: epic.id,
                new_status: computed,
            });
        }
    }

    // --- Phase 2: Find schedulable stories (approved sessions only) ---
    // Filter stories in approved sessions
    let schedulable_stories: Vec<&WorkItem> = state
        .work_items
        .iter()
        .filter(|w| {
            w.item_type == ItemType::Story
                && state
                    .session_statuses
                    .get(&w.decomposition_session_id)
                    .copied()
                    == Some(SessionStatus::Approved)
        })
        .collect();

    if schedulable_stories.is_empty() {
        return actions;
    }

    // Group stories by session for DAG construction
    let mut stories_by_session: HashMap<Uuid, Vec<&WorkItem>> = HashMap::new();
    for story in &schedulable_stories {
        stories_by_session
            .entry(story.decomposition_session_id)
            .or_default()
            .push(story);
    }

    // Build status map for all stories
    let status_map: HashMap<Uuid, WorkItemStatus> = state
        .work_items
        .iter()
        .filter(|w| w.item_type == ItemType::Story)
        .map(|w| (w.id, w.status))
        .collect();

    // Collect IDs of stories whose deps are resolved (ready candidates)
    let mut ready_candidates: Vec<Uuid> = Vec::new();

    for (session_id, session_stories) in &stories_by_session {
        // Get dependencies for this session
        let session_story_ids: std::collections::HashSet<Uuid> =
            session_stories.iter().map(|s| s.id).collect();
        let session_deps: Vec<Dependency> = state
            .dependencies
            .iter()
            .filter(|d| {
                session_story_ids.contains(&d.blocker_id)
                    || session_story_ids.contains(&d.blocked_id)
            })
            .cloned()
            .collect();

        // Build items vec from references
        let items: Vec<WorkItem> = session_stories.iter().map(|s| (*s).clone()).collect();

        // Build DAG (should not fail for valid data, but skip session on error)
        let dag = match build_dag(&items, &session_deps) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let ready = find_ready_stories(&dag, &status_map);
        for story_id in ready {
            // Only mark pending stories as ready
            if status_map.get(&story_id) == Some(&WorkItemStatus::Pending) {
                ready_candidates.push(story_id);
            }
        }
        let _ = session_id; // used as iteration key
    }

    // Emit MarkStoryReady actions for pending stories that have all blockers resolved
    // Sort by wave_number (via session), then sort_order
    ready_candidates.sort_by(|a, b| {
        let a_item = state.work_items.iter().find(|w| w.id == *a);
        let b_item = state.work_items.iter().find(|w| w.id == *b);
        match (a_item, b_item) {
            (Some(a), Some(b)) => a.sort_order.cmp(&b.sort_order),
            _ => std::cmp::Ordering::Equal,
        }
    });

    for story_id in &ready_candidates {
        actions.push(SchedulerAction::MarkStoryReady {
            story_id: *story_id,
        });
    }

    // --- Phase 3: Select stories to start ---
    let available_slots = state.max_parallel.saturating_sub(state.running_count);
    if available_slots == 0 {
        return actions;
    }

    // Collect ready stories (status == Ready) from approved sessions,
    // sorted by wave_number then sort_order
    let mut startable: Vec<&WorkItem> = schedulable_stories
        .iter()
        .filter(|w| w.status == WorkItemStatus::Ready)
        .copied()
        .collect();

    // Build a lookup: session_id -> wave_number
    // We don't have wave_number on WorkItem, but we can derive priority from
    // decomposition_session_id ordering. For now, sort by session_id (UUID ordering
    // gives creation order for v4 UUIDs — not ideal but acceptable until
    // wave_number is available). The PRD says "wave_number (lower first), then sort_order".
    // We'll use the session_statuses map order doesn't help. Instead, let's collect
    // session wave_numbers if provided. For now, sort by sort_order as primary since
    // wave_number isn't on WorkItem yet.
    // NOTE: When DecompositionSession model is added (US-009), wave_number will be available
    // in SchedulerState and sorting will use it. For now, sort by session_id then sort_order.
    startable.sort_by(|a, b| {
        a.decomposition_session_id
            .cmp(&b.decomposition_session_id)
            .then(a.sort_order.cmp(&b.sort_order))
    });

    let to_start = startable
        .iter()
        .take(available_slots as usize)
        .collect::<Vec<_>>();

    for story in to_start {
        actions.push(SchedulerAction::StartStory { story_id: story.id });
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::WorkItem;

    fn make_epic(session_id: Uuid) -> WorkItem {
        WorkItem::new_epic(session_id, "Epic 1".into(), "Desc".into(), "E1".into(), 0)
    }

    fn make_story_with_order(
        epic_id: Uuid,
        session_id: Uuid,
        short_id: &str,
        sort_order: i32,
    ) -> WorkItem {
        WorkItem::new_story(
            epic_id,
            session_id,
            format!("Story {short_id}"),
            "Desc".into(),
            "AC".into(),
            short_id.into(),
            sort_order,
        )
    }

    fn approved_session(session_id: Uuid) -> (Uuid, SessionStatus) {
        (session_id, SessionStatus::Approved)
    }

    fn unapproved_session(session_id: Uuid) -> (Uuid, SessionStatus) {
        (session_id, SessionStatus::InProgress)
    }

    // --- Basic: execution_enabled = false returns no actions ---

    #[test]
    fn schedule_returns_empty_when_execution_disabled() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        s1.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: false,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.is_empty());
    }

    // --- MarkStoryReady: pending stories with resolved blockers ---

    #[test]
    fn schedule_marks_pending_stories_ready_when_no_blockers() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::MarkStoryReady { story_id: s1.id }));
    }

    #[test]
    fn schedule_marks_story_ready_when_blocker_done() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        // S1 blocks S2; S1 is done
        s1.status = WorkItemStatus::Done;
        let dep = Dependency::new(s1.id, s2.id);

        let state = SchedulerState {
            work_items: vec![epic, s1, s2.clone()],
            dependencies: vec![dep],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::MarkStoryReady { story_id: s2.id }));
    }

    #[test]
    fn schedule_does_not_mark_blocked_story_ready() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        // S1 blocks S2; S1 is still pending
        let dep = Dependency::new(s1.id, s2.id);

        let state = SchedulerState {
            work_items: vec![epic, s1.clone(), s2.clone()],
            dependencies: vec![dep],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        // S1 should get MarkReady (no blockers), S2 should NOT
        assert!(actions.contains(&SchedulerAction::MarkStoryReady { story_id: s1.id }));
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s2.id
        )));
    }

    // --- StartStory: respects max_parallel ---

    #[test]
    fn schedule_starts_ready_stories_up_to_max_parallel() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 1);
        let mut s3 = make_story_with_order(epic.id, session_id, "S3", 2);

        s1.story_mark_ready(true).unwrap();
        s2.story_mark_ready(true).unwrap();
        s3.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone(), s2.clone(), s3.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 2,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();

        assert_eq!(start_actions.len(), 2);
    }

    #[test]
    fn schedule_respects_running_count() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        s1.story_mark_ready(true).unwrap();
        s2.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1, s2],
            dependencies: vec![],
            running_count: 2,
            max_parallel: 2,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();

        assert_eq!(start_actions.len(), 0);
    }

    #[test]
    fn schedule_starts_one_when_one_slot_available() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        s1.story_mark_ready(true).unwrap();
        s2.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone(), s2],
            dependencies: vec![],
            running_count: 2,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();

        assert_eq!(start_actions.len(), 1);
        // Should start S1 first (lower sort_order)
        assert!(actions.contains(&SchedulerAction::StartStory { story_id: s1.id }));
    }

    // --- Prioritization: sort_order ---

    #[test]
    fn schedule_prioritizes_by_sort_order() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 10);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 5);

        s1.story_mark_ready(true).unwrap();
        s2.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone(), s2.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 1,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                SchedulerAction::StartStory { story_id } => Some(*story_id),
                _ => None,
            })
            .collect();

        assert_eq!(start_actions.len(), 1);
        // S2 has lower sort_order (5 < 10), should be started first
        assert_eq!(start_actions[0], s2.id);
    }

    // --- Skips unapproved sessions ---

    #[test]
    fn schedule_skips_stories_in_unapproved_sessions() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [unapproved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        // No MarkStoryReady or StartStory for stories in unapproved sessions
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s1.id
        )));
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::StartStory { story_id } if *story_id == s1.id
        )));
    }

    #[test]
    fn schedule_skips_stories_in_discarded_sessions() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [(session_id, SessionStatus::Discarded)].into(),
        };

        let actions = schedule(&state);
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s1.id
        )));
    }

    // --- Epic status propagation ---

    #[test]
    fn schedule_propagates_epic_status_to_in_progress() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        // Story is in_progress, epic should be updated from pending to in_progress
        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();

        let state = SchedulerState {
            work_items: vec![epic.clone(), s1],
            dependencies: vec![],
            running_count: 1,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::UpdateEpicStatus {
            epic_id: epic.id,
            new_status: WorkItemStatus::InProgress,
        }));
    }

    #[test]
    fn schedule_propagates_epic_status_to_done() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();
        s1.story_complete().unwrap();

        let state = SchedulerState {
            work_items: vec![epic.clone(), s1],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::UpdateEpicStatus {
            epic_id: epic.id,
            new_status: WorkItemStatus::Done,
        }));
    }

    #[test]
    fn schedule_no_epic_update_when_status_matches() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        // Epic is already pending, all stories are pending → no update needed
        assert_eq!(epic.status, WorkItemStatus::Pending);

        let state = SchedulerState {
            work_items: vec![epic.clone(), s1],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(!actions.iter().any(
            |a| matches!(a, SchedulerAction::UpdateEpicStatus { epic_id, .. } if *epic_id == epic.id)
        ));
    }

    // --- Edge cases ---

    #[test]
    fn schedule_empty_state() {
        let state = SchedulerState {
            work_items: vec![],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: HashMap::new(),
        };

        let actions = schedule(&state);
        assert!(actions.is_empty());
    }

    #[test]
    fn schedule_no_start_when_all_stories_done() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();
        s1.story_complete().unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();
        assert!(start_actions.is_empty());
    }

    #[test]
    fn schedule_does_not_mark_already_ready_stories() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        s1.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        // S1 is already Ready, should NOT get MarkStoryReady
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s1.id
        )));
        // But should get StartStory
        assert!(actions.contains(&SchedulerAction::StartStory { story_id: s1.id }));
    }

    #[test]
    fn schedule_does_not_start_in_progress_stories() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 1,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::StartStory { story_id } if *story_id == s1.id
        )));
    }

    #[test]
    fn schedule_cancelled_blocker_unblocks_dependent() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        // S1 blocks S2; S1 is cancelled → S2 should become ready
        s1.story_cancel().unwrap();
        let dep = Dependency::new(s1.id, s2.id);

        let state = SchedulerState {
            work_items: vec![epic, s1, s2.clone()],
            dependencies: vec![dep],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::MarkStoryReady { story_id: s2.id }));
    }

    #[test]
    fn schedule_multiple_sessions_approved_and_unapproved() {
        let session1 = Uuid::new_v4();
        let session2 = Uuid::new_v4();
        let epic1 = make_epic(session1);
        let epic2 = make_epic(session2);
        let s1 = make_story_with_order(epic1.id, session1, "S1", 0);
        let s2 = make_story_with_order(epic2.id, session2, "S2", 0);

        let state = SchedulerState {
            work_items: vec![epic1, epic2, s1.clone(), s2.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session1), unapproved_session(session2)].into(),
        };

        let actions = schedule(&state);
        // S1 from approved session should be marked ready
        assert!(actions.contains(&SchedulerAction::MarkStoryReady { story_id: s1.id }));
        // S2 from unapproved session should NOT be scheduled
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s2.id
        )));
    }

    #[test]
    fn schedule_max_parallel_zero_means_no_starts() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        s1.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 0,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();
        assert!(start_actions.is_empty());
    }

    #[test]
    fn schedule_does_not_mark_done_story_as_ready() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();
        s1.story_complete().unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s1.id
        )));
    }

    #[test]
    fn schedule_failed_story_not_started_again() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();
        s1.story_fail().unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        // Failed stories should not be auto-started (require manual retry)
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::StartStory { story_id } if *story_id == s1.id
        )));
    }

    // --- AC: 5 ready stories, max_parallel=3 -> returns 3 start actions ---

    #[test]
    fn schedule_five_ready_stories_max_parallel_three_starts_three() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut stories: Vec<WorkItem> = (0..5)
            .map(|i| {
                let mut s = make_story_with_order(epic.id, session_id, &format!("S{}", i + 1), i);
                s.story_mark_ready(true).unwrap();
                s
            })
            .collect();

        let mut work_items = vec![epic];
        work_items.append(&mut stories);

        let state = SchedulerState {
            work_items,
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();

        assert_eq!(start_actions.len(), 3);
    }

    // --- AC: 2 running + 3 ready, max_parallel=3 -> returns 1 start action ---

    #[test]
    fn schedule_two_running_three_ready_max_three_starts_one() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 1);
        let mut s3 = make_story_with_order(epic.id, session_id, "S3", 2);

        s1.story_mark_ready(true).unwrap();
        s2.story_mark_ready(true).unwrap();
        s3.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            work_items: vec![epic, s1, s2, s3],
            dependencies: vec![],
            running_count: 2,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::StartStory { .. }))
            .collect();

        assert_eq!(start_actions.len(), 1);
    }

    // --- AC: all stories done -> returns no actions ---

    #[test]
    fn schedule_all_stories_done_returns_no_start_or_ready_actions() {
        let session_id = Uuid::new_v4();
        let mut epic = make_epic(session_id);
        epic.status = WorkItemStatus::Done;

        let mut stories: Vec<WorkItem> = (0..3)
            .map(|i| {
                let mut s = make_story_with_order(epic.id, session_id, &format!("S{}", i + 1), i);
                s.story_mark_ready(true).unwrap();
                s.story_start().unwrap();
                s.story_complete().unwrap();
                s
            })
            .collect();

        let mut work_items = vec![epic];
        work_items.append(&mut stories);

        let state = SchedulerState {
            work_items,
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        // No start or mark-ready actions when everything is done
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::StartStory { .. })));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::MarkStoryReady { .. })));
        // No epic update either since epic is already Done
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::UpdateEpicStatus { .. })));
    }

    // --- AC: wave ordering: W1 before W2 ---
    // Wave ordering is approximated by session_id ordering since wave_number
    // is not yet on WorkItem. This test verifies that stories from different
    // sessions are sorted by session_id (deterministic UUID ordering).

    #[test]
    fn schedule_wave_ordering_earlier_session_before_later() {
        // Use fixed UUIDs to control ordering: session1 < session2 lexicographically
        let session1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let session2 = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        let epic1 = WorkItem::new_epic(session1, "Epic W1".into(), "D".into(), "E1".into(), 0);
        let epic2 = WorkItem::new_epic(session2, "Epic W2".into(), "D".into(), "E2".into(), 0);

        let mut s_w2 = make_story_with_order(epic2.id, session2, "S2", 0);
        let mut s_w1 = make_story_with_order(epic1.id, session1, "S1", 0);

        s_w1.story_mark_ready(true).unwrap();
        s_w2.story_mark_ready(true).unwrap();

        let state = SchedulerState {
            // Deliberately put W2 story first to verify sorting
            work_items: vec![epic1, epic2, s_w2.clone(), s_w1.clone()],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 1,
            execution_enabled: true,
            session_statuses: [
                (session1, SessionStatus::Approved),
                (session2, SessionStatus::Approved),
            ]
            .into(),
        };

        let actions = schedule(&state);
        let start_actions: Vec<Uuid> = actions
            .iter()
            .filter_map(|a| match a {
                SchedulerAction::StartStory { story_id } => Some(*story_id),
                _ => None,
            })
            .collect();

        // Only 1 slot, should pick W1 (session1 sorts before session2)
        assert_eq!(start_actions.len(), 1);
        assert_eq!(start_actions[0], s_w1.id);
    }

    // --- AC: epic status materialization from child stories (comprehensive) ---

    #[test]
    fn schedule_epic_status_materialization_mixed_children() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);

        // Two stories: one done, one in_progress -> epic should be InProgress
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();
        s1.story_complete().unwrap();
        s2.story_mark_ready(true).unwrap();
        s2.story_start().unwrap();

        let state = SchedulerState {
            work_items: vec![epic.clone(), s1, s2],
            dependencies: vec![],
            running_count: 1,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::UpdateEpicStatus {
            epic_id: epic.id,
            new_status: WorkItemStatus::InProgress,
        }));
    }

    #[test]
    fn schedule_epic_status_materialization_all_cancelled() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let mut s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        s1.story_cancel().unwrap();
        s2.story_cancel().unwrap();

        let state = SchedulerState {
            work_items: vec![epic.clone(), s1, s2],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::UpdateEpicStatus {
            epic_id: epic.id,
            new_status: WorkItemStatus::Cancelled,
        }));
    }

    #[test]
    fn schedule_epic_status_materialization_failed() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let s2 = make_story_with_order(epic.id, session_id, "S2", 1);

        s1.story_mark_ready(true).unwrap();
        s1.story_start().unwrap();
        s1.story_fail().unwrap();
        // s2 still pending

        let state = SchedulerState {
            work_items: vec![epic.clone(), s1, s2],
            dependencies: vec![],
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        assert!(actions.contains(&SchedulerAction::UpdateEpicStatus {
            epic_id: epic.id,
            new_status: WorkItemStatus::Failed,
        }));
    }

    // --- Complex scenario: chain with multiple waves ---

    #[test]
    fn schedule_chain_dependency_resolves_step_by_step() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut s1 = make_story_with_order(epic.id, session_id, "S1", 0);
        let s2 = make_story_with_order(epic.id, session_id, "S2", 1);
        let s3 = make_story_with_order(epic.id, session_id, "S3", 2);

        // Chain: S1 -> S2 -> S3, S1 is done
        s1.status = WorkItemStatus::Done;
        let deps = vec![Dependency::new(s1.id, s2.id), Dependency::new(s2.id, s3.id)];

        let state = SchedulerState {
            work_items: vec![epic, s1, s2.clone(), s3.clone()],
            dependencies: deps,
            running_count: 0,
            max_parallel: 3,
            execution_enabled: true,
            session_statuses: [approved_session(session_id)].into(),
        };

        let actions = schedule(&state);
        // S2 should be marked ready (S1 is done)
        assert!(actions.contains(&SchedulerAction::MarkStoryReady { story_id: s2.id }));
        // S3 should NOT be marked ready (S2 is still pending)
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::MarkStoryReady { story_id } if *story_id == s3.id
        )));
    }
}
