use std::path::Path;

use rusqlite::Connection;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Migration error: {0}")]
    Migration(String),
    #[error("Ambiguous ID: {0}")]
    AmbiguousId(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// Embedded migration files, ordered by version number.
pub mod agent_runs;
pub mod decomposition_sessions;
pub mod pipeline;
pub mod projects;
pub mod spec_questions;
pub mod specs;
pub mod work_items;

const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../../migrations/001_init.sql")),
    (2, include_str!("../../migrations/002_pipeline.sql")),
    (3, include_str!("../../migrations/003_error_tracking.sql")),
    (4, include_str!("../../migrations/004_pipeline_mode.sql")),
    (5, include_str!("../../migrations/005_spec_questions.sql")),
];

/// Open a SQLite connection with WAL mode and foreign keys enabled.
pub fn open_connection(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Get the current schema version from the database.
/// Returns 0 if the schema_version table does not exist.
fn current_version(conn: &Connection) -> Result<u32> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(0);
    }
    let version: u32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    Ok(version)
}

/// Backup the database file before applying migrations.
fn backup_db(db_path: &Path, from_version: u32) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let backup_name = format!("{}.bak-v{}", db_path.display(), from_version);
    std::fs::copy(db_path, &backup_name)?;
    Ok(())
}

/// Run all pending migrations in a transaction.
/// Backs up the database before applying new migrations.
pub fn run_migrations(conn: &Connection, db_path: &Path) -> Result<u32> {
    let current = current_version(conn)?;
    let pending: Vec<_> = MIGRATIONS.iter().filter(|(v, _)| *v > current).collect();

    if pending.is_empty() {
        return Ok(current);
    }

    backup_db(db_path, current)?;

    let tx = conn.unchecked_transaction()?;
    for (version, sql) in &pending {
        tx.execute_batch(sql)
            .map_err(|e| DbError::Migration(format!("migration {} failed: {}", version, e)))?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
    }
    tx.commit()?;

    let new_version = pending.last().map(|(v, _)| *v).unwrap_or(current);
    Ok(new_version)
}

#[cfg(test)]
pub(crate) fn test_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let dummy_path = std::path::Path::new("/nonexistent/nflow.db");
    run_migrations(&conn, dummy_path).unwrap();
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    #[test]
    fn test_current_version_no_table() {
        let conn = in_memory_conn();
        assert_eq!(current_version(&conn).unwrap(), 0);
    }

    #[test]
    fn test_run_migrations_creates_all_tables() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        let version = run_migrations(&conn, dummy_path).unwrap();
        assert_eq!(version, 5);

        // Verify all tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();

        assert!(tables.contains(&"projects".to_string()));
        assert!(tables.contains(&"specs".to_string()));
        assert!(tables.contains(&"work_items".to_string()));
        assert!(tables.contains(&"dependencies".to_string()));
        assert!(tables.contains(&"agent_runs".to_string()));
        assert!(tables.contains(&"decomposition_sessions".to_string()));
        assert!(tables.contains(&"decomposition_specs".to_string()));
        assert!(tables.contains(&"pipeline_runs".to_string()));
        assert!(tables.contains(&"pipeline_stages".to_string()));
        assert!(tables.contains(&"pipeline_questions".to_string()));
        assert!(tables.contains(&"spec_questions".to_string()));
        assert!(tables.contains(&"schema_version".to_string()));
    }

    #[test]
    fn test_run_migrations_idempotent() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        let v1 = run_migrations(&conn, dummy_path).unwrap();
        let v2 = run_migrations(&conn, dummy_path).unwrap();
        assert_eq!(v1, 5);
        assert_eq!(v2, 5);
    }

    #[test]
    fn test_current_version_after_migration() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        run_migrations(&conn, dummy_path).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 5);
    }

    #[test]
    fn test_schema_version_tracking() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        run_migrations(&conn, dummy_path).unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 5);

        let max_version: u32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(max_version, 5);
    }

    #[test]
    fn test_indexes_created() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        run_migrations(&conn, dummy_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();

        assert!(indexes.contains(&"idx_specs_project_id".to_string()));
        assert!(indexes.contains(&"idx_work_items_parent_id".to_string()));
        assert!(indexes.contains(&"idx_work_items_session_id".to_string()));
        assert!(indexes.contains(&"idx_dependencies_blocker_id".to_string()));
        assert!(indexes.contains(&"idx_dependencies_blocked_id".to_string()));
    }

    #[test]
    fn test_foreign_keys_cascade() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        run_migrations(&conn, dummy_path).unwrap();

        // Insert a project
        conn.execute(
            "INSERT INTO projects (id, name, path, base_branch, git_provider, execution_enabled, created_at, updated_at) VALUES ('p1', 'test', '/tmp', 'main', 'github', 0, '2024-01-01', '2024-01-01')",
            [],
        ).unwrap();

        // Insert a spec
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at) VALUES ('s1', 'p1', 'spec1', '/tmp/spec.md', 'draft', 0, '2024-01-01', '2024-01-01')",
            [],
        ).unwrap();

        // Delete the project — spec should cascade
        conn.execute("DELETE FROM projects WHERE id = 'p1'", [])
            .unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM specs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_work_items_cascade_from_session() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        run_migrations(&conn, dummy_path).unwrap();

        // Insert project
        conn.execute(
            "INSERT INTO projects (id, name, path, created_at, updated_at) VALUES ('p1', 'test', '/tmp', '2024-01-01', '2024-01-01')",
            [],
        ).unwrap();

        // Insert decomposition session
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at) VALUES ('ds1', 'p1', 1, 'in_progress', '2024-01-01', '2024-01-01')",
            [],
        ).unwrap();

        // Insert work item
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, created_at, updated_at) VALUES ('w1', 'ds1', 'epic', 'Epic 1', '2024-01-01', '2024-01-01')",
            [],
        ).unwrap();

        // Delete session — work item should cascade
        conn.execute("DELETE FROM decomposition_sessions WHERE id = 'ds1'", [])
            .unwrap();

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_wal_mode_enabled() {
        let conn = in_memory_conn();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        // In-memory databases use "memory" for WAL, but the pragma was set
        assert!(mode == "wal" || mode == "memory");
    }

    #[test]
    fn test_open_connection_file() {
        let dir = std::env::temp_dir().join("nflow_test_db");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_open.db");

        // Clean up from previous runs
        let _ = std::fs::remove_file(&db_path);

        let conn = open_connection(&db_path).unwrap();
        let version = run_migrations(&conn, &db_path).unwrap();
        assert_eq!(version, 5);

        // Verify WAL mode
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");

        // Clean up
        drop(conn);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_backup_before_migration() {
        let dir = std::env::temp_dir().join("nflow_test_backup");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_backup.db");

        // Clean up from previous runs
        let _ = std::fs::remove_file(&db_path);
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|_| std::fs::read_dir(".").unwrap()) {
            if let Ok(e) = entry {
                if e.file_name().to_string_lossy().contains(".bak-v") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }

        // Create a db with some content
        let conn = open_connection(&db_path).unwrap();
        run_migrations(&conn, &db_path).unwrap();
        drop(conn);

        // The backup should exist (version 0 -> 1, backed up at v0)
        let backup_path = format!("{}.bak-v0", db_path.display());
        assert!(
            Path::new(&backup_path).exists(),
            "backup file should exist at {}",
            backup_path
        );

        // Clean up
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&backup_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_primary_keys_are_text() {
        let conn = in_memory_conn();
        let dummy_path = Path::new("/nonexistent/nflow.db");
        run_migrations(&conn, dummy_path).unwrap();

        // Verify TEXT primary keys by inserting UUID-format strings
        let uuid_str = "550e8400-e29b-41d4-a716-446655440000";
        conn.execute(
            "INSERT INTO projects (id, name, path, created_at, updated_at) VALUES (?1, 'test', '/tmp', '2024-01-01', '2024-01-01')",
            [uuid_str],
        ).unwrap();

        let retrieved: String = conn
            .query_row("SELECT id FROM projects WHERE id = ?1", [uuid_str], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retrieved, uuid_str);
    }
}
