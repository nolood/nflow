-- nflow initial schema

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL,
    base_branch TEXT NOT NULL DEFAULT 'main',
    git_provider TEXT NOT NULL DEFAULT 'github',
    execution_enabled INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS specs (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    file_path TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    session_active INTEGER NOT NULL DEFAULT 0,
    claude_session_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_specs_project_id ON specs(project_id);

CREATE TABLE IF NOT EXISTS decomposition_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    wave_number INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'in_progress',
    claude_session_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS decomposition_specs (
    session_id TEXT NOT NULL REFERENCES decomposition_sessions(id) ON DELETE CASCADE,
    spec_id TEXT NOT NULL REFERENCES specs(id) ON DELETE CASCADE,
    PRIMARY KEY (session_id, spec_id)
);

CREATE TABLE IF NOT EXISTS work_items (
    id TEXT PRIMARY KEY NOT NULL,
    parent_id TEXT REFERENCES work_items(id) ON DELETE CASCADE,
    decomposition_session_id TEXT NOT NULL REFERENCES decomposition_sessions(id) ON DELETE CASCADE,
    item_type TEXT NOT NULL,
    kind TEXT,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    acceptance_criteria TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    short_id TEXT NOT NULL DEFAULT '',
    sort_order INTEGER NOT NULL DEFAULT 0,
    branch_name TEXT,
    worktree_path TEXT,
    mr_url TEXT,
    commit_hash TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_work_items_parent_id ON work_items(parent_id);
CREATE INDEX IF NOT EXISTS idx_work_items_session_id ON work_items(decomposition_session_id);

CREATE TABLE IF NOT EXISTS dependencies (
    blocker_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    blocked_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    PRIMARY KEY (blocker_id, blocked_id)
);

CREATE INDEX IF NOT EXISTS idx_dependencies_blocker_id ON dependencies(blocker_id);
CREATE INDEX IF NOT EXISTS idx_dependencies_blocked_id ON dependencies(blocked_id);

CREATE TABLE IF NOT EXISTS agent_runs (
    id TEXT PRIMARY KEY NOT NULL,
    work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    pid INTEGER,
    session_id TEXT,
    pid_start_time INTEGER,
    status TEXT NOT NULL DEFAULT 'running',
    exit_code INTEGER,
    log_path TEXT,
    error_message TEXT,
    started_at TEXT NOT NULL,
    finished_at TEXT
);
