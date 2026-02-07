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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}
