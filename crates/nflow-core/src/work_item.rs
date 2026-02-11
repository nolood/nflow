use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{NflowError, Result};

/// Type of work item in the hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemType {
    Epic,
    Story,
    Task,
}

impl fmt::Display for ItemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemType::Epic => write!(f, "epic"),
            ItemType::Story => write!(f, "story"),
            ItemType::Task => write!(f, "task"),
        }
    }
}

/// Kind of task — impl or verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Impl,
    Verify,
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskKind::Impl => write!(f, "impl"),
            TaskKind::Verify => write!(f, "verify"),
        }
    }
}

/// Status for work items. Used by epics, stories, and tasks.
/// Not all statuses are valid for all item types — enforcement is via state machine methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkItemStatus {
    Pending,
    Ready,
    InProgress,
    Done,
    Failed,
    Cancelled,
}

impl fmt::Display for WorkItemStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkItemStatus::Pending => write!(f, "pending"),
            WorkItemStatus::Ready => write!(f, "ready"),
            WorkItemStatus::InProgress => write!(f, "in_progress"),
            WorkItemStatus::Done => write!(f, "done"),
            WorkItemStatus::Failed => write!(f, "failed"),
            WorkItemStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A work item in the epic → story → task hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub decomposition_session_id: Uuid,
    pub item_type: ItemType,
    pub kind: Option<TaskKind>,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    pub status: WorkItemStatus,
    pub short_id: String,
    pub sort_order: i32,
    pub branch_name: Option<String>,
    pub worktree_path: Option<String>,
    pub mr_url: Option<String>,
    pub commit_hash: Option<String>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A story-level dependency: `blocker` must be done before `blocked` can become ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub blocker_id: Uuid,
    pub blocked_id: Uuid,
}

impl Dependency {
    pub fn new(blocker_id: Uuid, blocked_id: Uuid) -> Self {
        Self {
            blocker_id,
            blocked_id,
        }
    }
}

impl WorkItem {
    /// Create a new epic.
    pub fn new_epic(
        decomposition_session_id: Uuid,
        title: String,
        description: String,
        short_id: String,
        sort_order: i32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            parent_id: None,
            decomposition_session_id,
            item_type: ItemType::Epic,
            kind: None,
            title,
            description,
            acceptance_criteria: String::new(),
            status: WorkItemStatus::Pending,
            short_id,
            sort_order,
            branch_name: None,
            worktree_path: None,
            mr_url: None,
            commit_hash: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a new story under an epic.
    pub fn new_story(
        parent_id: Uuid,
        decomposition_session_id: Uuid,
        title: String,
        description: String,
        acceptance_criteria: String,
        short_id: String,
        sort_order: i32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            parent_id: Some(parent_id),
            decomposition_session_id,
            item_type: ItemType::Story,
            kind: None,
            title,
            description,
            acceptance_criteria,
            status: WorkItemStatus::Pending,
            short_id,
            sort_order,
            branch_name: None,
            worktree_path: None,
            mr_url: None,
            commit_hash: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        }
    }

    // --- Story state machine ---

    /// Transition a story to ready.
    /// Valid: Pending -> Ready (requires all blockers done, checked by caller via `all_blockers_done`).
    pub fn story_mark_ready(&mut self, all_blockers_done: bool) -> Result<()> {
        self.assert_story()?;
        if self.status != WorkItemStatus::Pending {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "ready".into(),
            });
        }
        if !all_blockers_done {
            return Err(NflowError::InvalidState(
                "cannot mark story ready: not all blockers are done".into(),
            ));
        }
        self.status = WorkItemStatus::Ready;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Transition a story to in_progress.
    /// Valid: Ready -> InProgress, Failed -> InProgress (retry).
    pub fn story_start(&mut self) -> Result<()> {
        self.assert_story()?;
        match self.status {
            WorkItemStatus::Ready | WorkItemStatus::Failed => {
                self.status = WorkItemStatus::InProgress;
                self.updated_at = Utc::now();
                Ok(())
            }
            _ => Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "in_progress".into(),
            }),
        }
    }

    /// Mark a story as done.
    /// Valid: InProgress -> Done.
    pub fn story_complete(&mut self) -> Result<()> {
        self.assert_story()?;
        if self.status != WorkItemStatus::InProgress {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "done".into(),
            });
        }
        self.status = WorkItemStatus::Done;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark a story as failed.
    /// Valid: InProgress -> Failed.
    pub fn story_fail(&mut self) -> Result<()> {
        self.assert_story()?;
        if self.status != WorkItemStatus::InProgress {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "failed".into(),
            });
        }
        self.status = WorkItemStatus::Failed;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Cancel a story.
    /// Valid: Pending | Ready | InProgress -> Cancelled.
    pub fn story_cancel(&mut self) -> Result<()> {
        self.assert_story()?;
        match self.status {
            WorkItemStatus::Pending | WorkItemStatus::Ready | WorkItemStatus::InProgress => {
                self.status = WorkItemStatus::Cancelled;
                self.updated_at = Utc::now();
                Ok(())
            }
            _ => Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "cancelled".into(),
            }),
        }
    }

    fn assert_story(&self) -> Result<()> {
        if self.item_type != ItemType::Story {
            return Err(NflowError::InvalidState(format!(
                "expected story, got {}",
                self.item_type
            )));
        }
        Ok(())
    }

    fn assert_task(&self) -> Result<()> {
        if self.item_type != ItemType::Task {
            return Err(NflowError::InvalidState(format!(
                "expected task, got {}",
                self.item_type
            )));
        }
        Ok(())
    }

    // --- Task constructors ---

    /// Create a new impl task under a story.
    pub fn new_task(
        parent_id: Uuid,
        decomposition_session_id: Uuid,
        title: String,
        description: String,
        acceptance_criteria: String,
        short_id: String,
        sort_order: i32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            parent_id: Some(parent_id),
            decomposition_session_id,
            item_type: ItemType::Task,
            kind: Some(TaskKind::Impl),
            title,
            description,
            acceptance_criteria,
            status: WorkItemStatus::Pending,
            short_id,
            sort_order,
            branch_name: None,
            worktree_path: None,
            mr_url: None,
            commit_hash: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        }
    }

    // --- Task state machine ---

    /// Start a task.
    /// Valid: Pending -> InProgress, Failed -> InProgress (retry).
    pub fn task_start(&mut self) -> Result<()> {
        self.assert_task()?;
        match self.status {
            WorkItemStatus::Pending | WorkItemStatus::Failed => {
                self.status = WorkItemStatus::InProgress;
                self.updated_at = Utc::now();
                Ok(())
            }
            _ => Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "in_progress".into(),
            }),
        }
    }

    /// Mark a task as done with optional commit hash (for impl tasks).
    /// Valid: InProgress -> Done.
    pub fn task_complete(&mut self, commit_hash: Option<String>) -> Result<()> {
        self.assert_task()?;
        if self.status != WorkItemStatus::InProgress {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "done".into(),
            });
        }
        self.status = WorkItemStatus::Done;
        if commit_hash.is_some() {
            self.commit_hash = commit_hash;
        }
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark a task as failed.
    /// Valid: InProgress -> Failed.
    pub fn task_fail(&mut self) -> Result<()> {
        self.assert_task()?;
        if self.status != WorkItemStatus::InProgress {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "failed".into(),
            });
        }
        self.status = WorkItemStatus::Failed;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Cancel a task.
    /// Valid: Pending -> Cancelled.
    pub fn task_cancel(&mut self) -> Result<()> {
        self.assert_task()?;
        if self.status != WorkItemStatus::Pending {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "cancelled".into(),
            });
        }
        self.status = WorkItemStatus::Cancelled;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Skip a failed task (mark as done with skip metadata).
    /// Valid: Failed -> Done.
    pub fn task_skip(&mut self) -> Result<()> {
        self.assert_task()?;
        if self.status != WorkItemStatus::Failed {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "done".into(),
            });
        }
        self.status = WorkItemStatus::Done;
        self.updated_at = Utc::now();
        Ok(())
    }
}

/// Generate verify tasks for a list of impl tasks.
/// For each impl task, inserts a verify task immediately after it (sort_order = impl.sort_order + 1).
/// Verify task short_id = "{impl_short_id}v".
/// Returns the list of newly created verify tasks.
pub fn auto_generate_verify_tasks(impl_tasks: &[WorkItem]) -> Vec<WorkItem> {
    impl_tasks
        .iter()
        .filter(|t| t.item_type == ItemType::Task && t.kind == Some(TaskKind::Impl))
        .map(|impl_task| {
            let now = Utc::now();
            WorkItem {
                id: Uuid::new_v4(),
                parent_id: impl_task.parent_id,
                decomposition_session_id: impl_task.decomposition_session_id,
                item_type: ItemType::Task,
                kind: Some(TaskKind::Verify),
                title: format!("Verify: {}", impl_task.title),
                description: format!(
                    "Verify that '{}' was implemented correctly.",
                    impl_task.title
                ),
                acceptance_criteria: String::new(),
                status: WorkItemStatus::Pending,
                short_id: format!("{}v", impl_task.short_id),
                sort_order: impl_task.sort_order + 1,
                branch_name: None,
                worktree_path: None,
                mr_url: None,
                commit_hash: None,
                error_message: None,
                created_at: now,
                updated_at: now,
            }
        })
        .collect()
}

/// Skip an impl task and its paired verify task.
/// Marks both as done (skipped). Returns error if the impl task can't be skipped.
pub fn skip_task(impl_task: &mut WorkItem, tasks: &mut [WorkItem]) -> Result<()> {
    impl_task.task_skip()?;
    // Find and skip the paired verify task
    if let Some(verify) = find_paired_verify_mut(impl_task, tasks) {
        // Verify task may be pending — cancel it, or if failed, skip it
        match verify.status {
            WorkItemStatus::Pending => {
                verify.task_cancel()?;
            }
            WorkItemStatus::Failed => {
                verify.task_skip()?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Find the paired verify task for an impl task.
/// Matches by parent_id and sort_order = impl.sort_order + 1 with kind=Verify.
pub fn find_paired_verify<'a>(impl_task: &WorkItem, tasks: &'a [WorkItem]) -> Option<&'a WorkItem> {
    tasks.iter().find(|t| {
        t.item_type == ItemType::Task
            && t.kind == Some(TaskKind::Verify)
            && t.parent_id == impl_task.parent_id
            && t.sort_order == impl_task.sort_order + 1
    })
}

/// Find the paired verify task (mutable) for an impl task.
fn find_paired_verify_mut<'a>(
    impl_task: &WorkItem,
    tasks: &'a mut [WorkItem],
) -> Option<&'a mut WorkItem> {
    let parent_id = impl_task.parent_id;
    let expected_sort_order = impl_task.sort_order + 1;
    tasks.iter_mut().find(|t| {
        t.item_type == ItemType::Task
            && t.kind == Some(TaskKind::Verify)
            && t.parent_id == parent_id
            && t.sort_order == expected_sort_order
    })
}

/// Find the paired impl task for a verify task.
/// Matches by parent_id and sort_order = verify.sort_order - 1 with kind=Impl.
pub fn find_paired_impl<'a>(verify_task: &WorkItem, tasks: &'a [WorkItem]) -> Option<&'a WorkItem> {
    tasks.iter().find(|t| {
        t.item_type == ItemType::Task
            && t.kind == Some(TaskKind::Impl)
            && t.parent_id == verify_task.parent_id
            && t.sort_order == verify_task.sort_order - 1
    })
}

/// Get the next pending task by sort_order within a story.
/// Returns the task with the lowest sort_order that is still pending.
pub fn get_next_pending_task(story_id: Uuid, tasks: &[WorkItem]) -> Option<&WorkItem> {
    tasks
        .iter()
        .filter(|t| {
            t.item_type == ItemType::Task
                && t.parent_id == Some(story_id)
                && t.status == WorkItemStatus::Pending
        })
        .min_by_key(|t| t.sort_order)
}

/// Compute the materialized epic status from child story statuses.
///
/// Rules:
/// - `Pending` when all stories are pending
/// - `InProgress` when any story is in_progress or ready
/// - `Done` when all stories are done (or mix of done+cancelled)
/// - `Failed` when any story is failed and none is in_progress
/// - `Cancelled` when all stories are cancelled
pub fn propagate_epic_status(story_statuses: &[WorkItemStatus]) -> WorkItemStatus {
    if story_statuses.is_empty() {
        return WorkItemStatus::Pending;
    }

    let all_pending = story_statuses.iter().all(|s| *s == WorkItemStatus::Pending);
    if all_pending {
        return WorkItemStatus::Pending;
    }

    let all_cancelled = story_statuses
        .iter()
        .all(|s| *s == WorkItemStatus::Cancelled);
    if all_cancelled {
        return WorkItemStatus::Cancelled;
    }

    let any_in_progress_or_ready = story_statuses
        .iter()
        .any(|s| *s == WorkItemStatus::InProgress || *s == WorkItemStatus::Ready);
    if any_in_progress_or_ready {
        return WorkItemStatus::InProgress;
    }

    let any_failed = story_statuses.contains(&WorkItemStatus::Failed);
    if any_failed {
        return WorkItemStatus::Failed;
    }

    // All are done or cancelled (and not all cancelled — checked above)
    let all_done_or_cancelled = story_statuses
        .iter()
        .all(|s| *s == WorkItemStatus::Done || *s == WorkItemStatus::Cancelled);
    if all_done_or_cancelled {
        return WorkItemStatus::Done;
    }

    // Fallback: mix of pending and done/cancelled without in_progress/ready/failed
    // This means some are pending and some are done/cancelled → still in progress
    WorkItemStatus::InProgress
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_epic(session_id: Uuid) -> WorkItem {
        WorkItem::new_epic(session_id, "Epic 1".into(), "Desc".into(), "E1".into(), 0)
    }

    fn make_story(epic_id: Uuid, session_id: Uuid) -> WorkItem {
        WorkItem::new_story(
            epic_id,
            session_id,
            "Story 1".into(),
            "Desc".into(),
            "AC".into(),
            "S1".into(),
            0,
        )
    }

    // --- WorkItem struct fields ---

    #[test]
    fn work_item_has_all_required_fields() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);

        assert_eq!(epic.item_type, ItemType::Epic);
        assert!(epic.parent_id.is_none());
        assert_eq!(epic.decomposition_session_id, session_id);
        assert!(epic.kind.is_none());
        assert_eq!(epic.title, "Epic 1");
        assert_eq!(epic.description, "Desc");
        assert!(epic.acceptance_criteria.is_empty());
        assert_eq!(epic.status, WorkItemStatus::Pending);
        assert_eq!(epic.short_id, "E1");
        assert_eq!(epic.sort_order, 0);
        assert!(epic.branch_name.is_none());
        assert!(epic.worktree_path.is_none());
        assert!(epic.mr_url.is_none());
        assert!(epic.commit_hash.is_none());
    }

    #[test]
    fn story_has_parent_id() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let story = make_story(epic.id, session_id);

        assert_eq!(story.parent_id, Some(epic.id));
        assert_eq!(story.item_type, ItemType::Story);
        assert_eq!(story.status, WorkItemStatus::Pending);
    }

    // --- Story state machine: happy paths ---

    #[test]
    fn story_pending_to_ready() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        assert!(story.story_mark_ready(true).is_ok());
        assert_eq!(story.status, WorkItemStatus::Ready);
    }

    #[test]
    fn story_ready_to_in_progress() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        assert!(story.story_start().is_ok());
        assert_eq!(story.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn story_in_progress_to_done() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        assert!(story.story_complete().is_ok());
        assert_eq!(story.status, WorkItemStatus::Done);
    }

    #[test]
    fn story_in_progress_to_failed() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        assert!(story.story_fail().is_ok());
        assert_eq!(story.status, WorkItemStatus::Failed);
    }

    #[test]
    fn story_failed_to_in_progress_retry() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        story.story_fail().unwrap();
        assert!(story.story_start().is_ok());
        assert_eq!(story.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn story_cancel_from_pending() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        assert!(story.story_cancel().is_ok());
        assert_eq!(story.status, WorkItemStatus::Cancelled);
    }

    #[test]
    fn story_cancel_from_ready() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        assert!(story.story_cancel().is_ok());
        assert_eq!(story.status, WorkItemStatus::Cancelled);
    }

    #[test]
    fn story_cancel_from_in_progress() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        assert!(story.story_cancel().is_ok());
        assert_eq!(story.status, WorkItemStatus::Cancelled);
    }

    // --- Story state machine: invalid transitions ---

    #[test]
    fn story_ready_requires_all_blockers_done() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        let err = story.story_mark_ready(false).unwrap_err();
        assert!(matches!(err, NflowError::InvalidState(_)));
    }

    #[test]
    fn story_ready_from_non_pending_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        let err = story.story_mark_ready(true).unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_start_from_pending_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        let err = story.story_start().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_start_from_done_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        story.story_complete().unwrap();
        let err = story.story_start().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_complete_from_pending_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        let err = story.story_complete().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_fail_from_pending_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        let err = story.story_fail().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_cancel_from_done_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        story.story_complete().unwrap();
        let err = story.story_cancel().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_cancel_from_failed_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_mark_ready(true).unwrap();
        story.story_start().unwrap();
        story.story_fail().unwrap();
        let err = story.story_cancel().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn story_cancel_from_cancelled_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_cancel().unwrap();
        let err = story.story_cancel().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    // --- Story methods on non-story types fail ---

    #[test]
    fn story_methods_on_epic_fail() {
        let session_id = Uuid::new_v4();
        let mut epic = make_epic(session_id);

        assert!(matches!(
            epic.story_mark_ready(true).unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.story_start().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.story_complete().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.story_fail().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.story_cancel().unwrap_err(),
            NflowError::InvalidState(_)
        ));
    }

    // --- propagate_epic_status ---

    #[test]
    fn epic_status_empty_stories_is_pending() {
        assert_eq!(propagate_epic_status(&[]), WorkItemStatus::Pending);
    }

    #[test]
    fn epic_status_all_pending() {
        let statuses = vec![WorkItemStatus::Pending, WorkItemStatus::Pending];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Pending);
    }

    #[test]
    fn epic_status_any_in_progress() {
        let statuses = vec![WorkItemStatus::Pending, WorkItemStatus::InProgress];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::InProgress);
    }

    #[test]
    fn epic_status_any_ready() {
        let statuses = vec![WorkItemStatus::Pending, WorkItemStatus::Ready];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::InProgress);
    }

    #[test]
    fn epic_status_all_done() {
        let statuses = vec![WorkItemStatus::Done, WorkItemStatus::Done];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Done);
    }

    #[test]
    fn epic_status_done_and_cancelled_is_done() {
        let statuses = vec![WorkItemStatus::Done, WorkItemStatus::Cancelled];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Done);
    }

    #[test]
    fn epic_status_all_cancelled() {
        let statuses = vec![WorkItemStatus::Cancelled, WorkItemStatus::Cancelled];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Cancelled);
    }

    #[test]
    fn epic_status_any_failed_none_in_progress() {
        // Failed + Done + Pending: no in_progress stories → Failed per AC rules
        let statuses = vec![
            WorkItemStatus::Done,
            WorkItemStatus::Failed,
            WorkItemStatus::Pending,
        ];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Failed);
    }

    #[test]
    fn epic_status_failed_and_done_only() {
        let statuses = vec![WorkItemStatus::Done, WorkItemStatus::Failed];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Failed);
    }

    #[test]
    fn epic_status_failed_and_cancelled() {
        let statuses = vec![WorkItemStatus::Failed, WorkItemStatus::Cancelled];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::Failed);
    }

    #[test]
    fn epic_status_in_progress_overrides_failed() {
        let statuses = vec![WorkItemStatus::Failed, WorkItemStatus::InProgress];
        assert_eq!(propagate_epic_status(&statuses), WorkItemStatus::InProgress);
    }

    // --- Dependency model ---

    #[test]
    fn dependency_creation() {
        let blocker = Uuid::new_v4();
        let blocked = Uuid::new_v4();
        let dep = Dependency::new(blocker, blocked);

        assert_eq!(dep.blocker_id, blocker);
        assert_eq!(dep.blocked_id, blocked);
    }

    #[test]
    fn dependency_equality() {
        let blocker = Uuid::new_v4();
        let blocked = Uuid::new_v4();
        let dep1 = Dependency::new(blocker, blocked);
        let dep2 = Dependency::new(blocker, blocked);

        assert_eq!(dep1, dep2);
    }

    // --- updated_at changes on story mutations ---

    #[test]
    fn story_transitions_update_timestamp() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        let t0 = story.updated_at;
        story.story_mark_ready(true).unwrap();
        assert!(story.updated_at >= t0);

        let t1 = story.updated_at;
        story.story_start().unwrap();
        assert!(story.updated_at >= t1);

        let t2 = story.updated_at;
        story.story_complete().unwrap();
        assert!(story.updated_at >= t2);
    }

    // =========================================================
    // US-005: Task (impl and verify) tests
    // =========================================================

    fn make_impl_task(
        story_id: Uuid,
        session_id: Uuid,
        short_id: &str,
        sort_order: i32,
    ) -> WorkItem {
        WorkItem::new_task(
            story_id,
            session_id,
            format!("Task {short_id}"),
            "Desc".into(),
            "AC".into(),
            short_id.into(),
            sort_order,
        )
    }

    // --- Task constructor ---

    #[test]
    fn new_task_has_correct_fields() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let task = make_impl_task(story_id, session_id, "T1", 0);

        assert_eq!(task.item_type, ItemType::Task);
        assert_eq!(task.kind, Some(TaskKind::Impl));
        assert_eq!(task.parent_id, Some(story_id));
        assert_eq!(task.decomposition_session_id, session_id);
        assert_eq!(task.status, WorkItemStatus::Pending);
        assert_eq!(task.short_id, "T1");
        assert_eq!(task.sort_order, 0);
        assert!(task.commit_hash.is_none());
    }

    // --- Task state machine: happy paths ---

    #[test]
    fn task_pending_to_in_progress() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        assert!(task.task_start().is_ok());
        assert_eq!(task.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn task_in_progress_to_done() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        assert!(task.task_complete(None).is_ok());
        assert_eq!(task.status, WorkItemStatus::Done);
    }

    #[test]
    fn task_in_progress_to_done_with_commit_hash() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        assert!(task.task_complete(Some("abc123".into())).is_ok());
        assert_eq!(task.status, WorkItemStatus::Done);
        assert_eq!(task.commit_hash, Some("abc123".into()));
    }

    #[test]
    fn task_in_progress_to_failed() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        assert!(task.task_fail().is_ok());
        assert_eq!(task.status, WorkItemStatus::Failed);
    }

    #[test]
    fn task_failed_to_in_progress_retry() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        task.task_fail().unwrap();
        assert!(task.task_start().is_ok());
        assert_eq!(task.status, WorkItemStatus::InProgress);
    }

    #[test]
    fn task_pending_to_cancelled() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        assert!(task.task_cancel().is_ok());
        assert_eq!(task.status, WorkItemStatus::Cancelled);
    }

    #[test]
    fn task_failed_to_done_skip() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        task.task_fail().unwrap();
        assert!(task.task_skip().is_ok());
        assert_eq!(task.status, WorkItemStatus::Done);
    }

    // --- Task state machine: invalid transitions ---

    #[test]
    fn task_start_from_done_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        task.task_complete(None).unwrap();
        let err = task.task_start().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_start_from_cancelled_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_cancel().unwrap();
        let err = task.task_start().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_complete_from_pending_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        let err = task.task_complete(None).unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_fail_from_pending_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        let err = task.task_fail().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_cancel_from_in_progress_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        let err = task.task_cancel().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_skip_from_pending_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        let err = task.task_skip().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_skip_from_in_progress_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_start().unwrap();
        let err = task.task_skip().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    // --- Task methods on non-task types fail ---

    #[test]
    fn task_methods_on_story_fail() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        assert!(matches!(
            story.task_start().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            story.task_complete(None).unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            story.task_fail().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            story.task_cancel().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            story.task_skip().unwrap_err(),
            NflowError::InvalidState(_)
        ));
    }

    #[test]
    fn task_methods_on_epic_fail() {
        let session_id = Uuid::new_v4();
        let mut epic = make_epic(session_id);

        assert!(matches!(
            epic.task_start().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.task_complete(None).unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.task_fail().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.task_cancel().unwrap_err(),
            NflowError::InvalidState(_)
        ));
        assert!(matches!(
            epic.task_skip().unwrap_err(),
            NflowError::InvalidState(_)
        ));
    }

    // --- auto_generate_verify_tasks ---

    #[test]
    fn auto_generate_verify_tasks_creates_verify_for_each_impl() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let t1 = make_impl_task(story_id, session_id, "T1", 0);
        let t2 = make_impl_task(story_id, session_id, "T2", 2);

        let verify_tasks = auto_generate_verify_tasks(&[t1.clone(), t2.clone()]);

        assert_eq!(verify_tasks.len(), 2);

        assert_eq!(verify_tasks[0].kind, Some(TaskKind::Verify));
        assert_eq!(verify_tasks[0].short_id, "T1v");
        assert_eq!(verify_tasks[0].sort_order, 1);
        assert_eq!(verify_tasks[0].parent_id, Some(story_id));

        assert_eq!(verify_tasks[1].kind, Some(TaskKind::Verify));
        assert_eq!(verify_tasks[1].short_id, "T2v");
        assert_eq!(verify_tasks[1].sort_order, 3);
        assert_eq!(verify_tasks[1].parent_id, Some(story_id));
    }

    #[test]
    fn auto_generate_verify_tasks_skips_non_impl() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();

        // Create a story (not a task) — should be ignored
        let story = make_story(story_id, session_id);
        let verify_tasks = auto_generate_verify_tasks(&[story]);

        assert!(verify_tasks.is_empty());
    }

    #[test]
    fn auto_generate_verify_tasks_empty_input() {
        let verify_tasks = auto_generate_verify_tasks(&[]);
        assert!(verify_tasks.is_empty());
    }

    // --- find_paired_verify ---

    #[test]
    fn find_paired_verify_finds_correct_task() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let impl_task = make_impl_task(story_id, session_id, "T1", 0);
        let verify_tasks = auto_generate_verify_tasks(&[impl_task.clone()]);

        let found = find_paired_verify(&impl_task, &verify_tasks);
        assert!(found.is_some());
        assert_eq!(found.unwrap().short_id, "T1v");
    }

    #[test]
    fn find_paired_verify_returns_none_when_missing() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let impl_task = make_impl_task(story_id, session_id, "T1", 0);

        let found = find_paired_verify(&impl_task, &[]);
        assert!(found.is_none());
    }

    // --- find_paired_impl ---

    #[test]
    fn find_paired_impl_finds_correct_task() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let impl_task = make_impl_task(story_id, session_id, "T1", 0);
        let verify_tasks = auto_generate_verify_tasks(&[impl_task.clone()]);

        let all_tasks = vec![impl_task.clone()];
        let found = find_paired_impl(&verify_tasks[0], &all_tasks);
        assert!(found.is_some());
        assert_eq!(found.unwrap().short_id, "T1");
    }

    #[test]
    fn find_paired_impl_returns_none_when_missing() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let impl_task = make_impl_task(story_id, session_id, "T1", 0);
        let verify_tasks = auto_generate_verify_tasks(&[impl_task]);

        let found = find_paired_impl(&verify_tasks[0], &[]);
        assert!(found.is_none());
    }

    // --- get_next_pending_task ---

    #[test]
    fn get_next_pending_task_returns_lowest_sort_order() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let t1 = make_impl_task(story_id, session_id, "T1", 0);
        let t2 = make_impl_task(story_id, session_id, "T2", 2);
        let t3 = make_impl_task(story_id, session_id, "T3", 4);

        let tasks = vec![t1, t2, t3];
        let next = get_next_pending_task(story_id, &tasks);
        assert!(next.is_some());
        assert_eq!(next.unwrap().short_id, "T1");
    }

    #[test]
    fn get_next_pending_task_skips_non_pending() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut t1 = make_impl_task(story_id, session_id, "T1", 0);
        let t2 = make_impl_task(story_id, session_id, "T2", 2);

        t1.task_start().unwrap();
        t1.task_complete(None).unwrap();

        let tasks = vec![t1, t2];
        let next = get_next_pending_task(story_id, &tasks);
        assert!(next.is_some());
        assert_eq!(next.unwrap().short_id, "T2");
    }

    #[test]
    fn get_next_pending_task_returns_none_when_all_done() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut t1 = make_impl_task(story_id, session_id, "T1", 0);

        t1.task_start().unwrap();
        t1.task_complete(None).unwrap();

        let tasks = vec![t1];
        let next = get_next_pending_task(story_id, &tasks);
        assert!(next.is_none());
    }

    #[test]
    fn get_next_pending_task_filters_by_story() {
        let session_id = Uuid::new_v4();
        let story1 = Uuid::new_v4();
        let story2 = Uuid::new_v4();
        let t1 = make_impl_task(story1, session_id, "T1", 0);
        let t2 = make_impl_task(story2, session_id, "T2", 0);

        let tasks = vec![t1, t2];
        let next = get_next_pending_task(story1, &tasks);
        assert!(next.is_some());
        assert_eq!(next.unwrap().short_id, "T1");
    }

    // --- skip_task with paired verify ---

    #[test]
    fn skip_task_skips_impl_and_cancels_pending_verify() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut impl_task = make_impl_task(story_id, session_id, "T1", 0);
        let mut verify_tasks = auto_generate_verify_tasks(&[impl_task.clone()]);

        // Impl must be failed first to skip
        impl_task.task_start().unwrap();
        impl_task.task_fail().unwrap();

        skip_task(&mut impl_task, &mut verify_tasks).unwrap();

        assert_eq!(impl_task.status, WorkItemStatus::Done);
        assert_eq!(verify_tasks[0].status, WorkItemStatus::Cancelled);
    }

    #[test]
    fn skip_task_skips_impl_and_skips_failed_verify() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut impl_task = make_impl_task(story_id, session_id, "T1", 0);
        let mut verify_tasks = auto_generate_verify_tasks(&[impl_task.clone()]);

        // Fail both tasks
        impl_task.task_start().unwrap();
        impl_task.task_fail().unwrap();
        verify_tasks[0].task_start().unwrap();
        verify_tasks[0].task_fail().unwrap();

        skip_task(&mut impl_task, &mut verify_tasks).unwrap();

        assert_eq!(impl_task.status, WorkItemStatus::Done);
        assert_eq!(verify_tasks[0].status, WorkItemStatus::Done);
    }

    // --- Additional invalid transitions: cancelled -> done ---

    #[test]
    fn story_complete_from_cancelled_fails() {
        let session_id = Uuid::new_v4();
        let epic = make_epic(session_id);
        let mut story = make_story(epic.id, session_id);

        story.story_cancel().unwrap();
        let err = story.story_complete().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_complete_from_cancelled_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_cancel().unwrap();
        let err = task.task_complete(None).unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn task_skip_from_cancelled_fails() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        task.task_cancel().unwrap();
        let err = task.task_skip().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    // --- Task timestamp updates ---

    #[test]
    fn task_transitions_update_timestamp() {
        let session_id = Uuid::new_v4();
        let story_id = Uuid::new_v4();
        let mut task = make_impl_task(story_id, session_id, "T1", 0);

        let t0 = task.updated_at;
        task.task_start().unwrap();
        assert!(task.updated_at >= t0);

        let t1 = task.updated_at;
        task.task_complete(Some("hash".into())).unwrap();
        assert!(task.updated_at >= t1);
    }
}
