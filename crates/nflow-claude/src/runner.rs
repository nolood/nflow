use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout};

use crate::error::ClaudeError;

/// Configuration for a Claude CLI process invocation.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The prompt text to send to Claude.
    pub prompt: String,
    /// Path to a system prompt file appended via `--append-system-prompt-file`.
    pub system_prompt_file: Option<PathBuf>,
    /// Allowed tools list (e.g., `["Read", "Write", "Edit", "Bash"]`).
    pub allowed_tools: Vec<String>,
    /// Maximum number of agentic turns.
    pub max_turns: Option<u32>,
    /// Resume an existing session by ID.
    pub resume_session: Option<String>,
    /// Working directory for the Claude process.
    pub working_dir: Option<PathBuf>,
    /// Always `stream-json`.
    pub output_format: OutputFormat,
    /// Enable verbose output.
    pub verbose: bool,
    /// Include partial messages in stream output.
    pub include_partial_messages: bool,
}

/// Output format for the Claude CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    StreamJson,
}

impl OutputFormat {
    fn as_arg(&self) -> &str {
        match self {
            OutputFormat::StreamJson => "stream-json",
        }
    }
}

/// A running Claude CLI process with access to stdout/stderr streams.
#[derive(Debug)]
pub struct ClaudeProcess {
    /// The child process handle.
    pub child: Child,
    /// OS process ID.
    pub pid: u32,
    /// Line-buffered reader for stdout (stream-json output).
    pub stdout: Lines<BufReader<ChildStdout>>,
    /// Line-buffered reader for stderr.
    pub stderr: Lines<BufReader<ChildStderr>>,
}

/// Spawns Claude CLI processes.
pub struct ClaudeRunner {
    /// Override path to the claude binary. Defaults to "claude" (resolved via PATH).
    pub claude_binary: String,
}

impl Default for ClaudeRunner {
    fn default() -> Self {
        Self {
            claude_binary: "claude".to_string(),
        }
    }
}

impl ClaudeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_binary(binary: String) -> Self {
        Self {
            claude_binary: binary,
        }
    }

    /// Spawn a Claude CLI process with the given configuration.
    ///
    /// Returns a `ClaudeProcess` with piped stdout/stderr for stream reading.
    pub fn spawn(&self, config: &RunConfig) -> Result<ClaudeProcess, ClaudeError> {
        let mut cmd = tokio::process::Command::new(&self.claude_binary);

        // Core: -p "<prompt>" for print/non-interactive mode
        cmd.arg("-p").arg(&config.prompt);

        // System prompt file
        if let Some(ref path) = config.system_prompt_file {
            cmd.arg("--append-system-prompt-file").arg(path);
        }

        // Allowed tools
        if !config.allowed_tools.is_empty() {
            cmd.arg("--allowedTools")
                .arg(config.allowed_tools.join(","));
        }

        // Max turns
        if let Some(max_turns) = config.max_turns {
            cmd.arg("--max-turns").arg(max_turns.to_string());
        }

        // Resume session
        if let Some(ref session_id) = config.resume_session {
            cmd.arg("--resume").arg(session_id);
        }

        // Output format
        cmd.arg("--output-format")
            .arg(config.output_format.as_arg());

        // Verbose
        if config.verbose {
            cmd.arg("--verbose");
        }

        // Include partial messages
        if config.include_partial_messages {
            cmd.arg("--include-partial-messages");
        }

        // Working directory
        if let Some(ref dir) = config.working_dir {
            cmd.current_dir(dir);
        }

        // Pipe stdout and stderr for streaming
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Prevent the child from inheriting stdin
        cmd.stdin(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClaudeError::BinaryNotFound {
                    binary: self.claude_binary.clone(),
                }
            } else {
                ClaudeError::SpawnFailed {
                    binary: self.claude_binary.clone(),
                    source: e,
                }
            }
        })?;

        let pid = child.id().ok_or(ClaudeError::ProcessExited)?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let stdout_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stdout));
        let stderr_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stderr));

        Ok(ClaudeProcess {
            child,
            pid,
            stdout: stdout_lines,
            stderr: stderr_lines,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> RunConfig {
        RunConfig {
            prompt: "Hello".to_string(),
            system_prompt_file: None,
            allowed_tools: vec![],
            max_turns: None,
            resume_session: None,
            working_dir: None,
            output_format: OutputFormat::StreamJson,
            verbose: true,
            include_partial_messages: true,
        }
    }

    #[tokio::test]
    async fn spawn_binary_not_found() {
        let runner = ClaudeRunner::with_binary("nonexistent-claude-binary-xyz".to_string());
        let result = runner.spawn(&default_config());
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ClaudeError::BinaryNotFound { ref binary } => {
                assert_eq!(binary, "nonexistent-claude-binary-xyz");
            }
            _ => panic!("expected BinaryNotFound, got: {err}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("nonexistent-claude-binary-xyz"));
        assert!(msg.contains("not found"));
    }

    #[tokio::test]
    async fn spawn_with_echo_returns_process() {
        // Use 'echo' as a stand-in for claude — validates piping and PID extraction
        let runner = ClaudeRunner::with_binary("echo".to_string());
        let config = default_config();
        let process = runner.spawn(&config).expect("echo should spawn");
        assert!(process.pid > 0);
    }

    #[tokio::test]
    async fn spawn_with_all_options() {
        let runner = ClaudeRunner::with_binary("echo".to_string());
        let config = RunConfig {
            prompt: "test prompt".to_string(),
            system_prompt_file: Some(PathBuf::from("/tmp/system.md")),
            allowed_tools: vec!["Read".to_string(), "Write".to_string()],
            max_turns: Some(50),
            resume_session: Some("session-abc".to_string()),
            working_dir: Some(PathBuf::from("/tmp")),
            output_format: OutputFormat::StreamJson,
            verbose: true,
            include_partial_messages: true,
        };
        let mut process = runner.spawn(&config).expect("echo should spawn");
        assert!(process.pid > 0);

        // Read the echo output to verify args were passed
        let line = process.stdout.next_line().await.unwrap();
        let output = line.unwrap_or_default();
        // echo prints all args space-separated
        assert!(output.contains("-p"));
        assert!(output.contains("test prompt"));
        assert!(output.contains("--append-system-prompt-file"));
        assert!(output.contains("--allowedTools"));
        assert!(output.contains("Read,Write"));
        assert!(output.contains("--max-turns"));
        assert!(output.contains("50"));
        assert!(output.contains("--resume"));
        assert!(output.contains("session-abc"));
        assert!(output.contains("--output-format"));
        assert!(output.contains("stream-json"));
        assert!(output.contains("--verbose"));
        assert!(output.contains("--include-partial-messages"));
    }

    #[tokio::test]
    async fn spawn_without_optional_args() {
        let runner = ClaudeRunner::with_binary("echo".to_string());
        let config = RunConfig {
            prompt: "minimal".to_string(),
            system_prompt_file: None,
            allowed_tools: vec![],
            max_turns: None,
            resume_session: None,
            working_dir: None,
            output_format: OutputFormat::StreamJson,
            verbose: false,
            include_partial_messages: false,
        };
        let mut process = runner.spawn(&config).expect("echo should spawn");

        let line = process.stdout.next_line().await.unwrap();
        let output = line.unwrap_or_default();
        // Should have -p and --output-format but not optional flags
        assert!(output.contains("-p"));
        assert!(output.contains("minimal"));
        assert!(output.contains("--output-format"));
        assert!(!output.contains("--verbose"));
        assert!(!output.contains("--include-partial-messages"));
        assert!(!output.contains("--append-system-prompt-file"));
        assert!(!output.contains("--allowedTools"));
        assert!(!output.contains("--max-turns"));
        assert!(!output.contains("--resume"));
    }

    #[test]
    fn output_format_stream_json() {
        assert_eq!(OutputFormat::StreamJson.as_arg(), "stream-json");
    }

    #[test]
    fn runner_default() {
        let runner = ClaudeRunner::new();
        assert_eq!(runner.claude_binary, "claude");
    }

    #[test]
    fn runner_with_binary() {
        let runner = ClaudeRunner::with_binary("/usr/local/bin/claude".to_string());
        assert_eq!(runner.claude_binary, "/usr/local/bin/claude");
    }

    #[test]
    fn run_config_clone() {
        let config = default_config();
        let cloned = config.clone();
        assert_eq!(cloned.prompt, "Hello");
        assert_eq!(cloned.verbose, true);
    }
}
