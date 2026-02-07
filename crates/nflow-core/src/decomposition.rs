use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{NflowError, Result};
use crate::spec::{Spec, SpecStatus};
use crate::work_item::WorkItem;

/// Status of a decomposition session (wave).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecompositionStatus {
    InProgress,
    Approved,
    Discarded,
}

impl fmt::Display for DecompositionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecompositionStatus::InProgress => write!(f, "in_progress"),
            DecompositionStatus::Approved => write!(f, "approved"),
            DecompositionStatus::Discarded => write!(f, "discarded"),
        }
    }
}

/// A decomposition session represents a single wave of spec decomposition.
///
/// Each session decomposes one or more approved specs into an epic→story→task DAG.
/// Sessions are per-project and have an auto-incremented wave_number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecompositionSession {
    pub id: Uuid,
    pub project_id: Uuid,
    pub wave_number: u32,
    pub status: DecompositionStatus,
    pub claude_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DecompositionSession {
    /// Create a new decomposition session.
    ///
    /// `has_in_progress` should return true if the project already has an in-progress session.
    /// `max_wave_number` is the highest existing wave_number for this project (0 if none).
    pub fn new(
        project_id: Uuid,
        has_in_progress: impl Fn(Uuid) -> bool,
        max_wave_number: u32,
    ) -> Result<Self> {
        if has_in_progress(project_id) {
            return Err(NflowError::AlreadyExists(
                "project already has an in-progress decomposition session".into(),
            ));
        }

        let now = Utc::now();
        Ok(Self {
            id: Uuid::new_v4(),
            project_id,
            wave_number: max_wave_number + 1,
            status: DecompositionStatus::InProgress,
            claude_session_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    /// Approve the session, making its stories schedulable.
    ///
    /// Valid transition: InProgress -> Approved
    pub fn approve(&mut self) -> Result<()> {
        if self.status != DecompositionStatus::InProgress {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "approved".into(),
            });
        }
        self.status = DecompositionStatus::Approved;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Discard the session.
    ///
    /// Valid transition: InProgress -> Discarded, Approved -> Discarded
    /// The caller is responsible for deleting associated work items and
    /// freeing specs back to approved status (use `discard_session()`).
    pub fn discard(&mut self) -> Result<()> {
        if self.status == DecompositionStatus::Discarded {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "discarded".into(),
            });
        }
        self.status = DecompositionStatus::Discarded;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Set the Claude session ID for this decomposition session.
    pub fn set_claude_session_id(&mut self, session_id: String) {
        self.claude_session_id = Some(session_id);
        self.updated_at = Utc::now();
    }
}

/// Many-to-many mapping between decomposition sessions and specs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecompositionSpec {
    pub session_id: Uuid,
    pub spec_id: Uuid,
}

impl DecompositionSpec {
    pub fn new(session_id: Uuid, spec_id: Uuid) -> Self {
        Self {
            session_id,
            spec_id,
        }
    }
}

/// Compute the next wave number for a project.
///
/// Returns max_existing + 1, or 1 if no sessions exist.
pub fn next_wave_number(existing_wave_numbers: &[u32]) -> u32 {
    existing_wave_numbers.iter().max().copied().unwrap_or(0) + 1
}

/// Discard a session, cleaning up associated work items and freeing specs.
///
/// This is the full discard operation:
/// 1. Transitions session status to Discarded
/// 2. Returns the IDs of work items to delete (caller removes from storage)
/// 3. Frees specs back to Approved status (reverts from Decomposed)
///
/// `work_items` should be all work items belonging to this session.
/// `specs` should be the specs associated with this session.
pub fn discard_session(
    session: &mut DecompositionSession,
    work_items: &[WorkItem],
    specs: &mut [Spec],
) -> Result<Vec<Uuid>> {
    session.discard()?;

    // Collect work item IDs to delete
    let work_item_ids: Vec<Uuid> = work_items
        .iter()
        .filter(|w| w.decomposition_session_id == session.id)
        .map(|w| w.id)
        .collect();

    // Free specs back to approved status
    for spec in specs.iter_mut() {
        if spec.status == SpecStatus::Decomposed {
            spec.discard_plan()?;
        }
    }

    Ok(work_item_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_in_progress(_project_id: Uuid) -> bool {
        false
    }

    fn has_in_progress(_project_id: Uuid) -> bool {
        true
    }

    // --- Constructor ---

    #[test]
    fn new_session_defaults() {
        let project_id = Uuid::new_v4();
        let session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();

        assert_eq!(session.project_id, project_id);
        assert_eq!(session.wave_number, 1);
        assert_eq!(session.status, DecompositionStatus::InProgress);
        assert!(session.claude_session_id.is_none());
    }

    #[test]
    fn new_session_auto_increments_wave_number() {
        let project_id = Uuid::new_v4();
        let session = DecompositionSession::new(project_id, no_in_progress, 3).unwrap();
        assert_eq!(session.wave_number, 4);
    }

    #[test]
    fn new_session_fails_when_project_has_in_progress() {
        let project_id = Uuid::new_v4();
        let err = DecompositionSession::new(project_id, has_in_progress, 0).unwrap_err();
        assert!(matches!(err, NflowError::AlreadyExists(_)));
    }

    // --- State machine: approve ---

    #[test]
    fn approve_from_in_progress() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        assert!(session.approve().is_ok());
        assert_eq!(session.status, DecompositionStatus::Approved);
    }

    #[test]
    fn approve_from_approved_fails() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        session.approve().unwrap();
        let err = session.approve().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn approve_from_discarded_fails() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        session.discard().unwrap();
        let err = session.approve().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    // --- State machine: discard ---

    #[test]
    fn discard_from_in_progress() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        assert!(session.discard().is_ok());
        assert_eq!(session.status, DecompositionStatus::Discarded);
    }

    #[test]
    fn discard_from_approved_succeeds() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        session.approve().unwrap();
        assert!(session.discard().is_ok());
        assert_eq!(session.status, DecompositionStatus::Discarded);
    }

    #[test]
    fn discard_from_discarded_fails() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        session.discard().unwrap();
        let err = session.discard().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    // --- Claude session ID ---

    #[test]
    fn set_claude_session_id() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        session.set_claude_session_id("claude-abc".into());
        assert_eq!(session.claude_session_id.as_deref(), Some("claude-abc"));
    }

    // --- DecompositionSpec ---

    #[test]
    fn decomposition_spec_mapping() {
        let session_id = Uuid::new_v4();
        let spec_id = Uuid::new_v4();
        let mapping = DecompositionSpec::new(session_id, spec_id);
        assert_eq!(mapping.session_id, session_id);
        assert_eq!(mapping.spec_id, spec_id);
    }

    // --- next_wave_number ---

    #[test]
    fn next_wave_number_empty() {
        assert_eq!(next_wave_number(&[]), 1);
    }

    #[test]
    fn next_wave_number_with_existing() {
        assert_eq!(next_wave_number(&[1, 2, 3]), 4);
    }

    #[test]
    fn next_wave_number_non_sequential() {
        assert_eq!(next_wave_number(&[1, 5, 3]), 6);
    }

    // --- discard_session ---

    #[test]
    fn discard_session_returns_work_item_ids() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();

        let work_items = vec![
            WorkItem::new_epic(session.id, "Epic 1".into(), "Desc".into(), "E1".into(), 0),
            WorkItem::new_epic(session.id, "Epic 2".into(), "Desc".into(), "E2".into(), 1),
        ];

        let expected_ids: Vec<Uuid> = work_items.iter().map(|w| w.id).collect();
        let mut specs: Vec<Spec> = vec![];

        let deleted_ids = discard_session(&mut session, &work_items, &mut specs).unwrap();
        assert_eq!(deleted_ids, expected_ids);
        assert_eq!(session.status, DecompositionStatus::Discarded);
    }

    #[test]
    fn discard_session_frees_specs_back_to_approved() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();

        let mut spec1 = Spec::new(project_id, "spec-1".into(), "/specs/1.md".into());
        spec1.approve().unwrap();
        spec1.decompose().unwrap();

        let mut spec2 = Spec::new(project_id, "spec-2".into(), "/specs/2.md".into());
        spec2.approve().unwrap();
        spec2.decompose().unwrap();

        let mut specs = vec![spec1, spec2];
        let _ = discard_session(&mut session, &[], &mut specs).unwrap();

        for spec in &specs {
            assert_eq!(spec.status, SpecStatus::Approved);
        }
    }

    #[test]
    fn discard_session_only_affects_session_work_items() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        let other_session_id = Uuid::new_v4();

        let work_items = vec![
            WorkItem::new_epic(session.id, "Mine".into(), "Desc".into(), "E1".into(), 0),
            WorkItem::new_epic(
                other_session_id,
                "Other".into(),
                "Desc".into(),
                "E2".into(),
                1,
            ),
        ];

        let mut specs: Vec<Spec> = vec![];
        let deleted_ids = discard_session(&mut session, &work_items, &mut specs).unwrap();

        // Only the work item belonging to this session should be returned
        assert_eq!(deleted_ids.len(), 1);
        assert_eq!(deleted_ids[0], work_items[0].id);
    }

    #[test]
    fn discard_session_succeeds_from_approved() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        session.approve().unwrap();

        let deleted_ids = discard_session(&mut session, &[], &mut []).unwrap();
        assert!(deleted_ids.is_empty());
        assert_eq!(session.status, DecompositionStatus::Discarded);
    }

    // --- Display impl ---

    #[test]
    fn decomposition_status_display() {
        assert_eq!(DecompositionStatus::InProgress.to_string(), "in_progress");
        assert_eq!(DecompositionStatus::Approved.to_string(), "approved");
        assert_eq!(DecompositionStatus::Discarded.to_string(), "discarded");
    }

    // --- updated_at changes ---

    #[test]
    fn approve_updates_timestamp() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        let before = session.updated_at;
        session.approve().unwrap();
        assert!(session.updated_at >= before);
    }

    #[test]
    fn discard_updates_timestamp() {
        let project_id = Uuid::new_v4();
        let mut session = DecompositionSession::new(project_id, no_in_progress, 0).unwrap();
        let before = session.updated_at;
        session.discard().unwrap();
        assert!(session.updated_at >= before);
    }
}
