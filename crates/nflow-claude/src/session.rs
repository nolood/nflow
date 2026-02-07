use std::collections::HashMap;

use uuid::Uuid;

/// Tracks Claude session IDs for resumable entities (specs, decomposition sessions).
///
/// The daemon populates this from the database on startup/load.
/// After each Claude invocation, the session ID from the stream result is stored here,
/// and the daemon persists it back to the database.
///
/// This is an in-memory mapping — no IO, no database access.
#[derive(Debug, Default)]
pub struct SessionTracker {
    /// Maps entity_id (spec or decomposition session UUID) to Claude session ID.
    sessions: HashMap<Uuid, String>,
}

impl SessionTracker {
    /// Create a new empty session tracker.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Start tracking a session for an entity.
    ///
    /// If the entity already has a stored session ID (from a previous invocation or
    /// loaded from the database), it remains available via `get_resume_id()`.
    ///
    /// If this is the first invocation for the entity (no prior session ID),
    /// `get_resume_id()` will return `None`, meaning Claude starts a fresh session.
    pub fn start_session(&self, entity_id: Uuid) -> Option<String> {
        self.sessions.get(&entity_id).cloned()
    }

    /// Store the Claude session ID received from a stream result event.
    ///
    /// Called after the first (or each subsequent) Claude invocation completes
    /// and returns a session_id in the result event.
    pub fn update_session(&mut self, entity_id: Uuid, session_id: String) {
        self.sessions.insert(entity_id, session_id);
    }

    /// Get the resume ID for an entity, used to populate `RunConfig.resume_session`.
    ///
    /// Returns `Some(session_id)` if this entity has been invoked before,
    /// `None` if this is the first invocation.
    pub fn get_resume_id(&self, entity_id: Uuid) -> Option<&str> {
        self.sessions.get(&entity_id).map(|s| s.as_str())
    }

    /// Load a session ID from the database into the tracker.
    ///
    /// Called during initialization when loading entities that already have
    /// a `claude_session_id` stored (e.g., `spec.claude_session_id`,
    /// `decomposition_session.claude_session_id`).
    pub fn load_session(&mut self, entity_id: Uuid, session_id: String) {
        self.sessions.insert(entity_id, session_id);
    }

    /// Remove a session mapping (e.g., when an entity is deleted or a session is discarded).
    pub fn remove_session(&mut self, entity_id: Uuid) -> Option<String> {
        self.sessions.remove(&entity_id)
    }

    /// Check if an entity has a stored session ID.
    pub fn has_session(&self, entity_id: Uuid) -> bool {
        self.sessions.contains_key(&entity_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Constructor ---

    #[test]
    fn new_tracker_is_empty() {
        let tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        assert!(tracker.get_resume_id(entity_id).is_none());
        assert!(!tracker.has_session(entity_id));
    }

    #[test]
    fn default_tracker_is_empty() {
        let tracker = SessionTracker::default();
        assert!(tracker.get_resume_id(Uuid::new_v4()).is_none());
    }

    // --- start_session ---

    #[test]
    fn start_session_returns_none_for_new_entity() {
        let tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        assert!(tracker.start_session(entity_id).is_none());
    }

    #[test]
    fn start_session_returns_existing_session_id() {
        let mut tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        tracker.update_session(entity_id, "claude-sess-123".into());
        assert_eq!(
            tracker.start_session(entity_id),
            Some("claude-sess-123".into())
        );
    }

    // --- update_session ---

    #[test]
    fn update_session_stores_new_id() {
        let mut tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        tracker.update_session(entity_id, "sess-1".into());
        assert_eq!(tracker.get_resume_id(entity_id), Some("sess-1"));
    }

    #[test]
    fn update_session_overwrites_existing_id() {
        let mut tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        tracker.update_session(entity_id, "sess-1".into());
        tracker.update_session(entity_id, "sess-2".into());
        assert_eq!(tracker.get_resume_id(entity_id), Some("sess-2"));
    }

    // --- get_resume_id ---

    #[test]
    fn get_resume_id_returns_none_for_unknown_entity() {
        let tracker = SessionTracker::new();
        assert!(tracker.get_resume_id(Uuid::new_v4()).is_none());
    }

    #[test]
    fn get_resume_id_returns_session_id_after_update() {
        let mut tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        tracker.update_session(entity_id, "resume-me".into());
        assert_eq!(tracker.get_resume_id(entity_id), Some("resume-me"));
    }

    // --- load_session ---

    #[test]
    fn load_session_from_database() {
        let mut tracker = SessionTracker::new();
        let spec_id = Uuid::new_v4();
        tracker.load_session(spec_id, "db-loaded-session".into());
        assert_eq!(tracker.get_resume_id(spec_id), Some("db-loaded-session"));
        assert!(tracker.has_session(spec_id));
    }

    #[test]
    fn load_session_then_start_returns_loaded_id() {
        let mut tracker = SessionTracker::new();
        let spec_id = Uuid::new_v4();
        tracker.load_session(spec_id, "from-db".into());
        assert_eq!(tracker.start_session(spec_id), Some("from-db".into()));
    }

    // --- remove_session ---

    #[test]
    fn remove_session_returns_removed_id() {
        let mut tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        tracker.update_session(entity_id, "sess-x".into());
        let removed = tracker.remove_session(entity_id);
        assert_eq!(removed, Some("sess-x".into()));
        assert!(tracker.get_resume_id(entity_id).is_none());
        assert!(!tracker.has_session(entity_id));
    }

    #[test]
    fn remove_session_returns_none_for_unknown() {
        let mut tracker = SessionTracker::new();
        let removed = tracker.remove_session(Uuid::new_v4());
        assert!(removed.is_none());
    }

    // --- has_session ---

    #[test]
    fn has_session_false_for_new_entity() {
        let tracker = SessionTracker::new();
        assert!(!tracker.has_session(Uuid::new_v4()));
    }

    #[test]
    fn has_session_true_after_update() {
        let mut tracker = SessionTracker::new();
        let entity_id = Uuid::new_v4();
        tracker.update_session(entity_id, "s".into());
        assert!(tracker.has_session(entity_id));
    }

    // --- Multi-entity scenarios ---

    #[test]
    fn multiple_entities_independent() {
        let mut tracker = SessionTracker::new();
        let spec_id = Uuid::new_v4();
        let decomp_id = Uuid::new_v4();

        tracker.update_session(spec_id, "spec-session".into());
        tracker.update_session(decomp_id, "decomp-session".into());

        assert_eq!(tracker.get_resume_id(spec_id), Some("spec-session"));
        assert_eq!(tracker.get_resume_id(decomp_id), Some("decomp-session"));
    }

    #[test]
    fn spec_and_decomposition_session_workflow() {
        let mut tracker = SessionTracker::new();

        // Simulate: daemon loads spec with existing session_id from DB
        let spec_id = Uuid::new_v4();
        tracker.load_session(spec_id, "prev-spec-session".into());

        // Spec session resumed
        let resume_id = tracker.start_session(spec_id);
        assert_eq!(resume_id, Some("prev-spec-session".into()));

        // After Claude returns, update with new session_id
        tracker.update_session(spec_id, "new-spec-session".into());
        assert_eq!(tracker.get_resume_id(spec_id), Some("new-spec-session"));

        // New decomposition session — no prior session_id
        let decomp_id = Uuid::new_v4();
        let resume_id = tracker.start_session(decomp_id);
        assert!(resume_id.is_none());

        // After Claude returns first result
        tracker.update_session(decomp_id, "decomp-sess-1".into());
        assert_eq!(tracker.get_resume_id(decomp_id), Some("decomp-sess-1"));

        // Second round of decomposition feedback loop
        let resume_id = tracker.start_session(decomp_id);
        assert_eq!(resume_id, Some("decomp-sess-1".into()));

        tracker.update_session(decomp_id, "decomp-sess-2".into());
        assert_eq!(tracker.get_resume_id(decomp_id), Some("decomp-sess-2"));
    }

    #[test]
    fn remove_does_not_affect_other_entities() {
        let mut tracker = SessionTracker::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        tracker.update_session(id1, "s1".into());
        tracker.update_session(id2, "s2".into());

        tracker.remove_session(id1);
        assert!(tracker.get_resume_id(id1).is_none());
        assert_eq!(tracker.get_resume_id(id2), Some("s2"));
    }
}
