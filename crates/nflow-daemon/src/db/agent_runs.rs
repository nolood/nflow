use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use super::Result;

/// Status of an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

fn status_to_str(s: AgentRunStatus) -> &'static str {
    match s {
        AgentRunStatus::Running => "running",
        AgentRunStatus::Succeeded => "succeeded",
        AgentRunStatus::Failed => "failed",
        AgentRunStatus::Cancelled => "cancelled",
    }
}

fn status_from_str(s: &str) -> AgentRunStatus {
    match s {
        "succeeded" => AgentRunStatus::Succeeded,
        "failed" => AgentRunStatus::Failed,
        "cancelled" => AgentRunStatus::Cancelled,
        _ => AgentRunStatus::Running,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::nil())
}

/// Represents a single agent process execution for a work item.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub id: Uuid,
    pub work_item_id: Uuid,
    pub pid: Option<u32>,
    pub session_id: Option<String>,
    pub pid_start_time: Option<i64>,
    pub status: AgentRunStatus,
    pub exit_code: Option<i32>,
    pub log_path: Option<String>,
    pub error_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

fn row_to_agent_run(row: &Row<'_>) -> rusqlite::Result<AgentRun> {
    let id_str: String = row.get("id")?;
    let work_item_id_str: String = row.get("work_item_id")?;
    let pid: Option<u32> = row.get("pid")?;
    let session_id: Option<String> = row.get("session_id")?;
    let pid_start_time: Option<i64> = row.get("pid_start_time")?;
    let status_str: String = row.get("status")?;
    let exit_code: Option<i32> = row.get("exit_code")?;
    let log_path: Option<String> = row.get("log_path")?;
    let error_message: Option<String> = row.get("error_message")?;
    let started_at_str: String = row.get("started_at")?;
    let finished_at_str: Option<String> = row.get("finished_at")?;

    Ok(AgentRun {
        id: parse_uuid(&id_str),
        work_item_id: parse_uuid(&work_item_id_str),
        pid,
        session_id,
        pid_start_time,
        status: status_from_str(&status_str),
        exit_code,
        log_path,
        error_message,
        started_at: parse_datetime(&started_at_str),
        finished_at: finished_at_str.map(|s| parse_datetime(&s)),
    })
}

pub fn insert_agent_run(conn: &Connection, run: &AgentRun) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_runs (id, work_item_id, pid, session_id, pid_start_time, status, exit_code, log_path, error_message, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run.id.to_string(),
            run.work_item_id.to_string(),
            run.pid,
            run.session_id,
            run.pid_start_time,
            status_to_str(run.status),
            run.exit_code,
            run.log_path,
            run.error_message,
            run.started_at.to_rfc3339(),
            run.finished_at.map(|dt| dt.to_rfc3339()),
        ],
    )?;
    Ok(())
}

pub fn update_agent_run_status(
    conn: &Connection,
    id: &Uuid,
    status: AgentRunStatus,
    exit_code: Option<i32>,
    error_message: Option<&str>,
    finished_at: Option<DateTime<Utc>>,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET status = ?1, exit_code = ?2, error_message = ?3, finished_at = ?4 WHERE id = ?5",
        params![
            status_to_str(status),
            exit_code,
            error_message,
            finished_at.map(|dt| dt.to_rfc3339()),
            id.to_string(),
        ],
    )?;
    Ok(())
}

/// Update just the exit_code on an agent run (called from background task after process exits).
pub fn update_agent_run_exit_code(
    conn: &Connection,
    id: &Uuid,
    exit_code: Option<i32>,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET exit_code = ?1 WHERE id = ?2",
        params![exit_code, id.to_string()],
    )?;
    Ok(())
}

pub fn find_running_agent_runs(conn: &Connection) -> Result<Vec<AgentRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, work_item_id, pid, session_id, pid_start_time, status, exit_code, log_path, error_message, started_at, finished_at
         FROM agent_runs WHERE status = 'running'",
    )?;
    let rows = stmt.query_map([], row_to_agent_run)?;
    let mut runs = Vec::new();
    for row in rows {
        runs.push(row?);
    }
    Ok(runs)
}

/// Find running agent runs for a specific project.
pub fn find_running_agent_runs_by_project(
    conn: &Connection,
    project_id: &Uuid,
) -> Result<Vec<AgentRun>> {
    let mut stmt = conn.prepare(
        "SELECT ar.id, ar.work_item_id, ar.pid, ar.session_id, ar.pid_start_time, ar.status, ar.exit_code, ar.log_path, ar.error_message, ar.started_at, ar.finished_at
         FROM agent_runs ar
         JOIN work_items w ON ar.work_item_id = w.id
         JOIN decomposition_sessions ds ON w.decomposition_session_id = ds.id
         WHERE ar.status = 'running' AND ds.project_id = ?1",
    )?;
    let rows = stmt.query_map(params![project_id.to_string()], row_to_agent_run)?;
    let mut runs = Vec::new();
    for row in rows {
        runs.push(row?);
    }
    Ok(runs)
}

/// Cancel running agent runs for a project (mark as cancelled in DB).
pub fn cancel_running_agent_runs_by_project(conn: &Connection, project_id: &Uuid) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE agent_runs SET status = 'cancelled', finished_at = ?1
         WHERE status = 'running' AND work_item_id IN (
             SELECT w.id FROM work_items w
             JOIN decomposition_sessions ds ON w.decomposition_session_id = ds.id
             WHERE ds.project_id = ?2
         )",
        params![Utc::now().to_rfc3339(), project_id.to_string()],
    )?;
    Ok(changed as u64)
}

/// Find running agent runs for tasks within a specific story.
pub fn find_running_agent_runs_for_story(
    conn: &Connection,
    story_id: &Uuid,
) -> Result<Vec<AgentRun>> {
    let mut stmt = conn.prepare(
        "SELECT ar.id, ar.work_item_id, ar.pid, ar.session_id, ar.pid_start_time, ar.status, ar.exit_code, ar.log_path, ar.error_message, ar.started_at, ar.finished_at
         FROM agent_runs ar
         JOIN work_items w ON ar.work_item_id = w.id
         WHERE ar.status = 'running' AND w.parent_id = ?1",
    )?;
    let rows = stmt.query_map(params![story_id.to_string()], row_to_agent_run)?;
    let mut runs = Vec::new();
    for row in rows {
        runs.push(row?);
    }
    Ok(runs)
}

/// Update the Claude session_id on an agent run (stored after parsing stream output).
pub fn update_agent_run_session_id(conn: &Connection, id: &Uuid, session_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE agent_runs SET session_id = ?1 WHERE id = ?2",
        params![session_id, id.to_string()],
    )?;
    Ok(())
}

pub fn count_agent_runs_for_task(conn: &Connection, work_item_id: &Uuid) -> Result<u32> {
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM agent_runs WHERE work_item_id = ?1",
        params![work_item_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(count)
}


/// Find the most recent agent run for a given task (by started_at descending).
/// Find an agent run by its ID.
pub fn find_agent_run_by_id(conn: &Connection, id: &Uuid) -> Result<Option<AgentRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, work_item_id, pid, session_id, pid_start_time, status, exit_code, log_path, error_message, started_at, finished_at
         FROM agent_runs WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_string()], row_to_agent_run)?;
    match rows.next() {
        Some(Ok(run)) => Ok(Some(run)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

pub fn find_latest_agent_run_for_task(
    conn: &Connection,
    work_item_id: &Uuid,
) -> Result<Option<AgentRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, work_item_id, pid, session_id, pid_start_time, status, exit_code, log_path, error_message, started_at, finished_at
         FROM agent_runs WHERE work_item_id = ?1 ORDER BY started_at DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map(params![work_item_id.to_string()], row_to_agent_run)?;
    match rows.next() {
        Some(Ok(run)) => Ok(Some(run)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{projects::insert_project, test_conn};
    use nflow_core::project::{GitProvider, Project};
    use nflow_core::work_item::WorkItem;

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

    fn make_session(conn: &Connection, project_id: Uuid) -> Uuid {
        let session_id = Uuid::new_v4();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'in_progress', ?3, ?3)",
            params![
                session_id.to_string(),
                project_id.to_string(),
                now.to_rfc3339(),
            ],
        )
        .unwrap();
        session_id
    }

    fn make_work_item(conn: &Connection, session_id: Uuid) -> Uuid {
        let epic = WorkItem::new_epic(session_id, "Epic 1".into(), "Desc".into(), "E1".into(), 0);
        crate::db::work_items::insert_work_item(conn, &epic).unwrap();
        epic.id
    }

    fn make_agent_run(work_item_id: Uuid) -> AgentRun {
        AgentRun {
            id: Uuid::new_v4(),
            work_item_id,
            pid: Some(12345),
            session_id: Some("claude-session-abc".to_string()),
            pid_start_time: Some(1700000000),
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: Some("/logs/task.log".to_string()),
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        }
    }

    // --- insert_agent_run ---

    #[test]
    fn test_insert_and_find_running() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run = make_agent_run(wid);
        insert_agent_run(&conn, &run).unwrap();

        let running = find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, run.id);
        assert_eq!(running[0].work_item_id, wid);
        assert_eq!(running[0].pid, Some(12345));
        assert_eq!(running[0].session_id.as_deref(), Some("claude-session-abc"));
        assert_eq!(running[0].pid_start_time, Some(1700000000));
        assert_eq!(running[0].status, AgentRunStatus::Running);
        assert!(running[0].exit_code.is_none());
        assert_eq!(running[0].log_path.as_deref(), Some("/logs/task.log"));
        assert!(running[0].error_message.is_none());
        assert!(running[0].finished_at.is_none());
    }

    #[test]
    fn test_insert_agent_run_minimal_fields() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run = AgentRun {
            id: Uuid::new_v4(),
            work_item_id: wid,
            pid: None,
            session_id: None,
            pid_start_time: None,
            status: AgentRunStatus::Running,
            exit_code: None,
            log_path: None,
            error_message: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        insert_agent_run(&conn, &run).unwrap();

        let running = find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 1);
        assert!(running[0].pid.is_none());
        assert!(running[0].session_id.is_none());
        assert!(running[0].pid_start_time.is_none());
        assert!(running[0].log_path.is_none());
    }

    // --- update_agent_run_status ---

    #[test]
    fn test_update_agent_run_status_succeeded() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run = make_agent_run(wid);
        insert_agent_run(&conn, &run).unwrap();

        let finished = Utc::now();
        update_agent_run_status(
            &conn,
            &run.id,
            AgentRunStatus::Succeeded,
            Some(0),
            None,
            Some(finished),
        )
        .unwrap();

        // Should no longer be in running list
        let running = find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());
    }

    #[test]
    fn test_update_agent_run_status_failed_with_error() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run = make_agent_run(wid);
        insert_agent_run(&conn, &run).unwrap();

        let finished = Utc::now();
        update_agent_run_status(
            &conn,
            &run.id,
            AgentRunStatus::Failed,
            Some(1),
            Some("timeout exceeded"),
            Some(finished),
        )
        .unwrap();

        let running = find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());
    }

    #[test]
    fn test_update_agent_run_status_cancelled() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run = make_agent_run(wid);
        insert_agent_run(&conn, &run).unwrap();

        update_agent_run_status(
            &conn,
            &run.id,
            AgentRunStatus::Cancelled,
            None,
            None,
            Some(Utc::now()),
        )
        .unwrap();

        let running = find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());
    }

    // --- find_running_agent_runs ---

    #[test]
    fn test_find_running_agent_runs_empty() {
        let conn = test_conn();
        let running = find_running_agent_runs(&conn).unwrap();
        assert!(running.is_empty());
    }

    #[test]
    fn test_find_running_agent_runs_excludes_finished() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        // Insert a running agent
        let run1 = make_agent_run(wid);
        insert_agent_run(&conn, &run1).unwrap();

        // Insert a completed agent
        let run2 = AgentRun {
            id: Uuid::new_v4(),
            work_item_id: wid,
            pid: Some(12346),
            session_id: None,
            pid_start_time: None,
            status: AgentRunStatus::Succeeded,
            exit_code: Some(0),
            log_path: None,
            error_message: None,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
        };
        // Insert directly with succeeded status
        insert_agent_run(&conn, &run2).unwrap();

        let running = find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, run1.id);
    }

    #[test]
    fn test_find_running_multiple() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid1 = make_work_item(&conn, sid);

        let run1 = make_agent_run(wid1);
        let mut run2 = make_agent_run(wid1);
        run2.pid = Some(12346);
        insert_agent_run(&conn, &run1).unwrap();
        insert_agent_run(&conn, &run2).unwrap();

        let running = find_running_agent_runs(&conn).unwrap();
        assert_eq!(running.len(), 2);
    }

    // --- count_agent_runs_for_task ---

    #[test]
    fn test_count_agent_runs_for_task() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run1 = make_agent_run(wid);
        let run2 = AgentRun {
            id: Uuid::new_v4(),
            ..make_agent_run(wid)
        };
        let run3 = AgentRun {
            id: Uuid::new_v4(),
            ..make_agent_run(wid)
        };
        insert_agent_run(&conn, &run1).unwrap();
        insert_agent_run(&conn, &run2).unwrap();
        insert_agent_run(&conn, &run3).unwrap();

        let count = count_agent_runs_for_task(&conn, &wid).unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_count_agent_runs_for_task_zero() {
        let conn = test_conn();
        let count = count_agent_runs_for_task(&conn, &Uuid::new_v4()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_count_agent_runs_scoped_to_work_item() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid1 = make_work_item(&conn, sid);

        // Create a second work item (story under the epic)
        let story = WorkItem::new_story(
            wid1,
            sid,
            "Story".into(),
            "Desc".into(),
            "AC".into(),
            "S1".into(),
            1,
        );
        crate::db::work_items::insert_work_item(&conn, &story).unwrap();
        let wid2 = story.id;

        let run1 = make_agent_run(wid1);
        let run2 = make_agent_run(wid2);
        insert_agent_run(&conn, &run1).unwrap();
        insert_agent_run(&conn, &run2).unwrap();

        assert_eq!(count_agent_runs_for_task(&conn, &wid1).unwrap(), 1);
        assert_eq!(count_agent_runs_for_task(&conn, &wid2).unwrap(), 1);
    }

    // --- cascade on work item delete ---

    #[test]
    fn test_cascade_delete_on_work_item() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let run = make_agent_run(wid);
        insert_agent_run(&conn, &run).unwrap();

        // Delete the work item — agent run should cascade
        conn.execute(
            "DELETE FROM work_items WHERE id = ?1",
            params![wid.to_string()],
        )
        .unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // --- parameterized queries ---

    #[test]
    fn test_parameterized_queries() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let wid = make_work_item(&conn, sid);

        let mut run = make_agent_run(wid);
        run.error_message = Some("'; DROP TABLE agent_runs; --".to_string());
        run.log_path = Some("'; DROP TABLE agent_runs; --".to_string());
        insert_agent_run(&conn, &run).unwrap();

        // Table should still exist
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM agent_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
