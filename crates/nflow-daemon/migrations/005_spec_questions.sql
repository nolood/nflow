CREATE TABLE IF NOT EXISTS spec_questions (
    id TEXT PRIMARY KEY,
    spec_id TEXT NOT NULL REFERENCES specs(id) ON DELETE CASCADE,
    question TEXT NOT NULL,
    options TEXT,
    answered INTEGER NOT NULL DEFAULT 0,
    answer TEXT,
    created_at TEXT NOT NULL,
    answered_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_spec_questions_spec_id ON spec_questions(spec_id);
