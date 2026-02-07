use thiserror::Error;

#[derive(Debug, Error)]
pub enum NflowError {
    #[error("Invalid transition from {from} to {to}")]
    InvalidTransition { from: String, to: String },

    #[error("Cyclic dependency detected: {cycle:?}")]
    CyclicDependency { cycle: Vec<uuid::Uuid> },

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Already exists: {0}")]
    AlreadyExists(String),

    #[error("Invalid state: {0}")]
    InvalidState(String),

    #[error("Invalid params: {0}")]
    InvalidParams(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

pub type Result<T> = std::result::Result<T, NflowError>;
