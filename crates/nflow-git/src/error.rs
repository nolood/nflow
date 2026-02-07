use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Path already exists: {0}")]
    PathAlreadyExists(String),

    #[error("Not a git repository: {0}")]
    NotAGitRepository(String),

    #[error("Git command failed: {0}")]
    CommandFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;
