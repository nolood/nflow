ALTER TABLE pipeline_runs ADD COLUMN mode TEXT NOT NULL DEFAULT 'auto';

CREATE TABLE IF NOT EXISTS pipeline_questions (
    id TEXT PRIMARY KEY,
    pipeline_run_id TEXT NOT NULL REFERENCES pipeline_runs(id),
    question TEXT NOT NULL,
    context TEXT,
    answered INTEGER NOT NULL DEFAULT 0,
    answer TEXT,
    answered_by TEXT,
    created_at TEXT NOT NULL,
    answered_at TEXT
);
