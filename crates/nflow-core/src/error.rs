use thiserror::Error;

#[derive(Debug, Error)]
pub enum NflowError {
    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Invalid state: {0}")]
    InvalidState(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, NflowError>;
