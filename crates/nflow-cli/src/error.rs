use thiserror::Error;

/// Top-level error type for the nflow CLI.
///
/// Wraps all IO/system errors that the CLI client can encounter.
/// NflowError from nflow-core is re-exported for business logic errors.
#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Core(#[from] nflow_core::error::NflowError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Db(String),

    #[error("Git error: {0}")]
    Git(String),

    #[error("Claude error: {0}")]
    Claude(String),

    #[error("Socket error: {0}")]
    Socket(String),
}

pub type Result<T> = std::result::Result<T, CliError>;
