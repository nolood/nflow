use thiserror::Error;

/// Top-level error type for the nflow TUI.
#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Core(#[from] nflow_core::error::NflowError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Socket error: {0}")]
    Socket(String),

    #[error("Terminal error: {0}")]
    Terminal(String),
}

pub type Result<T> = std::result::Result<T, TuiError>;
