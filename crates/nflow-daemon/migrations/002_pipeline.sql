CREATE TABLE IF NOT EXISTS pipeline_runs (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    goal TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    current_stage TEXT,
    iteration INTEGER NOT NULL DEFAULT 0,
    max_iterations INTEGER NOT NULL DEFAULT 5,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pipeline_runs_project_id ON pipeline_runs(project_id);

CREATE TABLE IF NOT EXISTS pipeline_stages (
    id TEXT PRIMARY KEY NOT NULL,
    pipeline_run_id TEXT NOT NULL REFERENCES pipeline_runs(id) ON DELETE CASCADE,
    stage_type TEXT NOT NULL,
    iteration INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    input_context TEXT,
    output_result TEXT,
    agent_run_id TEXT,
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pipeline_stages_run_id ON pipeline_stages(pipeline_run_id);
