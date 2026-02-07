use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Path already exists: {0}")]
    PathAlreadyExists(String),

    #[error("Not a git repository: {0}")]
    NotAGitRepository(String),

    #[error("Git command failed: {0}")]
    CommandFailed(String),

    #[error("Rebase conflict: {details}")]
    RebaseConflict { details: String },

    #[error("Push failed: authentication error: {0}")]
    AuthError(String),

    #[error("Push failed: network error: {0}")]
    NetworkError(String),

    #[error("Push failed: branch protection: {0}")]
    BranchProtection(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;
