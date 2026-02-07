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

    #[error("Invalid transition from {from} to {to}")]
    InvalidTransition { from: String, to: String },

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Cyclic dependency detected: {cycle:?}")]
    CyclicDependency { cycle: Vec<uuid::Uuid> },
}

pub type Result<T> = std::result::Result<T, NflowError>;
