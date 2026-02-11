use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
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

impl RunConfig {
    /// Base config with common defaults: stream-json, verbose, include-partial-messages.
    fn base(prompt: String) -> Self {
        Self {
            prompt,
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

    /// Config for spec sessions (interactive Claude dialogue).
    ///
    /// - `with_codebase=true`: allowedTools = Read, Glob, Grep, Write, AskUserQuestion
    /// - `with_codebase=false`: allowedTools = Write, AskUserQuestion
    /// - No max_turns (multi-turn Q&A)
    pub fn for_spec(prompt: String, with_codebase: bool) -> Self {
        let allowed_tools = if with_codebase {
            vec![
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
                "Write".to_string(),
                "AskUserQuestion".to_string(),
            ]
        } else {
            vec!["Write".to_string(), "AskUserQuestion".to_string()]
        };
        Self {
            allowed_tools,
            ..Self::base(prompt)
        }
    }

    /// Config for decomposition sessions (Claude decomposes specs into DAG).
    ///
    /// - `with_codebase=true`: allowedTools = Read, Glob, Grep; working_dir = project_path
    /// - `with_codebase=false`: no allowedTools; working_dir = specs_dir
    /// - No max_turns
    pub fn for_decompose(prompt: String, with_codebase: bool, working_dir: PathBuf) -> Self {
        let allowed_tools = if with_codebase {
            vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()]
        } else {
            vec![]
        };
        Self {
            allowed_tools,
            working_dir: Some(working_dir),
            ..Self::base(prompt)
        }
    }

    /// Config for implementation tasks.
    ///
    /// allowedTools: Read, Write, Edit, Bash, Glob, Grep; max_turns: 50
    pub fn for_impl_task(prompt: String) -> Self {
        Self {
            allowed_tools: vec![
                "Read".to_string(),
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
            ],
            max_turns: Some(50),
            ..Self::base(prompt)
        }
    }

    /// Config for verify tasks.
    ///
    /// allowedTools: Read, Bash, Glob, Grep (NO Write/Edit); max_turns: 30
    pub fn for_verify_task(prompt: String) -> Self {
        Self {
            allowed_tools: vec![
                "Read".to_string(),
                "Bash".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
            ],
            max_turns: Some(30),
            ..Self::base(prompt)
        }
    }

    /// Config for pipeline plan stage.
    ///
    /// allowedTools: Read, Glob, Grep; max_turns: 30
    pub fn for_pipeline_plan(prompt: String) -> Self {
        Self {
            allowed_tools: vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()],
            max_turns: Some(30),
            ..Self::base(prompt)
        }
    }

    /// Config for pipeline implement stage.
    ///
    /// allowedTools: Read, Write, Edit, Bash, Glob, Grep; max_turns: 50
    pub fn for_pipeline_implement(prompt: String) -> Self {
        Self {
            allowed_tools: vec![
                "Read".to_string(),
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
            ],
            max_turns: Some(50),
            ..Self::base(prompt)
        }
    }

    /// Config for pipeline review stage.
    ///
    /// allowedTools: Read, Bash, Glob, Grep; max_turns: 30
    pub fn for_pipeline_review(prompt: String) -> Self {
        Self {
            allowed_tools: vec![
                "Read".to_string(),
                "Bash".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
            ],
            max_turns: Some(30),
            ..Self::base(prompt)
        }
    }
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

impl ClaudeProcess {
    /// Gracefully terminate the process: sends SIGTERM, waits 10 seconds, then SIGKILL if still alive.
    ///
    /// Returns the exit status and any final stderr output.
    pub async fn terminate(&mut self) -> Result<(ExitStatus, String), ClaudeError> {
        self.terminate_with_timeout(Duration::from_secs(10)).await
    }

    /// Gracefully terminate with a custom timeout before escalating to SIGKILL.
    pub async fn terminate_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<(ExitStatus, String), ClaudeError> {
        let pid = self.pid;

        // Send SIGTERM
        signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
            .map_err(|e| ClaudeError::SignalFailed { pid, source: e })?;

        // Wait for the process to exit within timeout
        match self.wait_with_timeout(timeout).await? {
            Some(status) => {
                let stderr = self.collect_stderr().await;
                Ok((status, stderr))
            }
            None => {
                // Timeout — escalate to SIGKILL
                self.kill_inner()?;
                // After SIGKILL, wait for the process to be reaped
                let status = self.child.wait().await.map_err(ClaudeError::Io)?;
                let stderr = self.collect_stderr().await;
                Ok((status, stderr))
            }
        }
    }

    /// Immediately kill the process with SIGKILL.
    ///
    /// Returns the exit status and any final stderr output.
    pub async fn kill(&mut self) -> Result<(ExitStatus, String), ClaudeError> {
        self.kill_inner()?;
        let status = self.child.wait().await.map_err(ClaudeError::Io)?;
        let stderr = self.collect_stderr().await;
        Ok((status, stderr))
    }

    /// Wait for the process to exit within the given duration.
    ///
    /// Returns `Some(ExitStatus)` if the process exits in time, `None` on timeout.
    pub async fn wait_with_timeout(
        &mut self,
        duration: Duration,
    ) -> Result<Option<ExitStatus>, ClaudeError> {
        match tokio::time::timeout(duration, self.child.wait()).await {
            Ok(result) => {
                let status = result.map_err(ClaudeError::Io)?;
                Ok(Some(status))
            }
            Err(_) => Ok(None),
        }
    }

    /// Send SIGKILL to the process.
    fn kill_inner(&self) -> Result<(), ClaudeError> {
        signal::kill(Pid::from_raw(self.pid as i32), Signal::SIGKILL).map_err(|e| {
            ClaudeError::SignalFailed {
                pid: self.pid,
                source: e,
            }
        })
    }

    /// Collect any remaining stderr output.
    async fn collect_stderr(&mut self) -> String {
        let mut output = String::new();
        while let Ok(Some(line)) = self.stderr.next_line().await {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&line);
        }
        output
    }
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

    // --- Invocation mode tests ---

    #[test]
    fn for_spec_with_codebase() {
        let config = RunConfig::for_spec("spec prompt".into(), true);
        assert_eq!(config.prompt, "spec prompt");
        assert_eq!(
            config.allowed_tools,
            vec!["Read", "Glob", "Grep", "Write", "AskUserQuestion"]
        );
        assert_eq!(config.max_turns, None);
        assert_eq!(config.output_format, OutputFormat::StreamJson);
        assert!(config.verbose);
        assert!(config.include_partial_messages);
        assert!(config.working_dir.is_none());
    }

    #[test]
    fn for_spec_without_codebase() {
        let config = RunConfig::for_spec("spec prompt".into(), false);
        assert_eq!(config.prompt, "spec prompt");
        assert_eq!(config.allowed_tools, vec!["Write", "AskUserQuestion"]);
        assert_eq!(config.max_turns, None);
        assert!(config.verbose);
        assert!(config.include_partial_messages);
    }

    #[test]
    fn for_decompose_with_codebase() {
        let config =
            RunConfig::for_decompose("decompose".into(), true, PathBuf::from("/project/root"));
        assert_eq!(config.prompt, "decompose");
        assert_eq!(config.allowed_tools, vec!["Read", "Glob", "Grep"]);
        assert_eq!(config.max_turns, None);
        assert_eq!(config.working_dir, Some(PathBuf::from("/project/root")));
        assert!(config.verbose);
        assert!(config.include_partial_messages);
    }

    #[test]
    fn for_decompose_without_codebase() {
        let config =
            RunConfig::for_decompose("decompose".into(), false, PathBuf::from("/specs/dir"));
        assert_eq!(config.prompt, "decompose");
        assert!(config.allowed_tools.is_empty());
        assert_eq!(config.max_turns, None);
        assert_eq!(config.working_dir, Some(PathBuf::from("/specs/dir")));
        assert!(config.verbose);
        assert!(config.include_partial_messages);
    }

    #[test]
    fn for_impl_task_tools_and_limits() {
        let config = RunConfig::for_impl_task("implement feature X".into());
        assert_eq!(config.prompt, "implement feature X");
        assert_eq!(
            config.allowed_tools,
            vec!["Read", "Write", "Edit", "Bash", "Glob", "Grep"]
        );
        assert_eq!(config.max_turns, Some(50));
        assert!(config.verbose);
        assert!(config.include_partial_messages);
    }

    #[test]
    fn for_verify_task_tools_and_limits() {
        let config = RunConfig::for_verify_task("verify feature X".into());
        assert_eq!(config.prompt, "verify feature X");
        assert_eq!(config.allowed_tools, vec!["Read", "Bash", "Glob", "Grep"]);
        assert_eq!(config.max_turns, Some(30));
        assert!(config.verbose);
        assert!(config.include_partial_messages);
    }

    #[test]
    fn for_verify_task_no_write_or_edit() {
        let config = RunConfig::for_verify_task("verify".into());
        assert!(!config.allowed_tools.contains(&"Write".to_string()));
        assert!(!config.allowed_tools.contains(&"Edit".to_string()));
    }

    #[test]
    fn all_modes_use_stream_json() {
        let spec = RunConfig::for_spec("p".into(), true);
        let decomp = RunConfig::for_decompose("p".into(), false, PathBuf::from("/tmp"));
        let impl_t = RunConfig::for_impl_task("p".into());
        let verify = RunConfig::for_verify_task("p".into());

        for config in [&spec, &decomp, &impl_t, &verify] {
            assert_eq!(config.output_format, OutputFormat::StreamJson);
            assert!(config.verbose);
            assert!(config.include_partial_messages);
        }
    }

    #[tokio::test]
    async fn spawn_for_impl_task_passes_correct_args() {
        let runner = ClaudeRunner::with_binary("echo".to_string());
        let config = RunConfig::for_impl_task("implement it".into());
        let mut process = runner.spawn(&config).expect("echo should spawn");

        let line = process.stdout.next_line().await.unwrap();
        let output = line.unwrap_or_default();
        assert!(output.contains("-p"));
        assert!(output.contains("implement it"));
        assert!(output.contains("--allowedTools"));
        assert!(output.contains("Read,Write,Edit,Bash,Glob,Grep"));
        assert!(output.contains("--max-turns"));
        assert!(output.contains("50"));
        assert!(output.contains("--output-format"));
        assert!(output.contains("stream-json"));
        assert!(output.contains("--verbose"));
        assert!(output.contains("--include-partial-messages"));
    }

    #[tokio::test]
    async fn spawn_for_verify_task_passes_correct_args() {
        let runner = ClaudeRunner::with_binary("echo".to_string());
        let config = RunConfig::for_verify_task("verify it".into());
        let mut process = runner.spawn(&config).expect("echo should spawn");

        let line = process.stdout.next_line().await.unwrap();
        let output = line.unwrap_or_default();
        assert!(output.contains("--allowedTools"));
        assert!(output.contains("Read,Bash,Glob,Grep"));
        assert!(output.contains("--max-turns"));
        assert!(output.contains("30"));
        assert!(!output.contains("Write"));
        assert!(!output.contains("Edit"));
    }

    // --- Termination tests ---

    /// Helper to spawn a long-running sleep process directly (bypassing ClaudeRunner
    /// which adds args that `sleep` doesn't understand).
    fn spawn_sleep_process(seconds: u32) -> ClaudeProcess {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg(seconds.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd.spawn().expect("sleep should spawn");
        let pid = child.id().expect("should have pid");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdout_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stdout));
        let stderr_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stderr));

        ClaudeProcess {
            child,
            pid,
            stdout: stdout_lines,
            stderr: stderr_lines,
        }
    }

    #[tokio::test]
    async fn terminate_sends_sigterm_then_exits() {
        let mut process = spawn_sleep_process(60);
        assert!(process.pid > 0);

        let (status, _stderr) = process.terminate().await.expect("terminate should succeed");
        // Process should have been terminated by SIGTERM (signal 15)
        assert!(!status.success());
    }

    #[tokio::test]
    async fn kill_sends_sigkill() {
        let mut process = spawn_sleep_process(60);
        assert!(process.pid > 0);

        let (status, _stderr) = process.kill().await.expect("kill should succeed");
        // Process should have been killed
        assert!(!status.success());
    }

    #[tokio::test]
    async fn wait_with_timeout_returns_some_on_fast_exit() {
        // echo exits immediately
        let runner = ClaudeRunner::with_binary("echo".to_string());
        let config = default_config();
        let mut process = runner.spawn(&config).expect("echo should spawn");

        let result = process
            .wait_with_timeout(Duration::from_secs(5))
            .await
            .expect("wait should not error");
        assert!(result.is_some());
        assert!(result.unwrap().success());
    }

    #[tokio::test]
    async fn wait_with_timeout_returns_none_on_timeout() {
        let mut process = spawn_sleep_process(60);

        let result = process
            .wait_with_timeout(Duration::from_millis(100))
            .await
            .expect("wait should not error");
        assert!(result.is_none());

        // Clean up the process
        process.kill().await.expect("cleanup kill should succeed");
    }

    #[tokio::test]
    async fn terminate_with_custom_timeout() {
        let mut process = spawn_sleep_process(60);

        let (status, _stderr) = process
            .terminate_with_timeout(Duration::from_secs(5))
            .await
            .expect("terminate should succeed");
        assert!(!status.success());
    }

    #[tokio::test]
    async fn kill_captures_stderr_output() {
        // Use bash -c to write to stderr before sleeping
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg("echo 'test error output' >&2 && sleep 60")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd.spawn().expect("bash should spawn");
        let pid = child.id().expect("should have pid");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdout_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stdout));
        let stderr_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stderr));

        let mut process = ClaudeProcess {
            child,
            pid,
            stdout: stdout_lines,
            stderr: stderr_lines,
        };

        // Give the process a moment to write to stderr
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (status, stderr_output) = process.kill().await.expect("kill should succeed");
        assert!(!status.success());
        assert!(
            stderr_output.contains("test error output"),
            "stderr should be captured, got: {stderr_output}"
        );
    }

    #[tokio::test]
    async fn terminate_escalates_to_sigkill_on_trap() {
        // Use a process that traps SIGTERM and ignores it
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg("trap '' TERM; sleep 60")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd.spawn().expect("bash should spawn");
        let pid = child.id().expect("should have pid");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdout_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stdout));
        let stderr_lines = tokio::io::AsyncBufReadExt::lines(BufReader::new(stderr));

        let mut process = ClaudeProcess {
            child,
            pid,
            stdout: stdout_lines,
            stderr: stderr_lines,
        };

        // Give the process a moment to set up the trap
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Use a short timeout so SIGTERM will time out and escalate to SIGKILL
        let (status, _stderr) = process
            .terminate_with_timeout(Duration::from_millis(500))
            .await
            .expect("terminate should succeed via SIGKILL escalation");
        assert!(!status.success());
    }
}
