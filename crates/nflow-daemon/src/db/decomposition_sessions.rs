use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use nflow_core::decomposition::{DecompositionSession, DecompositionSpec, DecompositionStatus};

use super::Result;

fn status_to_str(s: DecompositionStatus) -> &'static str {
    match s {
        DecompositionStatus::InProgress => "in_progress",
        DecompositionStatus::Approved => "approved",
        DecompositionStatus::Discarded => "discarded",
    }
}

fn status_from_str(s: &str) -> DecompositionStatus {
    match s {
        "approved" => DecompositionStatus::Approved,
        "discarded" => DecompositionStatus::Discarded,
        _ => DecompositionStatus::InProgress,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::nil())
}

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<DecompositionSession> {
    let id_str: String = row.get("id")?;
    let project_id_str: String = row.get("project_id")?;
    let wave_number: u32 = row.get("wave_number")?;
    let status_str: String = row.get("status")?;
    let claude_session_id: Option<String> = row.get("claude_session_id")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;

    Ok(DecompositionSession {
        id: parse_uuid(&id_str),
        project_id: parse_uuid(&project_id_str),
        wave_number,
        status: status_from_str(&status_str),
        claude_session_id,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    })
}

pub fn insert_decomposition_session(
    conn: &Connection,
    session: &DecompositionSession,
) -> Result<()> {
    conn.execute(
        "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, claude_session_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session.id.to_string(),
            session.project_id.to_string(),
            session.wave_number,
            status_to_str(session.status),
            session.claude_session_id,
            session.created_at.to_rfc3339(),
            session.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn insert_decomposition_spec(conn: &Connection, spec: &DecompositionSpec) -> Result<()> {
    conn.execute(
        "INSERT INTO decomposition_specs (session_id, spec_id) VALUES (?1, ?2)",
        params![spec.session_id.to_string(), spec.spec_id.to_string()],
    )?;
    Ok(())
}

pub fn get_decomposition_session(
    conn: &Connection,
    id: &Uuid,
) -> Result<Option<DecompositionSession>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, wave_number, status, claude_session_id, created_at, updated_at
         FROM decomposition_sessions WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_string()], row_to_session)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn find_draft_session(
    conn: &Connection,
    project_id: &Uuid,
) -> Result<Option<DecompositionSession>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, wave_number, status, claude_session_id, created_at, updated_at
         FROM decomposition_sessions WHERE project_id = ?1 AND status = 'in_progress'
         ORDER BY created_at DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![project_id.to_string()], row_to_session)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list_sessions_by_project(
    conn: &Connection,
    project_id: &Uuid,
) -> Result<Vec<DecompositionSession>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, wave_number, status, claude_session_id, created_at, updated_at
         FROM decomposition_sessions WHERE project_id = ?1 ORDER BY wave_number",
    )?;
    let rows = stmt.query_map(params![project_id.to_string()], row_to_session)?;
    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(row?);
    }
    Ok(sessions)
}

pub fn update_session_status(
    conn: &Connection,
    id: &Uuid,
    status: DecompositionStatus,
) -> Result<()> {
    conn.execute(
        "UPDATE decomposition_sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![
            status_to_str(status),
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(())
}

pub fn next_wave_number(conn: &Connection, project_id: &Uuid) -> Result<u32> {
    let max: Option<u32> = conn.query_row(
        "SELECT MAX(wave_number) FROM decomposition_sessions WHERE project_id = ?1",
        params![project_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(max.unwrap_or(0) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{projects::insert_project, specs::insert_spec, test_conn};
    use nflow_core::project::{GitProvider, Project};
    use nflow_core::spec::Spec;

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

    fn make_session(project_id: Uuid, wave_number: u32) -> DecompositionSession {
        let now = Utc::now();
        DecompositionSession {
            id: Uuid::new_v4(),
            project_id,
            wave_number,
            status: DecompositionStatus::InProgress,
            claude_session_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    // --- insert_decomposition_session and get_decomposition_session ---

    #[test]
    fn test_insert_and_get_session() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        let retrieved = get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.id, session.id);
        assert_eq!(retrieved.project_id, pid);
        assert_eq!(retrieved.wave_number, 1);
        assert_eq!(retrieved.status, DecompositionStatus::InProgress);
        assert!(retrieved.claude_session_id.is_none());
    }

    #[test]
    fn test_insert_session_with_claude_session_id() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let mut session = make_session(pid, 1);
        session.claude_session_id = Some("claude-xyz".to_string());
        insert_decomposition_session(&conn, &session).unwrap();

        let retrieved = get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.claude_session_id.as_deref(), Some("claude-xyz"));
    }

    #[test]
    fn test_get_session_not_found() {
        let conn = test_conn();
        let result = get_decomposition_session(&conn, &Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    // --- find_draft_session ---

    #[test]
    fn test_find_draft_session() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        let draft = find_draft_session(&conn, &pid).unwrap().unwrap();
        assert_eq!(draft.id, session.id);
        assert_eq!(draft.status, DecompositionStatus::InProgress);
    }

    #[test]
    fn test_find_draft_session_none_when_all_approved() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let mut session = make_session(pid, 1);
        session.status = DecompositionStatus::Approved;
        insert_decomposition_session(&conn, &session).unwrap();

        let result = find_draft_session(&conn, &pid).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_draft_session_scoped_to_project() {
        let conn = test_conn();
        let pid1 = make_project(&conn);
        let pid2 = make_project(&conn);

        let session1 = make_session(pid1, 1);
        let session2 = make_session(pid2, 1);
        insert_decomposition_session(&conn, &session1).unwrap();
        insert_decomposition_session(&conn, &session2).unwrap();

        let draft = find_draft_session(&conn, &pid1).unwrap().unwrap();
        assert_eq!(draft.id, session1.id);
    }

    // --- list_sessions_by_project ---

    #[test]
    fn test_list_sessions_by_project() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let s1 = make_session(pid, 1);
        let s2 = make_session(pid, 2);
        let s3 = make_session(pid, 3);
        insert_decomposition_session(&conn, &s1).unwrap();
        insert_decomposition_session(&conn, &s3).unwrap();
        insert_decomposition_session(&conn, &s2).unwrap();

        let sessions = list_sessions_by_project(&conn, &pid).unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].wave_number, 1);
        assert_eq!(sessions[1].wave_number, 2);
        assert_eq!(sessions[2].wave_number, 3);
    }

    #[test]
    fn test_list_sessions_by_project_empty() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sessions = list_sessions_by_project(&conn, &pid).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_scoped_to_project() {
        let conn = test_conn();
        let pid1 = make_project(&conn);
        let pid2 = make_project(&conn);

        insert_decomposition_session(&conn, &make_session(pid1, 1)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid1, 2)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid2, 1)).unwrap();

        let sessions1 = list_sessions_by_project(&conn, &pid1).unwrap();
        assert_eq!(sessions1.len(), 2);

        let sessions2 = list_sessions_by_project(&conn, &pid2).unwrap();
        assert_eq!(sessions2.len(), 1);
    }

    // --- update_session_status ---

    #[test]
    fn test_update_session_status_to_approved() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        update_session_status(&conn, &session.id, DecompositionStatus::Approved).unwrap();

        let retrieved = get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.status, DecompositionStatus::Approved);
    }

    #[test]
    fn test_update_session_status_to_discarded() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        update_session_status(&conn, &session.id, DecompositionStatus::Discarded).unwrap();

        let retrieved = get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.status, DecompositionStatus::Discarded);
    }

    // --- next_wave_number ---

    #[test]
    fn test_next_wave_number_no_sessions() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let next = next_wave_number(&conn, &pid).unwrap();
        assert_eq!(next, 1);
    }

    #[test]
    fn test_next_wave_number_with_existing() {
        let conn = test_conn();
        let pid = make_project(&conn);

        insert_decomposition_session(&conn, &make_session(pid, 1)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid, 2)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid, 3)).unwrap();

        let next = next_wave_number(&conn, &pid).unwrap();
        assert_eq!(next, 4);
    }

    #[test]
    fn test_next_wave_number_scoped_to_project() {
        let conn = test_conn();
        let pid1 = make_project(&conn);
        let pid2 = make_project(&conn);

        insert_decomposition_session(&conn, &make_session(pid1, 1)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid1, 5)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid2, 1)).unwrap();

        assert_eq!(next_wave_number(&conn, &pid1).unwrap(), 6);
        assert_eq!(next_wave_number(&conn, &pid2).unwrap(), 2);
    }

    // --- insert_decomposition_spec ---

    #[test]
    fn test_insert_decomposition_spec() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        let spec = Spec::new(pid, "my-spec".into(), "/specs/my-spec.md".into());
        insert_spec(&conn, &spec).unwrap();

        let ds = DecompositionSpec::new(session.id, spec.id);
        insert_decomposition_spec(&conn, &ds).unwrap();

        // Verify via raw query
        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM decomposition_specs WHERE session_id = ?1 AND spec_id = ?2",
                params![session.id.to_string(), spec.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_insert_multiple_specs_per_session() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        let spec1 = Spec::new(pid, "spec-1".into(), "/specs/1.md".into());
        let spec2 = Spec::new(pid, "spec-2".into(), "/specs/2.md".into());
        insert_spec(&conn, &spec1).unwrap();
        insert_spec(&conn, &spec2).unwrap();

        insert_decomposition_spec(&conn, &DecompositionSpec::new(session.id, spec1.id)).unwrap();
        insert_decomposition_spec(&conn, &DecompositionSpec::new(session.id, spec2.id)).unwrap();

        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM decomposition_specs WHERE session_id = ?1",
                params![session.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    // --- cascade on session delete ---

    #[test]
    fn test_cascade_delete_session_removes_specs_mapping() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let session = make_session(pid, 1);
        insert_decomposition_session(&conn, &session).unwrap();

        let spec = Spec::new(pid, "my-spec".into(), "/specs/my-spec.md".into());
        insert_spec(&conn, &spec).unwrap();
        insert_decomposition_spec(&conn, &DecompositionSpec::new(session.id, spec.id)).unwrap();

        // Delete session — decomposition_specs mapping should cascade
        conn.execute(
            "DELETE FROM decomposition_sessions WHERE id = ?1",
            params![session.id.to_string()],
        )
        .unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM decomposition_specs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_cascade_delete_project_removes_sessions() {
        let conn = test_conn();
        let pid = make_project(&conn);

        insert_decomposition_session(&conn, &make_session(pid, 1)).unwrap();
        insert_decomposition_session(&conn, &make_session(pid, 2)).unwrap();

        conn.execute(
            "DELETE FROM projects WHERE id = ?1",
            params![pid.to_string()],
        )
        .unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM decomposition_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    // --- parameterized queries ---

    #[test]
    fn test_parameterized_queries() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let mut session = make_session(pid, 1);
        session.claude_session_id = Some("'; DROP TABLE decomposition_sessions; --".to_string());
        insert_decomposition_session(&conn, &session).unwrap();

        let retrieved = get_decomposition_session(&conn, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            retrieved.claude_session_id.as_deref(),
            Some("'; DROP TABLE decomposition_sessions; --")
        );

        // Table should still exist
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM decomposition_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}
