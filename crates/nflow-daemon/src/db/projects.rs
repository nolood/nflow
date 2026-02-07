use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use nflow_core::project::{GitProvider, Project};

use super::Result;

fn git_provider_to_str(provider: GitProvider) -> &'static str {
    match provider {
        GitProvider::Github => "github",
        GitProvider::Gitlab => "gitlab",
    }
}

fn git_provider_from_str(s: &str) -> GitProvider {
    match s {
        "gitlab" => GitProvider::Gitlab,
        _ => GitProvider::Github,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn row_to_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    let id_str: String = row.get("id")?;
    let name: String = row.get("name")?;
    let path: String = row.get("path")?;
    let base_branch: String = row.get("base_branch")?;
    let git_provider_str: String = row.get("git_provider")?;
    let execution_enabled: bool = row.get("execution_enabled")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;

    Ok(Project {
        id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()),
        name,
        path,
        base_branch,
        git_provider: git_provider_from_str(&git_provider_str),
        execution_enabled,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    })
}

pub fn insert_project(conn: &Connection, project: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (id, name, path, base_branch, git_provider, execution_enabled, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            project.id.to_string(),
            project.name,
            project.path,
            project.base_branch,
            git_provider_to_str(project.git_provider),
            project.execution_enabled,
            project.created_at.to_rfc3339(),
            project.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_project_by_name(conn: &Connection, name: &str) -> Result<Option<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, base_branch, git_provider, execution_enabled, created_at, updated_at
         FROM projects WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(params![name], row_to_project)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn get_project_by_id(conn: &Connection, id: &Uuid) -> Result<Option<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, base_branch, git_provider, execution_enabled, created_at, updated_at
         FROM projects WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_string()], row_to_project)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list_projects(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, base_branch, git_provider, execution_enabled, created_at, updated_at
         FROM projects ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_project)?;
    let mut projects = Vec::new();
    for row in rows {
        projects.push(row?);
    }
    Ok(projects)
}

pub fn update_project(conn: &Connection, project: &Project) -> Result<()> {
    conn.execute(
        "UPDATE projects SET name = ?1, path = ?2, base_branch = ?3, git_provider = ?4, execution_enabled = ?5, updated_at = ?6
         WHERE id = ?7",
        params![
            project.name,
            project.path,
            project.base_branch,
            git_provider_to_str(project.git_provider),
            project.execution_enabled,
            project.updated_at.to_rfc3339(),
            project.id.to_string(),
        ],
    )?;
    Ok(())
}

pub fn delete_project(conn: &Connection, id: &Uuid) -> Result<()> {
    conn.execute(
        "DELETE FROM projects WHERE id = ?1",
        params![id.to_string()],
    )?;
    Ok(())
}

pub fn project_name_exists(conn: &Connection, name: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM projects WHERE name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_conn;

    fn make_project(name: &str) -> Project {
        let now = Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            path: format!("/home/user/{}", name),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn test_insert_and_get_by_id() {
        let conn = test_conn();
        let project = make_project("test-project");
        insert_project(&conn, &project).unwrap();

        let retrieved = get_project_by_id(&conn, &project.id).unwrap().unwrap();
        assert_eq!(retrieved.id, project.id);
        assert_eq!(retrieved.name, "test-project");
        assert_eq!(retrieved.path, "/home/user/test-project");
        assert_eq!(retrieved.base_branch, "main");
        assert_eq!(retrieved.git_provider, GitProvider::Github);
        assert!(retrieved.execution_enabled);
    }

    #[test]
    fn test_insert_and_get_by_name() {
        let conn = test_conn();
        let project = make_project("my-project");
        insert_project(&conn, &project).unwrap();

        let retrieved = get_project_by_name(&conn, "my-project").unwrap().unwrap();
        assert_eq!(retrieved.id, project.id);
        assert_eq!(retrieved.name, "my-project");
    }

    #[test]
    fn test_get_by_name_not_found() {
        let conn = test_conn();
        let result = get_project_by_name(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_by_id_not_found() {
        let conn = test_conn();
        let result = get_project_by_id(&conn, &Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_list_projects_empty() {
        let conn = test_conn();
        let projects = list_projects(&conn).unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn test_list_projects_multiple() {
        let conn = test_conn();
        insert_project(&conn, &make_project("alpha")).unwrap();
        insert_project(&conn, &make_project("beta")).unwrap();
        insert_project(&conn, &make_project("gamma")).unwrap();

        let projects = list_projects(&conn).unwrap();
        assert_eq!(projects.len(), 3);
        // Ordered by name
        assert_eq!(projects[0].name, "alpha");
        assert_eq!(projects[1].name, "beta");
        assert_eq!(projects[2].name, "gamma");
    }

    #[test]
    fn test_update_project() {
        let conn = test_conn();
        let mut project = make_project("original");
        insert_project(&conn, &project).unwrap();

        project.name = "updated".to_string();
        project.path = "/new/path".to_string();
        project.base_branch = "develop".to_string();
        project.git_provider = GitProvider::Gitlab;
        project.execution_enabled = false;
        update_project(&conn, &project).unwrap();

        let retrieved = get_project_by_id(&conn, &project.id).unwrap().unwrap();
        assert_eq!(retrieved.name, "updated");
        assert_eq!(retrieved.path, "/new/path");
        assert_eq!(retrieved.base_branch, "develop");
        assert_eq!(retrieved.git_provider, GitProvider::Gitlab);
        assert!(!retrieved.execution_enabled);
    }

    #[test]
    fn test_delete_project() {
        let conn = test_conn();
        let project = make_project("to-delete");
        insert_project(&conn, &project).unwrap();

        delete_project(&conn, &project.id).unwrap();

        let result = get_project_by_id(&conn, &project.id).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_cascades_to_specs() {
        let conn = test_conn();
        let project = make_project("cascade-test");
        insert_project(&conn, &project).unwrap();

        // Insert a spec referencing this project
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'spec1', '/tmp/spec.md', 'draft', 0, '2024-01-01', '2024-01-01')",
            params![Uuid::new_v4().to_string(), project.id.to_string()],
        )
        .unwrap();

        delete_project(&conn, &project.id).unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM specs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_project_name_exists() {
        let conn = test_conn();
        assert!(!project_name_exists(&conn, "test").unwrap());

        insert_project(&conn, &make_project("test")).unwrap();
        assert!(project_name_exists(&conn, "test").unwrap());
        assert!(!project_name_exists(&conn, "other").unwrap());
    }

    #[test]
    fn test_insert_duplicate_name_fails() {
        let conn = test_conn();
        insert_project(&conn, &make_project("dup")).unwrap();

        let mut dup = make_project("dup");
        dup.id = Uuid::new_v4(); // different ID, same name
        let result = insert_project(&conn, &dup);
        assert!(result.is_err());
    }

    #[test]
    fn test_gitlab_provider_roundtrip() {
        let conn = test_conn();
        let now = Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: "gl-project".to_string(),
            path: "/home/user/gl".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Gitlab,
            execution_enabled: false,
            created_at: now,
            updated_at: now,
        };
        insert_project(&conn, &project).unwrap();

        let retrieved = get_project_by_id(&conn, &project.id).unwrap().unwrap();
        assert_eq!(retrieved.git_provider, GitProvider::Gitlab);
        assert!(!retrieved.execution_enabled);
    }

    #[test]
    fn test_parameterized_queries() {
        let conn = test_conn();
        // Attempt SQL injection via name — should be safely parameterized
        let mut project = make_project("test'; DROP TABLE projects; --");
        project.name = "test'; DROP TABLE projects; --".to_string();
        insert_project(&conn, &project).unwrap();

        let retrieved = get_project_by_name(&conn, "test'; DROP TABLE projects; --")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.name, "test'; DROP TABLE projects; --");

        // Table should still exist
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
