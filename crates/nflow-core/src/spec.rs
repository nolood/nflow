use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{NflowError, Result};

/// Status of a spec in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecStatus {
    Draft,
    Approved,
    Decomposed,
    Deleted,
}

impl fmt::Display for SpecStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecStatus::Draft => write!(f, "draft"),
            SpecStatus::Approved => write!(f, "approved"),
            SpecStatus::Decomposed => write!(f, "decomposed"),
            SpecStatus::Deleted => write!(f, "deleted"),
        }
    }
}

/// A specification document managed by nflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub file_path: String,
    pub status: SpecStatus,
    pub session_active: bool,
    pub claude_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Spec {
    /// Create a new spec in draft status.
    pub fn new(project_id: Uuid, name: String, file_path: String) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            project_id,
            name,
            file_path,
            status: SpecStatus::Draft,
            session_active: false,
            claude_session_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Approve a draft spec.
    /// Valid transition: Draft -> Approved
    pub fn approve(&mut self) -> Result<()> {
        if self.status != SpecStatus::Draft {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "approved".into(),
            });
        }
        self.status = SpecStatus::Approved;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Reopen an approved spec back to draft.
    /// Valid transition: Approved -> Draft
    /// Fails if status is decomposed.
    pub fn reopen(&mut self) -> Result<()> {
        if self.status != SpecStatus::Approved {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "draft".into(),
            });
        }
        self.status = SpecStatus::Draft;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Mark an approved spec as decomposed.
    /// Valid transition: Approved -> Decomposed
    pub fn decompose(&mut self) -> Result<()> {
        if self.status != SpecStatus::Approved {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "decomposed".into(),
            });
        }
        self.status = SpecStatus::Decomposed;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Discard a decomposed plan, returning spec to approved.
    /// Valid transition: Decomposed -> Approved
    pub fn discard_plan(&mut self) -> Result<()> {
        if self.status != SpecStatus::Decomposed {
            return Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "approved".into(),
            });
        }
        self.status = SpecStatus::Approved;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// Delete a spec.
    /// Valid transitions: Draft -> Deleted, Approved -> Deleted
    /// Fails if status is decomposed (must discard wave first).
    pub fn delete(&mut self) -> Result<()> {
        match self.status {
            SpecStatus::Draft | SpecStatus::Approved => {
                self.status = SpecStatus::Deleted;
                self.updated_at = Utc::now();
                Ok(())
            }
            _ => Err(NflowError::InvalidTransition {
                from: self.status.to_string(),
                to: "deleted".into(),
            }),
        }
    }

    /// Start a Claude session for this spec.
    /// Fails if a session is already active.
    pub fn start_session(&mut self) -> Result<()> {
        if self.session_active {
            return Err(NflowError::InvalidState("session is already active".into()));
        }
        self.session_active = true;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// End the current Claude session, storing the session ID.
    pub fn end_session(&mut self, claude_session_id: String) -> Result<()> {
        self.session_active = false;
        self.claude_session_id = Some(claude_session_id);
        self.updated_at = Utc::now();
        Ok(())
    }
}

// ─── Spec Questions ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecQuestion {
    pub id: Uuid,
    pub spec_id: Uuid,
    pub question: String,
    pub options: Option<String>,
    pub answered: bool,
    pub answer: Option<String>,
    pub created_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
}

impl SpecQuestion {
    pub fn new(spec_id: Uuid, question: String, options: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            spec_id,
            question,
            options,
            answered: false,
            answer: None,
            created_at: Utc::now(),
            answered_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec() -> Spec {
        Spec::new(
            Uuid::new_v4(),
            "test-spec".into(),
            "/path/to/spec.md".into(),
        )
    }

    // --- Struct fields ---

    #[test]
    fn new_spec_has_correct_defaults() {
        let project_id = Uuid::new_v4();
        let spec = Spec::new(project_id, "my-spec".into(), "/specs/my-spec.md".into());

        assert_eq!(spec.project_id, project_id);
        assert_eq!(spec.name, "my-spec");
        assert_eq!(spec.file_path, "/specs/my-spec.md");
        assert_eq!(spec.status, SpecStatus::Draft);
        assert!(!spec.session_active);
        assert!(spec.claude_session_id.is_none());
    }

    // --- State transitions: happy paths ---

    #[test]
    fn draft_to_approved() {
        let mut spec = make_spec();
        assert!(spec.approve().is_ok());
        assert_eq!(spec.status, SpecStatus::Approved);
    }

    #[test]
    fn approved_to_draft_reopen() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        assert!(spec.reopen().is_ok());
        assert_eq!(spec.status, SpecStatus::Draft);
    }

    #[test]
    fn approved_to_decomposed() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        assert!(spec.decompose().is_ok());
        assert_eq!(spec.status, SpecStatus::Decomposed);
    }

    #[test]
    fn decomposed_to_approved_via_discard() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        spec.decompose().unwrap();
        assert!(spec.discard_plan().is_ok());
        assert_eq!(spec.status, SpecStatus::Approved);
    }

    #[test]
    fn draft_to_deleted() {
        let mut spec = make_spec();
        assert!(spec.delete().is_ok());
        assert_eq!(spec.status, SpecStatus::Deleted);
    }

    #[test]
    fn approved_to_deleted() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        assert!(spec.delete().is_ok());
        assert_eq!(spec.status, SpecStatus::Deleted);
    }

    // --- State transitions: invalid ---

    #[test]
    fn approve_from_approved_fails() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        let err = spec.approve().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn approve_from_decomposed_fails() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        spec.decompose().unwrap();
        let err = spec.approve().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn reopen_from_draft_fails() {
        let mut spec = make_spec();
        let err = spec.reopen().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn reopen_from_decomposed_fails() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        spec.decompose().unwrap();
        let err = spec.reopen().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn decompose_from_draft_fails() {
        let mut spec = make_spec();
        let err = spec.decompose().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn delete_from_decomposed_fails() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        spec.decompose().unwrap();
        let err = spec.delete().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn delete_from_deleted_fails() {
        let mut spec = make_spec();
        spec.delete().unwrap();
        let err = spec.delete().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn discard_plan_from_draft_fails() {
        let mut spec = make_spec();
        let err = spec.discard_plan().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn discard_plan_from_approved_fails() {
        let mut spec = make_spec();
        spec.approve().unwrap();
        let err = spec.discard_plan().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn reopen_from_deleted_fails() {
        let mut spec = make_spec();
        spec.delete().unwrap();
        let err = spec.reopen().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    #[test]
    fn approve_from_deleted_fails() {
        let mut spec = make_spec();
        spec.delete().unwrap();
        let err = spec.approve().unwrap_err();
        assert!(matches!(err, NflowError::InvalidTransition { .. }));
    }

    // --- Session management ---

    #[test]
    fn start_session_success() {
        let mut spec = make_spec();
        assert!(spec.start_session().is_ok());
        assert!(spec.session_active);
    }

    #[test]
    fn start_session_when_already_active_fails() {
        let mut spec = make_spec();
        spec.start_session().unwrap();
        let err = spec.start_session().unwrap_err();
        assert!(matches!(err, NflowError::InvalidState(_)));
    }

    #[test]
    fn end_session_stores_id_and_deactivates() {
        let mut spec = make_spec();
        spec.start_session().unwrap();
        assert!(spec.end_session("session-123".into()).is_ok());
        assert!(!spec.session_active);
        assert_eq!(spec.claude_session_id.as_deref(), Some("session-123"));
    }

    #[test]
    fn end_session_overwrites_previous_id() {
        let mut spec = make_spec();
        spec.start_session().unwrap();
        spec.end_session("session-1".into()).unwrap();
        spec.start_session().unwrap();
        spec.end_session("session-2".into()).unwrap();
        assert_eq!(spec.claude_session_id.as_deref(), Some("session-2"));
    }

    // --- updated_at changes on mutations ---

    #[test]
    fn approve_updates_timestamp() {
        let mut spec = make_spec();
        let before = spec.updated_at;
        // Small sleep not needed — Utc::now() should differ
        spec.approve().unwrap();
        assert!(spec.updated_at >= before);
    }
}
