use std::process::ExitCode;

use thiserror::Error;

/// Exit codes for the nflow CLI.
///
/// 0: success
/// 1: general error
/// 2: daemon not running
/// 3: entity not found
/// 4: invalid arguments
/// 5: invalid state transition
pub mod exit_code {
    use std::process::ExitCode;

    pub const SUCCESS: u8 = 0;
    pub const GENERAL_ERROR: u8 = 1;
    pub const DAEMON_NOT_RUNNING: u8 = 2;
    pub const NOT_FOUND: u8 = 3;
    pub const INVALID_ARGS: u8 = 4;
    pub const INVALID_STATE: u8 = 5;

    pub fn to_exit_code(code: u8) -> ExitCode {
        ExitCode::from(code)
    }
}

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

impl CliError {
    /// Map this error to a semantic exit code.
    pub fn exit_code(&self) -> ExitCode {
        exit_code::to_exit_code(self.exit_code_raw())
    }

    /// Return the raw exit code number for this error.
    pub fn exit_code_raw(&self) -> u8 {
        match self {
            CliError::Core(e) => core_error_exit_code(e),
            CliError::Socket(msg) => classify_error_message(msg),
            _ => exit_code::GENERAL_ERROR,
        }
    }
}

/// Map an NflowError variant to the appropriate exit code.
fn core_error_exit_code(e: &nflow_core::error::NflowError) -> u8 {
    use nflow_core::error::NflowError;
    match e {
        NflowError::NotFound(_) => exit_code::NOT_FOUND,
        NflowError::InvalidParams(_) | NflowError::ValidationError(_) => exit_code::INVALID_ARGS,
        NflowError::InvalidState(_) | NflowError::InvalidTransition { .. } => {
            exit_code::INVALID_STATE
        }
        NflowError::AlreadyExists(_) | NflowError::CyclicDependency { .. } => {
            exit_code::GENERAL_ERROR
        }
    }
}

/// Classify a daemon error message (which may contain prefixes) to an exit code.
fn classify_error_message(msg: &str) -> u8 {
    if msg.starts_with("NOT_FOUND:") {
        exit_code::NOT_FOUND
    } else if msg.starts_with("INVALID_PARAMS:") || msg.starts_with("ALREADY_EXISTS:") {
        exit_code::INVALID_ARGS
    } else if msg.starts_with("INVALID_STATE:") {
        exit_code::INVALID_STATE
    } else if msg.contains("daemon") && msg.contains("not running") {
        exit_code::DAEMON_NOT_RUNNING
    } else {
        exit_code::GENERAL_ERROR
    }
}

pub type Result<T> = std::result::Result<T, CliError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_not_found_exit_code() {
        let err = CliError::Socket("NOT_FOUND: spec does not exist".to_string());
        assert_eq!(err.exit_code_raw(), exit_code::NOT_FOUND);
    }

    #[test]
    fn test_socket_invalid_state_exit_code() {
        let err = CliError::Socket("INVALID_STATE: spec is already approved".to_string());
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_STATE);
    }

    #[test]
    fn test_socket_invalid_params_exit_code() {
        let err = CliError::Socket("INVALID_PARAMS: name is required".to_string());
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_ARGS);
    }

    #[test]
    fn test_socket_already_exists_exit_code() {
        let err = CliError::Socket("ALREADY_EXISTS: project already registered".to_string());
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_ARGS);
    }

    #[test]
    fn test_socket_daemon_not_running_exit_code() {
        let err = CliError::Socket("daemon is not running".to_string());
        assert_eq!(err.exit_code_raw(), exit_code::DAEMON_NOT_RUNNING);
    }

    #[test]
    fn test_socket_generic_error_exit_code() {
        let err = CliError::Socket("connection refused".to_string());
        assert_eq!(err.exit_code_raw(), exit_code::GENERAL_ERROR);
    }

    #[test]
    fn test_core_not_found_exit_code() {
        let err = CliError::Core(nflow_core::error::NflowError::NotFound(
            "project".to_string(),
        ));
        assert_eq!(err.exit_code_raw(), exit_code::NOT_FOUND);
    }

    #[test]
    fn test_core_invalid_params_exit_code() {
        let err = CliError::Core(nflow_core::error::NflowError::InvalidParams(
            "bad input".to_string(),
        ));
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_ARGS);
    }

    #[test]
    fn test_core_validation_error_exit_code() {
        let err = CliError::Core(nflow_core::error::NflowError::ValidationError(
            "name empty".to_string(),
        ));
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_ARGS);
    }

    #[test]
    fn test_core_invalid_state_exit_code() {
        let err = CliError::Core(nflow_core::error::NflowError::InvalidState(
            "wrong state".to_string(),
        ));
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_STATE);
    }

    #[test]
    fn test_core_invalid_transition_exit_code() {
        let err = CliError::Core(nflow_core::error::NflowError::InvalidTransition {
            from: "draft".to_string(),
            to: "decomposed".to_string(),
        });
        assert_eq!(err.exit_code_raw(), exit_code::INVALID_STATE);
    }

    #[test]
    fn test_io_error_exit_code() {
        let err = CliError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
        assert_eq!(err.exit_code_raw(), exit_code::GENERAL_ERROR);
    }
}
