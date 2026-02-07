use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use uuid::Uuid;

use nflow_core::work_item::{Dependency, ItemType, TaskKind, WorkItem, WorkItemStatus};

use super::Result;

fn item_type_to_str(t: ItemType) -> &'static str {
    match t {
        ItemType::Epic => "epic",
        ItemType::Story => "story",
        ItemType::Task => "task",
    }
}

fn item_type_from_str(s: &str) -> ItemType {
    match s {
        "story" => ItemType::Story,
        "task" => ItemType::Task,
        _ => ItemType::Epic,
    }
}

fn task_kind_to_str(k: TaskKind) -> &'static str {
    match k {
        TaskKind::Impl => "impl",
        TaskKind::Verify => "verify",
    }
}

fn task_kind_from_str(s: &str) -> Option<TaskKind> {
    match s {
        "impl" => Some(TaskKind::Impl),
        "verify" => Some(TaskKind::Verify),
        _ => None,
    }
}

fn status_to_str(s: WorkItemStatus) -> &'static str {
    match s {
        WorkItemStatus::Pending => "pending",
        WorkItemStatus::Ready => "ready",
        WorkItemStatus::InProgress => "in_progress",
        WorkItemStatus::Done => "done",
        WorkItemStatus::Failed => "failed",
        WorkItemStatus::Cancelled => "cancelled",
    }
}

fn status_from_str(s: &str) -> WorkItemStatus {
    match s {
        "ready" => WorkItemStatus::Ready,
        "in_progress" => WorkItemStatus::InProgress,
        "done" => WorkItemStatus::Done,
        "failed" => WorkItemStatus::Failed,
        "cancelled" => WorkItemStatus::Cancelled,
        _ => WorkItemStatus::Pending,
    }
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::nil())
}

fn row_to_work_item(row: &Row<'_>) -> rusqlite::Result<WorkItem> {
    let id_str: String = row.get("id")?;
    let parent_id_str: Option<String> = row.get("parent_id")?;
    let session_id_str: String = row.get("decomposition_session_id")?;
    let item_type_str: String = row.get("item_type")?;
    let kind_str: Option<String> = row.get("kind")?;
    let title: String = row.get("title")?;
    let description: String = row.get("description")?;
    let acceptance_criteria: String = row.get("acceptance_criteria")?;
    let status_str: String = row.get("status")?;
    let short_id: String = row.get("short_id")?;
    let sort_order: i32 = row.get("sort_order")?;
    let branch_name: Option<String> = row.get("branch_name")?;
    let worktree_path: Option<String> = row.get("worktree_path")?;
    let mr_url: Option<String> = row.get("mr_url")?;
    let commit_hash: Option<String> = row.get("commit_hash")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;

    Ok(WorkItem {
        id: parse_uuid(&id_str),
        parent_id: parent_id_str.map(|s| parse_uuid(&s)),
        decomposition_session_id: parse_uuid(&session_id_str),
        item_type: item_type_from_str(&item_type_str),
        kind: kind_str.and_then(|s| task_kind_from_str(&s)),
        title,
        description,
        acceptance_criteria,
        status: status_from_str(&status_str),
        short_id,
        sort_order,
        branch_name,
        worktree_path,
        mr_url,
        commit_hash,
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    })
}

pub fn insert_work_item(conn: &Connection, item: &WorkItem) -> Result<()> {
    conn.execute(
        "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, acceptance_criteria, status, short_id, sort_order, branch_name, worktree_path, mr_url, commit_hash, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            item.id.to_string(),
            item.parent_id.map(|id| id.to_string()),
            item.decomposition_session_id.to_string(),
            item_type_to_str(item.item_type),
            item.kind.map(task_kind_to_str),
            item.title,
            item.description,
            item.acceptance_criteria,
            status_to_str(item.status),
            item.short_id,
            item.sort_order,
            item.branch_name,
            item.worktree_path,
            item.mr_url,
            item.commit_hash,
            item.created_at.to_rfc3339(),
            item.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn insert_dependency(conn: &Connection, dep: &Dependency) -> Result<()> {
    conn.execute(
        "INSERT INTO dependencies (blocker_id, blocked_id) VALUES (?1, ?2)",
        params![dep.blocker_id.to_string(), dep.blocked_id.to_string()],
    )?;
    Ok(())
}

pub fn get_work_item_by_id(conn: &Connection, id: &Uuid) -> Result<Option<WorkItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, decomposition_session_id, item_type, kind, title, description, acceptance_criteria, status, short_id, sort_order, branch_name, worktree_path, mr_url, commit_hash, created_at, updated_at
         FROM work_items WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id.to_string()], row_to_work_item)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn list_work_items_by_session(conn: &Connection, session_id: &Uuid) -> Result<Vec<WorkItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, decomposition_session_id, item_type, kind, title, description, acceptance_criteria, status, short_id, sort_order, branch_name, worktree_path, mr_url, commit_hash, created_at, updated_at
         FROM work_items WHERE decomposition_session_id = ?1 ORDER BY sort_order",
    )?;
    let rows = stmt.query_map(params![session_id.to_string()], row_to_work_item)?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

pub fn list_work_items_by_parent(conn: &Connection, parent_id: &Uuid) -> Result<Vec<WorkItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, decomposition_session_id, item_type, kind, title, description, acceptance_criteria, status, short_id, sort_order, branch_name, worktree_path, mr_url, commit_hash, created_at, updated_at
         FROM work_items WHERE parent_id = ?1 ORDER BY sort_order",
    )?;
    let rows = stmt.query_map(params![parent_id.to_string()], row_to_work_item)?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

pub fn list_dependencies_by_session(
    conn: &Connection,
    session_id: &Uuid,
) -> Result<Vec<Dependency>> {
    let mut stmt = conn.prepare(
        "SELECT d.blocker_id, d.blocked_id
         FROM dependencies d
         JOIN work_items w ON d.blocker_id = w.id
         WHERE w.decomposition_session_id = ?1",
    )?;
    let rows = stmt.query_map(params![session_id.to_string()], |row| {
        let blocker_str: String = row.get("blocker_id")?;
        let blocked_str: String = row.get("blocked_id")?;
        Ok(Dependency::new(
            parse_uuid(&blocker_str),
            parse_uuid(&blocked_str),
        ))
    })?;
    let mut deps = Vec::new();
    for row in rows {
        deps.push(row?);
    }
    Ok(deps)
}

pub fn list_blockers_for_story(conn: &Connection, story_id: &Uuid) -> Result<Vec<Dependency>> {
    let mut stmt =
        conn.prepare("SELECT blocker_id, blocked_id FROM dependencies WHERE blocked_id = ?1")?;
    let rows = stmt.query_map(params![story_id.to_string()], |row| {
        let blocker_str: String = row.get("blocker_id")?;
        let blocked_str: String = row.get("blocked_id")?;
        Ok(Dependency::new(
            parse_uuid(&blocker_str),
            parse_uuid(&blocked_str),
        ))
    })?;
    let mut deps = Vec::new();
    for row in rows {
        deps.push(row?);
    }
    Ok(deps)
}

pub fn list_dependents_of_story(conn: &Connection, story_id: &Uuid) -> Result<Vec<Dependency>> {
    let mut stmt =
        conn.prepare("SELECT blocker_id, blocked_id FROM dependencies WHERE blocker_id = ?1")?;
    let rows = stmt.query_map(params![story_id.to_string()], |row| {
        let blocker_str: String = row.get("blocker_id")?;
        let blocked_str: String = row.get("blocked_id")?;
        Ok(Dependency::new(
            parse_uuid(&blocker_str),
            parse_uuid(&blocked_str),
        ))
    })?;
    let mut deps = Vec::new();
    for row in rows {
        deps.push(row?);
    }
    Ok(deps)
}

pub fn update_work_item_status(conn: &Connection, id: &Uuid, status: WorkItemStatus) -> Result<()> {
    conn.execute(
        "UPDATE work_items SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![
            status_to_str(status),
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(())
}

pub fn update_work_item_commit(conn: &Connection, id: &Uuid, commit_hash: &str) -> Result<()> {
    conn.execute(
        "UPDATE work_items SET commit_hash = ?1, updated_at = ?2 WHERE id = ?3",
        params![commit_hash, Utc::now().to_rfc3339(), id.to_string()],
    )?;
    Ok(())
}

pub fn update_story_worktree(
    conn: &Connection,
    id: &Uuid,
    branch_name: &str,
    worktree_path: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE work_items SET branch_name = ?1, worktree_path = ?2, updated_at = ?3 WHERE id = ?4",
        params![
            branch_name,
            worktree_path,
            Utc::now().to_rfc3339(),
            id.to_string(),
        ],
    )?;
    Ok(())
}

pub fn update_story_mr(conn: &Connection, id: &Uuid, mr_url: &str) -> Result<()> {
    conn.execute(
        "UPDATE work_items SET mr_url = ?1, updated_at = ?2 WHERE id = ?3",
        params![mr_url, Utc::now().to_rfc3339(), id.to_string()],
    )?;
    Ok(())
}

pub fn delete_work_items_by_session(conn: &Connection, session_id: &Uuid) -> Result<()> {
    conn.execute(
        "DELETE FROM work_items WHERE decomposition_session_id = ?1",
        params![session_id.to_string()],
    )?;
    Ok(())
}

pub fn count_tasks_by_status(
    conn: &Connection,
    story_id: &Uuid,
) -> Result<HashMap<WorkItemStatus, u32>> {
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) as cnt
         FROM work_items
         WHERE parent_id = ?1 AND item_type = 'task'
         GROUP BY status",
    )?;
    let rows = stmt.query_map(params![story_id.to_string()], |row| {
        let status_str: String = row.get("status")?;
        let count: u32 = row.get("cnt")?;
        Ok((status_from_str(&status_str), count))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (status, count) = row?;
        map.insert(status, count);
    }
    Ok(map)
}

/// Count stories for a project (across all decomposition sessions).
pub fn count_stories_by_project(conn: &Connection, project_id: &Uuid) -> Result<u32> {
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM work_items w
         JOIN decomposition_sessions ds ON w.decomposition_session_id = ds.id
         WHERE ds.project_id = ?1 AND w.item_type = 'story'",
        params![project_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Count stories with pending or ready status for a project (schedulable but not yet started).
pub fn count_pending_stories_by_project(conn: &Connection, project_id: &Uuid) -> Result<u32> {
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM work_items w
         JOIN decomposition_sessions ds ON w.decomposition_session_id = ds.id
         WHERE ds.project_id = ?1 AND w.item_type = 'story'
         AND w.status IN ('pending', 'ready')",
        params![project_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// List worktree paths for all stories in a project.
pub fn list_worktree_paths_by_project(conn: &Connection, project_id: &Uuid) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT w.worktree_path FROM work_items w
         JOIN decomposition_sessions ds ON w.decomposition_session_id = ds.id
         WHERE ds.project_id = ?1 AND w.item_type = 'story' AND w.worktree_path IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![project_id.to_string()], |row| {
        let path: String = row.get(0)?;
        Ok(path)
    })?;
    let mut paths = Vec::new();
    for row in rows {
        paths.push(row?);
    }
    Ok(paths)
}

/// Check if a session has any stories with in_progress status.
pub fn has_in_progress_stories_by_session(conn: &Connection, session_id: &Uuid) -> Result<bool> {
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM work_items
         WHERE decomposition_session_id = ?1 AND item_type = 'story' AND status = 'in_progress'",
        params![session_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Find a work item by its wave-prefixed short ID (e.g., "W1-T1").
/// Parses the wave number from the prefix and looks up the item by short_id
/// within the matching decomposition session.
pub fn find_work_item_by_wave_short_id(
    conn: &Connection,
    wave_short_id: &str,
) -> Result<Option<WorkItem>> {
    // Parse "W{wave}-{short_id}" format
    let (wave_number, short_id) = match parse_wave_prefix(wave_short_id) {
        Some(parsed) => parsed,
        None => return Ok(None),
    };

    let mut stmt = conn.prepare(
        "SELECT w.id, w.parent_id, w.decomposition_session_id, w.item_type, w.kind, w.title, w.description, w.acceptance_criteria, w.status, w.short_id, w.sort_order, w.branch_name, w.worktree_path, w.mr_url, w.commit_hash, w.created_at, w.updated_at
         FROM work_items w
         JOIN decomposition_sessions ds ON w.decomposition_session_id = ds.id
         WHERE w.short_id = ?1 AND ds.wave_number = ?2",
    )?;
    let mut rows = stmt.query_map(params![short_id, wave_number], row_to_work_item)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Parse a wave-prefixed short ID like "W1-T1" into (wave_number, short_id).
fn parse_wave_prefix(wave_short_id: &str) -> Option<(u32, &str)> {
    let rest = wave_short_id.strip_prefix('W')?;
    let dash_pos = rest.find('-')?;
    let wave_number: u32 = rest[..dash_pos].parse().ok()?;
    let short_id = &rest[dash_pos + 1..];
    if short_id.is_empty() {
        return None;
    }
    Some((wave_number, short_id))
}

/// List all stories in a specific session (wave).
pub fn list_stories_by_session(conn: &Connection, session_id: &Uuid) -> Result<Vec<WorkItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, decomposition_session_id, item_type, kind, title, description, acceptance_criteria, status, short_id, sort_order, branch_name, worktree_path, mr_url, commit_hash, created_at, updated_at
         FROM work_items WHERE decomposition_session_id = ?1 AND item_type = 'story' ORDER BY sort_order",
    )?;
    let rows = stmt.query_map(params![session_id.to_string()], row_to_work_item)?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

/// Cancel all in-progress work items for a project.
/// Returns the number of items cancelled.
pub fn cancel_in_progress_items_by_project(conn: &Connection, project_id: &Uuid) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE work_items SET status = 'cancelled', updated_at = ?1
         WHERE decomposition_session_id IN (
             SELECT id FROM decomposition_sessions WHERE project_id = ?2
         ) AND status = 'in_progress'",
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

    fn make_epic(session_id: Uuid) -> WorkItem {
        WorkItem::new_epic(session_id, "Epic 1".into(), "Desc".into(), "E1".into(), 0)
    }

    fn make_story(epic_id: Uuid, session_id: Uuid, short_id: &str, sort_order: i32) -> WorkItem {
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

    fn make_task(story_id: Uuid, session_id: Uuid, short_id: &str, sort_order: i32) -> WorkItem {
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

    // Helper to insert an epic into the DB and return it
    fn insert_epic(conn: &Connection, session_id: Uuid) -> WorkItem {
        let epic = make_epic(session_id);
        insert_work_item(conn, &epic).unwrap();
        epic
    }

    // --- insert_work_item and get_work_item_by_id ---

    #[test]
    fn test_insert_and_get_epic() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = make_epic(sid);
        insert_work_item(&conn, &epic).unwrap();

        let retrieved = get_work_item_by_id(&conn, &epic.id).unwrap().unwrap();
        assert_eq!(retrieved.id, epic.id);
        assert_eq!(retrieved.item_type, ItemType::Epic);
        assert!(retrieved.parent_id.is_none());
        assert_eq!(retrieved.decomposition_session_id, sid);
        assert!(retrieved.kind.is_none());
        assert_eq!(retrieved.title, "Epic 1");
        assert_eq!(retrieved.status, WorkItemStatus::Pending);
        assert_eq!(retrieved.short_id, "E1");
        assert_eq!(retrieved.sort_order, 0);
    }

    #[test]
    fn test_insert_and_get_story() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        let retrieved = get_work_item_by_id(&conn, &story.id).unwrap().unwrap();
        assert_eq!(retrieved.id, story.id);
        assert_eq!(retrieved.item_type, ItemType::Story);
        assert_eq!(retrieved.parent_id, Some(epic.id));
        assert!(retrieved.kind.is_none());
        assert_eq!(retrieved.title, "Story S1");
    }

    #[test]
    fn test_insert_and_get_task() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();
        let task = make_task(story.id, sid, "T1", 0);
        insert_work_item(&conn, &task).unwrap();

        let retrieved = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(retrieved.id, task.id);
        assert_eq!(retrieved.item_type, ItemType::Task);
        assert_eq!(retrieved.kind, Some(TaskKind::Impl));
        assert_eq!(retrieved.parent_id, Some(story.id));
        assert_eq!(retrieved.title, "Task T1");
    }

    #[test]
    fn test_get_work_item_not_found() {
        let conn = test_conn();
        let result = get_work_item_by_id(&conn, &Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    // --- insert_dependency ---

    #[test]
    fn test_insert_dependency() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let s1 = make_story(epic.id, sid, "S1", 0);
        let s2 = make_story(epic.id, sid, "S2", 1);
        insert_work_item(&conn, &s1).unwrap();
        insert_work_item(&conn, &s2).unwrap();

        let dep = Dependency::new(s1.id, s2.id);
        insert_dependency(&conn, &dep).unwrap();

        let blockers = list_blockers_for_story(&conn, &s2.id).unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].blocker_id, s1.id);
        assert_eq!(blockers[0].blocked_id, s2.id);
    }

    // --- list_work_items_by_session ---

    #[test]
    fn test_list_work_items_by_session() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let s1 = make_story(epic.id, sid, "S1", 1);
        let s2 = make_story(epic.id, sid, "S2", 2);
        insert_work_item(&conn, &s1).unwrap();
        insert_work_item(&conn, &s2).unwrap();

        let items = list_work_items_by_session(&conn, &sid).unwrap();
        // epic (sort_order=0), s1 (sort_order=1), s2 (sort_order=2)
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].short_id, "E1");
        assert_eq!(items[1].short_id, "S1");
        assert_eq!(items[2].short_id, "S2");
    }

    #[test]
    fn test_list_work_items_by_session_ordered_by_sort_order() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        // Insert in reverse order
        let epic = make_epic(sid);
        insert_work_item(&conn, &epic).unwrap();
        let s2 = make_story(epic.id, sid, "S2", 5);
        let s1 = make_story(epic.id, sid, "S1", 1);
        insert_work_item(&conn, &s2).unwrap();
        insert_work_item(&conn, &s1).unwrap();

        let items = list_work_items_by_session(&conn, &sid).unwrap();
        assert_eq!(items[0].sort_order, 0); // epic
        assert_eq!(items[1].sort_order, 1); // S1
        assert_eq!(items[2].sort_order, 5); // S2
    }

    #[test]
    fn test_list_work_items_by_session_empty() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let items = list_work_items_by_session(&conn, &sid).unwrap();
        assert!(items.is_empty());
    }

    // --- list_work_items_by_parent ---

    #[test]
    fn test_list_work_items_by_parent() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let s1 = make_story(epic.id, sid, "S1", 0);
        let s2 = make_story(epic.id, sid, "S2", 1);
        insert_work_item(&conn, &s1).unwrap();
        insert_work_item(&conn, &s2).unwrap();

        let children = list_work_items_by_parent(&conn, &epic.id).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].short_id, "S1");
        assert_eq!(children[1].short_id, "S2");
    }

    #[test]
    fn test_list_work_items_by_parent_tasks() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();
        let t1 = make_task(story.id, sid, "T1", 0);
        let t2 = make_task(story.id, sid, "T2", 2);
        insert_work_item(&conn, &t1).unwrap();
        insert_work_item(&conn, &t2).unwrap();

        let children = list_work_items_by_parent(&conn, &story.id).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].short_id, "T1");
        assert_eq!(children[1].short_id, "T2");
    }

    // --- list_dependencies_by_session ---

    #[test]
    fn test_list_dependencies_by_session() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let s1 = make_story(epic.id, sid, "S1", 0);
        let s2 = make_story(epic.id, sid, "S2", 1);
        let s3 = make_story(epic.id, sid, "S3", 2);
        insert_work_item(&conn, &s1).unwrap();
        insert_work_item(&conn, &s2).unwrap();
        insert_work_item(&conn, &s3).unwrap();

        let dep1 = Dependency::new(s1.id, s2.id);
        let dep2 = Dependency::new(s1.id, s3.id);
        insert_dependency(&conn, &dep1).unwrap();
        insert_dependency(&conn, &dep2).unwrap();

        let deps = list_dependencies_by_session(&conn, &sid).unwrap();
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_list_dependencies_by_session_empty() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let deps = list_dependencies_by_session(&conn, &sid).unwrap();
        assert!(deps.is_empty());
    }

    // --- list_blockers_for_story ---

    #[test]
    fn test_list_blockers_for_story() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let s1 = make_story(epic.id, sid, "S1", 0);
        let s2 = make_story(epic.id, sid, "S2", 1);
        let s3 = make_story(epic.id, sid, "S3", 2);
        insert_work_item(&conn, &s1).unwrap();
        insert_work_item(&conn, &s2).unwrap();
        insert_work_item(&conn, &s3).unwrap();

        // S3 is blocked by both S1 and S2
        insert_dependency(&conn, &Dependency::new(s1.id, s3.id)).unwrap();
        insert_dependency(&conn, &Dependency::new(s2.id, s3.id)).unwrap();

        let blockers = list_blockers_for_story(&conn, &s3.id).unwrap();
        assert_eq!(blockers.len(), 2);

        // S1 has no blockers
        let blockers = list_blockers_for_story(&conn, &s1.id).unwrap();
        assert!(blockers.is_empty());
    }

    // --- update_work_item_status ---

    #[test]
    fn test_update_work_item_status() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        update_work_item_status(&conn, &story.id, WorkItemStatus::Ready).unwrap();

        let retrieved = get_work_item_by_id(&conn, &story.id).unwrap().unwrap();
        assert_eq!(retrieved.status, WorkItemStatus::Ready);
    }

    #[test]
    fn test_update_work_item_status_multiple_transitions() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        update_work_item_status(&conn, &story.id, WorkItemStatus::Ready).unwrap();
        update_work_item_status(&conn, &story.id, WorkItemStatus::InProgress).unwrap();
        update_work_item_status(&conn, &story.id, WorkItemStatus::Done).unwrap();

        let retrieved = get_work_item_by_id(&conn, &story.id).unwrap().unwrap();
        assert_eq!(retrieved.status, WorkItemStatus::Done);
    }

    // --- update_work_item_commit ---

    #[test]
    fn test_update_work_item_commit() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();
        let task = make_task(story.id, sid, "T1", 0);
        insert_work_item(&conn, &task).unwrap();

        update_work_item_commit(&conn, &task.id, "abc123def456").unwrap();

        let retrieved = get_work_item_by_id(&conn, &task.id).unwrap().unwrap();
        assert_eq!(retrieved.commit_hash, Some("abc123def456".to_string()));
    }

    // --- update_story_worktree ---

    #[test]
    fn test_update_story_worktree() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        update_story_worktree(&conn, &story.id, "feature/story-1", "/worktrees/story-1").unwrap();

        let retrieved = get_work_item_by_id(&conn, &story.id).unwrap().unwrap();
        assert_eq!(retrieved.branch_name, Some("feature/story-1".to_string()));
        assert_eq!(
            retrieved.worktree_path,
            Some("/worktrees/story-1".to_string())
        );
    }

    // --- update_story_mr ---

    #[test]
    fn test_update_story_mr() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        update_story_mr(&conn, &story.id, "https://github.com/org/repo/pull/42").unwrap();

        let retrieved = get_work_item_by_id(&conn, &story.id).unwrap().unwrap();
        assert_eq!(
            retrieved.mr_url,
            Some("https://github.com/org/repo/pull/42".to_string())
        );
    }

    // --- delete_work_items_by_session ---

    #[test]
    fn test_delete_work_items_by_session() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();
        let task = make_task(story.id, sid, "T1", 0);
        insert_work_item(&conn, &task).unwrap();

        delete_work_items_by_session(&conn, &sid).unwrap();

        let items = list_work_items_by_session(&conn, &sid).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_delete_work_items_by_session_cascades_dependencies() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let s1 = make_story(epic.id, sid, "S1", 0);
        let s2 = make_story(epic.id, sid, "S2", 1);
        insert_work_item(&conn, &s1).unwrap();
        insert_work_item(&conn, &s2).unwrap();
        insert_dependency(&conn, &Dependency::new(s1.id, s2.id)).unwrap();

        delete_work_items_by_session(&conn, &sid).unwrap();

        let dep_count: u32 = conn
            .query_row("SELECT COUNT(*) FROM dependencies", [], |row| row.get(0))
            .unwrap();
        assert_eq!(dep_count, 0);
    }

    #[test]
    fn test_delete_work_items_scoped_to_session() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid1 = make_session(&conn, pid);
        let sid2 = Uuid::new_v4();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 2, 'in_progress', ?3, ?3)",
            params![sid2.to_string(), pid.to_string(), now.to_rfc3339()],
        )
        .unwrap();

        let epic1 = make_epic(sid1);
        insert_work_item(&conn, &epic1).unwrap();
        let epic2 = WorkItem::new_epic(sid2, "Epic 2".into(), "Desc".into(), "E2".into(), 0);
        insert_work_item(&conn, &epic2).unwrap();

        delete_work_items_by_session(&conn, &sid1).unwrap();

        let items1 = list_work_items_by_session(&conn, &sid1).unwrap();
        assert!(items1.is_empty());
        let items2 = list_work_items_by_session(&conn, &sid2).unwrap();
        assert_eq!(items2.len(), 1);
    }

    // --- count_tasks_by_status ---

    #[test]
    fn test_count_tasks_by_status() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        let t1 = make_task(story.id, sid, "T1", 0);
        let t2 = make_task(story.id, sid, "T2", 2);
        let t3 = make_task(story.id, sid, "T3", 4);
        insert_work_item(&conn, &t1).unwrap();
        insert_work_item(&conn, &t2).unwrap();
        insert_work_item(&conn, &t3).unwrap();

        // Update some statuses
        update_work_item_status(&conn, &t1.id, WorkItemStatus::Done).unwrap();
        update_work_item_status(&conn, &t2.id, WorkItemStatus::InProgress).unwrap();
        // t3 stays pending

        let counts = count_tasks_by_status(&conn, &story.id).unwrap();
        assert_eq!(counts.get(&WorkItemStatus::Done), Some(&1));
        assert_eq!(counts.get(&WorkItemStatus::InProgress), Some(&1));
        assert_eq!(counts.get(&WorkItemStatus::Pending), Some(&1));
        assert!(counts.get(&WorkItemStatus::Failed).is_none());
    }

    #[test]
    fn test_count_tasks_by_status_empty() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        let counts = count_tasks_by_status(&conn, &story.id).unwrap();
        assert!(counts.is_empty());
    }

    #[test]
    fn test_count_tasks_by_status_excludes_non_tasks() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        // Add a child story under the story (unusual but tests the filter)
        let child_story = make_story(story.id, sid, "S1.1", 0);
        insert_work_item(&conn, &child_story).unwrap();

        // Add a task
        let task = make_task(story.id, sid, "T1", 1);
        insert_work_item(&conn, &task).unwrap();

        let counts = count_tasks_by_status(&conn, &story.id).unwrap();
        // Only the task should be counted
        assert_eq!(counts.get(&WorkItemStatus::Pending), Some(&1));
        assert_eq!(counts.len(), 1);
    }

    // --- verify task kind roundtrip ---

    #[test]
    fn test_verify_task_roundtrip() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let story = make_story(epic.id, sid, "S1", 0);
        insert_work_item(&conn, &story).unwrap();

        let impl_task = make_task(story.id, sid, "T1", 0);
        insert_work_item(&conn, &impl_task).unwrap();

        let verify_tasks = nflow_core::work_item::auto_generate_verify_tasks(&[impl_task.clone()]);
        insert_work_item(&conn, &verify_tasks[0]).unwrap();

        let retrieved = get_work_item_by_id(&conn, &verify_tasks[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.kind, Some(TaskKind::Verify));
        assert_eq!(retrieved.short_id, "T1v");
        assert_eq!(retrieved.sort_order, 1);
    }

    // --- optional fields roundtrip ---

    #[test]
    fn test_optional_fields_roundtrip() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = insert_epic(&conn, sid);
        let mut story = make_story(epic.id, sid, "S1", 0);
        story.branch_name = Some("feature/s1".to_string());
        story.worktree_path = Some("/worktrees/s1".to_string());
        story.mr_url = Some("https://example.com/pr/1".to_string());
        story.commit_hash = Some("deadbeef".to_string());
        insert_work_item(&conn, &story).unwrap();

        let retrieved = get_work_item_by_id(&conn, &story.id).unwrap().unwrap();
        assert_eq!(retrieved.branch_name, Some("feature/s1".to_string()));
        assert_eq!(retrieved.worktree_path, Some("/worktrees/s1".to_string()));
        assert_eq!(
            retrieved.mr_url,
            Some("https://example.com/pr/1".to_string())
        );
        assert_eq!(retrieved.commit_hash, Some("deadbeef".to_string()));
    }

    // --- count pending stories ---

    #[test]
    fn test_count_pending_stories_by_project() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let epic = make_epic(sid);
        insert_work_item(&conn, &epic).unwrap();

        // No stories yet
        assert_eq!(count_pending_stories_by_project(&conn, &pid).unwrap(), 0);

        // Add a pending story
        let s1 = WorkItem::new_story(
            epic.id,
            sid,
            "Story 1".to_string(),
            "desc".to_string(),
            "ac".to_string(),
            "S1".to_string(),
            0,
        );
        insert_work_item(&conn, &s1).unwrap();
        assert_eq!(count_pending_stories_by_project(&conn, &pid).unwrap(), 1);

        // Add a ready story
        let mut s2 = WorkItem::new_story(
            epic.id,
            sid,
            "Story 2".to_string(),
            "desc".to_string(),
            "ac".to_string(),
            "S2".to_string(),
            1,
        );
        s2.status = WorkItemStatus::Ready;
        insert_work_item(&conn, &s2).unwrap();
        assert_eq!(count_pending_stories_by_project(&conn, &pid).unwrap(), 2);

        // Add an in_progress story — should NOT be counted
        let mut s3 = WorkItem::new_story(
            epic.id,
            sid,
            "Story 3".to_string(),
            "desc".to_string(),
            "ac".to_string(),
            "S3".to_string(),
            2,
        );
        s3.status = WorkItemStatus::InProgress;
        insert_work_item(&conn, &s3).unwrap();
        assert_eq!(count_pending_stories_by_project(&conn, &pid).unwrap(), 2);

        // Add a done story — should NOT be counted
        let mut s4 = WorkItem::new_story(
            epic.id,
            sid,
            "Story 4".to_string(),
            "desc".to_string(),
            "ac".to_string(),
            "S4".to_string(),
            3,
        );
        s4.status = WorkItemStatus::Done;
        insert_work_item(&conn, &s4).unwrap();
        assert_eq!(count_pending_stories_by_project(&conn, &pid).unwrap(), 2);
    }

    // --- parameterized queries (SQL injection resistance) ---

    #[test]
    fn test_parameterized_queries() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid);
        let mut epic = make_epic(sid);
        epic.title = "test'; DROP TABLE work_items; --".to_string();
        insert_work_item(&conn, &epic).unwrap();

        let retrieved = get_work_item_by_id(&conn, &epic.id).unwrap().unwrap();
        assert_eq!(retrieved.title, "test'; DROP TABLE work_items; --");

        // Table should still exist
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // --- parse_wave_prefix ---

    #[test]
    fn test_parse_wave_prefix_valid() {
        assert_eq!(parse_wave_prefix("W1-T1"), Some((1, "T1")));
        assert_eq!(parse_wave_prefix("W2-S3"), Some((2, "S3")));
        assert_eq!(parse_wave_prefix("W10-T1v"), Some((10, "T1v")));
        assert_eq!(parse_wave_prefix("W1-E1"), Some((1, "E1")));
    }

    #[test]
    fn test_parse_wave_prefix_invalid() {
        assert_eq!(parse_wave_prefix("T1"), None);
        assert_eq!(parse_wave_prefix("W-T1"), None);
        assert_eq!(parse_wave_prefix("W1-"), None);
        assert_eq!(parse_wave_prefix(""), None);
        assert_eq!(parse_wave_prefix("X1-T1"), None);
        assert_eq!(parse_wave_prefix("Wabc-T1"), None);
    }

    // --- find_work_item_by_wave_short_id ---

    #[test]
    fn test_find_work_item_by_wave_short_id_found() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid); // wave 1
        let epic = make_epic(sid);
        insert_work_item(&conn, &epic).unwrap();

        let task = make_task(epic.id, sid, "T1", 0);
        insert_work_item(&conn, &task).unwrap();

        let found = find_work_item_by_wave_short_id(&conn, "W1-T1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, task.id);
    }

    #[test]
    fn test_find_work_item_by_wave_short_id_not_found() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let _sid = make_session(&conn, pid);

        let found = find_work_item_by_wave_short_id(&conn, "W1-T99").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_find_work_item_by_wave_short_id_wrong_wave() {
        let conn = test_conn();
        let pid = make_project(&conn);
        let sid = make_session(&conn, pid); // wave 1
        let epic = make_epic(sid);
        insert_work_item(&conn, &epic).unwrap();

        let task = make_task(epic.id, sid, "T1", 0);
        insert_work_item(&conn, &task).unwrap();

        // Wrong wave number
        let found = find_work_item_by_wave_short_id(&conn, "W2-T1").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_find_work_item_by_wave_short_id_invalid_format() {
        let conn = test_conn();
        let found = find_work_item_by_wave_short_id(&conn, "invalid").unwrap();
        assert!(found.is_none());
    }
}
