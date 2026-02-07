use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use nflow_core::spec::{Spec, SpecStatus};

use super::Result;

fn spec_status_to_str(status: SpecStatus) -> &'static str {
    match status {
        SpecStatus::Draft => "draft",
        SpecStatus::Approved => "approved",
        SpecStatus::Decomposed => "decomposed",
        SpecStatus::Deleted => "deleted",
    }
}

fn spec_status_from_str(s: &str) -> SpecStatus {
    match s {
        "approved" => SpecStatus::Approved,
        "decomposed" => SpecStatus::Decomposed,
        "deleted" => SpecStatus::Deleted,
        _ => SpecStatus::Draft,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn row_to_spec(row: &Row<'_>) -> rusqlite::Result<Spec> {
    let id_str: String = row.get("id")?;
    let project_id_str: String = row.get("project_id")?;
    let name: String = row.get("name")?;
    let file_path: String = row.get("file_path")?;
    let status_str: String = row.get("status")?;
    let session_active: bool = row.get("session_active")?;
    let claude_session_id: Option<String> = row.get("claude_session_id")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;

    Ok(Spec {
        id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()),
        project_id: Uuid::parse_str(&project_id_str).unwrap_or_else(|_| Uuid::nil()),
        name,
        file_path,
        status: spec_status_from_str(&status_str),
        session_active,
        claude_session_id,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    })
}

pub fn insert_spec(conn: &Connection, spec: &Spec) -> Result<()> {
    conn.execute(
        "INSERT INTO specs (id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            spec.id.to_string(),
            spec.project_id.to_string(),
            spec.name,
            spec.file_path,
            spec_status_to_str(spec.status),
            spec.session_active,
            spec.claude_session_id,
            spec.created_at.to_rfc3339(),
            spec.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_spec_by_name(conn: &Connection, project_id: &Uuid, name: &str) -> Result<Option<Spec>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at
         FROM specs WHERE project_id = ?1 AND name = ?2",
    )?;
    let mut rows = stmt.query_map(params![project_id.to_string(), name], row_to_spec)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list_specs_by_project(conn: &Connection, project_id: &Uuid) -> Result<Vec<Spec>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at
         FROM specs WHERE project_id = ?1 ORDER BY name",
    )?;
    let rows = stmt.query_map(params![project_id.to_string()], row_to_spec)?;
    let mut specs = Vec::new();
    for row in rows {
        specs.push(row?);
    }
    Ok(specs)
}

pub fn list_specs_by_status(
    conn: &Connection,
    project_id: &Uuid,
    status: SpecStatus,
) -> Result<Vec<Spec>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at
         FROM specs WHERE project_id = ?1 AND status = ?2 ORDER BY name",
    )?;
    let rows = stmt.query_map(
        params![project_id.to_string(), spec_status_to_str(status)],
        row_to_spec,
    )?;
    let mut specs = Vec::new();
    for row in rows {
        specs.push(row?);
    }
    Ok(specs)
}

pub fn update_spec_status(conn: &Connection, id: &Uuid, status: SpecStatus) -> Result<()> {
    conn.execute(
        "UPDATE specs SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![
            spec_status_to_str(status),
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(())
}

pub fn update_spec_session(
    conn: &Connection,
    id: &Uuid,
    session_active: bool,
    claude_session_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE specs SET session_active = ?1, claude_session_id = ?2, updated_at = ?3 WHERE id = ?4",
        params![
            session_active,
            claude_session_id,
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(())
}

pub fn find_latest_draft_spec(conn: &Connection, project_id: &Uuid) -> Result<Option<Spec>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at
         FROM specs WHERE project_id = ?1 AND status = 'draft' ORDER BY updated_at DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![project_id.to_string()], row_to_spec)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn find_unassigned_approved_specs(conn: &Connection, project_id: &Uuid) -> Result<Vec<Spec>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.project_id, s.name, s.file_path, s.status, s.session_active, s.claude_session_id, s.created_at, s.updated_at
         FROM specs s
         WHERE s.project_id = ?1 AND s.status = 'approved'
           AND s.id NOT IN (SELECT spec_id FROM decomposition_specs)
         ORDER BY s.name",
    )?;
    let rows = stmt.query_map(params![project_id.to_string()], row_to_spec)?;
    let mut specs = Vec::new();
    for row in rows {
        specs.push(row?);
    }
    Ok(specs)
}

pub fn reset_active_sessions(conn: &Connection, project_id: &Uuid) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE specs SET session_active = 0, updated_at = ?1 WHERE project_id = ?2 AND session_active = 1",
        params![Utc::now().to_rfc3339(), project_id.to_string()],
    )?;
    Ok(changed as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{projects::insert_project, test_conn};
    use nflow_core::project::{GitProvider, Project};

    fn make_project(conn: &Connection) -> Uuid {
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: "test-project".to_string(),
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

    fn make_spec(project_id: Uuid, name: &str) -> Spec {
        Spec::new(project_id, name.to_string(), format!("/specs/{}.md", name))
    }

    #[test]
    fn test_insert_and_get_by_name() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec = make_spec(pid, "my-spec");
        insert_spec(&conn, &spec).unwrap();

        let retrieved = get_spec_by_name(&conn, &pid, "my-spec").unwrap().unwrap();
        assert_eq!(retrieved.id, spec.id);
        assert_eq!(retrieved.name, "my-spec");
        assert_eq!(retrieved.project_id, pid);
        assert_eq!(retrieved.file_path, "/specs/my-spec.md");
        assert_eq!(retrieved.status, SpecStatus::Draft);
        assert!(!retrieved.session_active);
        assert!(retrieved.claude_session_id.is_none());
    }

    #[test]
    fn test_get_by_name_not_found() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let result = get_spec_by_name(&conn, &pid, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_by_name_wrong_project() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec = make_spec(pid, "my-spec");
        insert_spec(&conn, &spec).unwrap();

        let other_pid = Uuid::new_v4();
        let result = get_spec_by_name(&conn, &other_pid, "my-spec").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_list_specs_by_project() {
        let conn = test_conn();
        let pid = make_project(&conn);
        insert_spec(&conn, &make_spec(pid, "beta")).unwrap();
        insert_spec(&conn, &make_spec(pid, "alpha")).unwrap();
        insert_spec(&conn, &make_spec(pid, "gamma")).unwrap();

        let specs = list_specs_by_project(&conn, &pid).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].name, "alpha");
        assert_eq!(specs[1].name, "beta");
        assert_eq!(specs[2].name, "gamma");
    }

    #[test]
    fn test_list_specs_by_project_empty() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let specs = list_specs_by_project(&conn, &pid).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn test_list_specs_by_status() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let draft = make_spec(pid, "draft-spec");
        insert_spec(&conn, &draft).unwrap();

        let mut approved = make_spec(pid, "approved-spec");
        approved.status = SpecStatus::Approved;
        insert_spec(&conn, &approved).unwrap();

        let mut decomposed = make_spec(pid, "decomposed-spec");
        decomposed.status = SpecStatus::Decomposed;
        insert_spec(&conn, &decomposed).unwrap();

        let drafts = list_specs_by_status(&conn, &pid, SpecStatus::Draft).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].name, "draft-spec");

        let approveds = list_specs_by_status(&conn, &pid, SpecStatus::Approved).unwrap();
        assert_eq!(approveds.len(), 1);
        assert_eq!(approveds[0].name, "approved-spec");

        let decomposeds = list_specs_by_status(&conn, &pid, SpecStatus::Decomposed).unwrap();
        assert_eq!(decomposeds.len(), 1);
        assert_eq!(decomposeds[0].name, "decomposed-spec");
    }

    #[test]
    fn test_update_spec_status() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec = make_spec(pid, "my-spec");
        insert_spec(&conn, &spec).unwrap();

        update_spec_status(&conn, &spec.id, SpecStatus::Approved).unwrap();

        let retrieved = get_spec_by_name(&conn, &pid, "my-spec").unwrap().unwrap();
        assert_eq!(retrieved.status, SpecStatus::Approved);
    }

    #[test]
    fn test_update_spec_session() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec = make_spec(pid, "my-spec");
        insert_spec(&conn, &spec).unwrap();

        // Activate session
        update_spec_session(&conn, &spec.id, true, Some("claude-123")).unwrap();
        let retrieved = get_spec_by_name(&conn, &pid, "my-spec").unwrap().unwrap();
        assert!(retrieved.session_active);
        assert_eq!(retrieved.claude_session_id.as_deref(), Some("claude-123"));

        // Deactivate session
        update_spec_session(&conn, &spec.id, false, Some("claude-123")).unwrap();
        let retrieved = get_spec_by_name(&conn, &pid, "my-spec").unwrap().unwrap();
        assert!(!retrieved.session_active);
        assert_eq!(retrieved.claude_session_id.as_deref(), Some("claude-123"));
    }

    #[test]
    fn test_update_spec_session_clear_claude_id() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let spec = make_spec(pid, "my-spec");
        insert_spec(&conn, &spec).unwrap();

        update_spec_session(&conn, &spec.id, true, Some("claude-123")).unwrap();
        update_spec_session(&conn, &spec.id, false, None).unwrap();

        let retrieved = get_spec_by_name(&conn, &pid, "my-spec").unwrap().unwrap();
        assert!(!retrieved.session_active);
        assert!(retrieved.claude_session_id.is_none());
    }

    #[test]
    fn test_find_latest_draft_spec() {
        let conn = test_conn();
        let pid = make_project(&conn);

        // Insert specs with different updated_at times
        let mut spec1 = make_spec(pid, "old-draft");
        spec1.updated_at = "2024-01-01T00:00:00Z".parse().unwrap();
        insert_spec(&conn, &spec1).unwrap();

        let mut spec2 = make_spec(pid, "new-draft");
        spec2.updated_at = "2024-06-01T00:00:00Z".parse().unwrap();
        insert_spec(&conn, &spec2).unwrap();

        // Also insert an approved spec — should not be returned
        let mut approved = make_spec(pid, "approved-spec");
        approved.status = SpecStatus::Approved;
        approved.updated_at = "2025-01-01T00:00:00Z".parse().unwrap();
        insert_spec(&conn, &approved).unwrap();

        let latest = find_latest_draft_spec(&conn, &pid).unwrap().unwrap();
        assert_eq!(latest.name, "new-draft");
    }

    #[test]
    fn test_find_latest_draft_spec_none() {
        let conn = test_conn();
        let pid = make_project(&conn);

        // Only an approved spec
        let mut approved = make_spec(pid, "approved");
        approved.status = SpecStatus::Approved;
        insert_spec(&conn, &approved).unwrap();

        let result = find_latest_draft_spec(&conn, &pid).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_unassigned_approved_specs() {
        let conn = test_conn();
        let pid = make_project(&conn);

        // Create approved specs
        let mut spec1 = make_spec(pid, "unassigned-1");
        spec1.status = SpecStatus::Approved;
        insert_spec(&conn, &spec1).unwrap();

        let mut spec2 = make_spec(pid, "unassigned-2");
        spec2.status = SpecStatus::Approved;
        insert_spec(&conn, &spec2).unwrap();

        // Create a draft spec — should not appear
        let draft = make_spec(pid, "draft-spec");
        insert_spec(&conn, &draft).unwrap();

        // All approved specs should be unassigned
        let unassigned = find_unassigned_approved_specs(&conn, &pid).unwrap();
        assert_eq!(unassigned.len(), 2);
        assert_eq!(unassigned[0].name, "unassigned-1");
        assert_eq!(unassigned[1].name, "unassigned-2");
    }

    #[test]
    fn test_find_unassigned_approved_specs_excludes_assigned() {
        let conn = test_conn();
        let pid = make_project(&conn);

        // Create approved specs
        let mut spec1 = make_spec(pid, "assigned-spec");
        spec1.status = SpecStatus::Approved;
        insert_spec(&conn, &spec1).unwrap();

        let mut spec2 = make_spec(pid, "free-spec");
        spec2.status = SpecStatus::Approved;
        insert_spec(&conn, &spec2).unwrap();

        // Create a decomposition session and assign spec1 to it
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'in_progress', ?3, ?3)",
            params![
                Uuid::new_v4().to_string(),
                pid.to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();
        let session_id: String = conn
            .query_row(
                "SELECT id FROM decomposition_sessions WHERE project_id = ?1",
                params![pid.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO decomposition_specs (session_id, spec_id) VALUES (?1, ?2)",
            params![session_id, spec1.id.to_string()],
        )
        .unwrap();

        // Only the free spec should be returned
        let unassigned = find_unassigned_approved_specs(&conn, &pid).unwrap();
        assert_eq!(unassigned.len(), 1);
        assert_eq!(unassigned[0].name, "free-spec");
    }

    #[test]
    fn test_reset_active_sessions() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let mut spec1 = make_spec(pid, "active-1");
        spec1.session_active = true;
        insert_spec(&conn, &spec1).unwrap();

        let mut spec2 = make_spec(pid, "active-2");
        spec2.session_active = true;
        insert_spec(&conn, &spec2).unwrap();

        let spec3 = make_spec(pid, "inactive");
        insert_spec(&conn, &spec3).unwrap();

        let count = reset_active_sessions(&conn, &pid).unwrap();
        assert_eq!(count, 2);

        // All should now be inactive
        let specs = list_specs_by_project(&conn, &pid).unwrap();
        for spec in &specs {
            assert!(!spec.session_active);
        }
    }

    #[test]
    fn test_reset_active_sessions_none_active() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let spec = make_spec(pid, "inactive");
        insert_spec(&conn, &spec).unwrap();

        let count = reset_active_sessions(&conn, &pid).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_reset_active_sessions_scoped_to_project() {
        let conn = test_conn();
        let pid1 = make_project(&conn);

        // Create a second project
        let now = Utc::now();
        let project2 = Project {
            id: Uuid::new_v4(),
            name: "other-project".to_string(),
            path: "/home/user/other".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: true,
            created_at: now,
            updated_at: now,
        };
        insert_project(&conn, &project2).unwrap();

        let mut spec1 = make_spec(pid1, "p1-active");
        spec1.session_active = true;
        insert_spec(&conn, &spec1).unwrap();

        let mut spec2 = make_spec(project2.id, "p2-active");
        spec2.session_active = true;
        insert_spec(&conn, &spec2).unwrap();

        // Reset only project 1
        let count = reset_active_sessions(&conn, &pid1).unwrap();
        assert_eq!(count, 1);

        // Project 2's spec should still be active
        let p2_specs = list_specs_by_project(&conn, &project2.id).unwrap();
        assert!(p2_specs[0].session_active);
    }

    #[test]
    fn test_parameterized_queries() {
        let conn = test_conn();
        let pid = make_project(&conn);

        let spec = make_spec(pid, "test'; DROP TABLE specs; --");
        insert_spec(&conn, &spec).unwrap();

        let retrieved = get_spec_by_name(&conn, &pid, "test'; DROP TABLE specs; --")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.name, "test'; DROP TABLE specs; --");

        // Table should still exist
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM specs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_spec_with_claude_session_id_roundtrip() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let mut spec = make_spec(pid, "with-session");
        spec.claude_session_id = Some("session-abc-123".to_string());
        spec.session_active = true;
        insert_spec(&conn, &spec).unwrap();

        let retrieved = get_spec_by_name(&conn, &pid, "with-session")
            .unwrap()
            .unwrap();
        assert_eq!(
            retrieved.claude_session_id.as_deref(),
            Some("session-abc-123")
        );
        assert!(retrieved.session_active);
    }
}
