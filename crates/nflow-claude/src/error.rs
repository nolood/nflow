use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error("Claude binary '{binary}' not found in PATH. Install Claude Code CLI or set the correct path.")]
    BinaryNotFound { binary: String },

    #[error("Failed to spawn Claude process '{binary}': {source}")]
    SpawnFailed {
        binary: String,
        source: std::io::Error,
    },

    #[error("Claude process exited before PID could be read")]
    ProcessExited,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
