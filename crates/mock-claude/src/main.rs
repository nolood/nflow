//! Mock Claude binary for E2E testing.
//!
//! Mimics the `claude` CLI tool by reading command-line arguments,
//! detecting the mode from the prompt content, and outputting
//! appropriate stream-json responses to stdout.
//!
//! # Modes
//!
//! - **Spec**: Detected when prompt contains "specification" or system prompt file
//!   contains "spec_session". Outputs AskUserQuestion events and writes a spec file.
//! - **Decompose**: Detected when prompt contains decomposition-related keywords
//!   or system prompt file contains "decompose". Outputs a valid JSON DAG.
//! - **Impl task**: Detected when prompt contains "[T" or "Implement the task".
//!   Creates a git commit with the correct `[short_id]` tag.
//! - **Verify task**: Detected when prompt mentions "verify" or system prompt file
//!   contains "verify_task". Outputs "VERIFICATION PASSED" in the result.
//!
//! # Environment Variables
//!
//! - `MOCK_CLAUDE_FAIL`: If set to "1", exit with code 1
//! - `MOCK_CLAUDE_NO_COMMIT`: If set to "1", skip creating the git commit (impl mode)
//! - `MOCK_CLAUDE_WRONG_MESSAGE`: If set to "1", use a commit message without the task ID tag
//! - `MOCK_CLAUDE_EXIT_CODE`: Override exit code (default 0)
//! - `MOCK_CLAUDE_VERIFY_FAIL`: If set to "1", output "VERIFICATION FAILED" instead
//! - `MOCK_CLAUDE_SPEC_COMPLETE`: If set to "1", write the spec file (completing the session)
//! - `MOCK_CLAUDE_SESSION_ID`: Override session ID (default: auto-generated)
//! - `MOCK_CLAUDE_DECOMPOSE_JSON`: Override decomposition JSON output
//! - `MOCK_CLAUDE_SLEEP`: Sleep for N seconds before producing output (for timeout testing)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let parsed = parse_args(&args);

    // Check for failure simulation
    if env::var("MOCK_CLAUDE_FAIL").as_deref() == Ok("1") {
        // Output an error event and exit with non-zero
        emit_text_delta("Error: simulated failure");
        emit_error("Simulated failure");
        let code: i32 = env::var("MOCK_CLAUDE_EXIT_CODE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        process::exit(code);
    }

    // Sleep if requested (for timeout testing)
    if let Ok(secs) = env::var("MOCK_CLAUDE_SLEEP") {
        if let Ok(duration) = secs.parse::<u64>() {
            emit_text_delta(&format!(
                "Sleeping for {}s (mock timeout test)...",
                duration
            ));
            std::thread::sleep(std::time::Duration::from_secs(duration));
        }
    }

    let session_id = env::var("MOCK_CLAUDE_SESSION_ID")
        .unwrap_or_else(|_| format!("mock-session-{}", uuid::Uuid::new_v4()));

    // Detect mode from prompt and system prompt file
    let mode = detect_mode(&parsed);

    match mode {
        Mode::Spec => handle_spec(&parsed, &session_id),
        Mode::Decompose => handle_decompose(&parsed, &session_id),
        Mode::ImplTask => handle_impl_task(&parsed, &session_id),
        Mode::VerifyTask => handle_verify_task(&parsed, &session_id),
        Mode::Unknown => handle_unknown(&parsed, &session_id),
    }

    let exit_code: i32 = env::var("MOCK_CLAUDE_EXIT_CODE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    process::exit(exit_code);
}

#[derive(Debug)]
enum Mode {
    Spec,
    Decompose,
    ImplTask,
    VerifyTask,
    Unknown,
}

#[derive(Debug, Default)]
struct ParsedArgs {
    prompt: String,
    system_prompt_file: Option<PathBuf>,
    resume_session: Option<String>,
    working_dir: Option<PathBuf>,
    max_turns: Option<u32>,
    allowed_tools: Vec<String>,
}

fn parse_args(args: &[String]) -> ParsedArgs {
    let mut parsed = ParsedArgs::default();
    let mut i = 1; // skip binary name
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                if i + 1 < args.len() {
                    parsed.prompt = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--append-system-prompt-file" => {
                if i + 1 < args.len() {
                    parsed.system_prompt_file = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--resume" => {
                if i + 1 < args.len() {
                    parsed.resume_session = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--cwd" => {
                if i + 1 < args.len() {
                    parsed.working_dir = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--max-turns" => {
                if i + 1 < args.len() {
                    parsed.max_turns = args[i + 1].parse().ok();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--allowedTools" => {
                if i + 1 < args.len() {
                    parsed.allowed_tools = args[i + 1].split(',').map(|s| s.to_string()).collect();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            // Skip other flags
            "--output-format" | "--verbose" | "--include-partial-messages" => {
                // --verbose and --include-partial-messages are standalone flags
                if args[i] == "--output-format" && i + 1 < args.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    parsed
}

fn detect_mode(parsed: &ParsedArgs) -> Mode {
    let prompt_lower = parsed.prompt.to_lowercase();

    // Check system prompt file name for mode hints
    let spf_name = parsed
        .system_prompt_file
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Also try reading the system prompt file content for more hints
    let spf_content = parsed
        .system_prompt_file
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default()
        .to_lowercase();

    // Verify task: check first because verify prompts also contain "task"
    if spf_name.contains("verify_task")
        || spf_content.contains("verification passed")
        || (prompt_lower.contains("verify") && prompt_lower.contains("task"))
    {
        return Mode::VerifyTask;
    }

    // Impl task: prompt contains task ID pattern or "implement the task"
    if spf_name.contains("task_execution")
        || prompt_lower.contains("implement the task")
        || (prompt_lower.contains("[t") && prompt_lower.contains("] "))
    {
        return Mode::ImplTask;
    }

    // Decompose: system prompt or prompt content
    if spf_name.contains("decompose")
        || spf_content.contains("break down")
        || spf_content.contains("epics")
        || prompt_lower.contains("decompos")
    {
        return Mode::Decompose;
    }

    // Spec: system prompt or prompt content
    if spf_name.contains("spec_session")
        || spf_content.contains("specification")
        || prompt_lower.contains("specification")
        || prompt_lower.contains("write a spec")
    {
        return Mode::Spec;
    }

    Mode::Unknown
}

// ---------------------------------------------------------------------------
// Mode handlers
// ---------------------------------------------------------------------------

fn handle_spec(parsed: &ParsedArgs, session_id: &str) {
    let is_resume = parsed.resume_session.is_some();
    let should_complete = is_resume || env::var("MOCK_CLAUDE_SPEC_COMPLETE").as_deref() == Ok("1");

    if should_complete {
        // On resume (answering a question), complete the spec by writing the file
        emit_text_delta("Thank you for your answers. Writing the specification...");

        // Find the spec file path from the prompt or system prompt
        if let Some(spec_path) = find_spec_file_path(parsed) {
            // Ensure parent dir exists
            if let Some(parent) = spec_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let spec_content = generate_spec_content(parsed);
            let _ = fs::write(&spec_path, spec_content);

            // Emit a Write tool use (simulating Claude writing the file)
            emit_tool_use(
                "Write",
                &serde_json::json!({
                    "file_path": spec_path.to_string_lossy(),
                    "content": "# Specification\n\nGenerated by mock Claude."
                }),
            );
            emit_tool_result(&format!("File written to {}", spec_path.to_string_lossy()));
        }

        emit_result("Specification has been written successfully.", session_id);
    } else {
        // First invocation: ask a question
        emit_text_delta("I'll help you write the specification. Let me ask some questions...");

        emit_tool_use(
            "AskUserQuestion",
            &serde_json::json!({
                "question": "What is the main purpose of this feature?",
                "options": ["Option A", "Option B", "Option C"]
            }),
        );
        emit_tool_result("User will provide answer.");

        emit_result("Waiting for user input.", session_id);
    }
}

fn handle_decompose(parsed: &ParsedArgs, session_id: &str) {
    emit_text_delta("Analyzing specs and decomposing into epics, stories, and tasks...");

    // Check for custom decomposition JSON from env var
    let decompose_json = if let Ok(custom) = env::var("MOCK_CLAUDE_DECOMPOSE_JSON") {
        custom
    } else {
        default_decomposition_json()
    };

    // The result must be wrapped in a markdown code fence because the handler
    // extracts JSON from ```json ... ``` blocks
    let result_text = format!(
        "Here is the decomposition:\n\n```json\n{}\n```",
        decompose_json
    );

    // If this is a resume (feedback), acknowledge it
    if parsed.resume_session.is_some() {
        emit_text_delta("I've reviewed your feedback and updated the decomposition...");
    }

    emit_result(&result_text, session_id);
}

fn handle_impl_task(parsed: &ParsedArgs, session_id: &str) {
    emit_text_delta("Implementing the task...");

    // Extract task short ID from prompt (e.g., "[T1]" or "[W1-T1]")
    let short_id = extract_short_id(&parsed.prompt).unwrap_or_else(|| "T1".to_string());

    let no_commit = env::var("MOCK_CLAUDE_NO_COMMIT").as_deref() == Ok("1");
    let wrong_message = env::var("MOCK_CLAUDE_WRONG_MESSAGE").as_deref() == Ok("1");

    if !no_commit {
        // Determine working directory
        let cwd = parsed
            .working_dir
            .clone()
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Create a dummy file and commit it
        let dummy_file = cwd.join(format!("{}.txt", short_id.to_lowercase().replace('-', "_")));
        let _ = fs::write(&dummy_file, format!("Implementation for {}\n", short_id));

        // Stage and commit using git
        let commit_msg = if wrong_message {
            format!("feat: implement {}", short_id) // Missing brackets
        } else {
            format!("feat: [{}] implement feature", short_id)
        };

        let git_add = process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&cwd)
            .output();

        if let Ok(output) = git_add {
            if output.status.success() {
                let git_commit = process::Command::new("git")
                    .args(["commit", "-m", &commit_msg, "--allow-empty"])
                    .current_dir(&cwd)
                    .output();

                if let Ok(commit_output) = git_commit {
                    let stdout = String::from_utf8_lossy(&commit_output.stdout);
                    // Emit the git output as a tool result so parse_log_file can extract commit info
                    emit_tool_use(
                        "Bash",
                        &serde_json::json!({"command": format!("git commit -m \"{}\"", commit_msg)}),
                    );
                    emit_tool_result(&stdout);
                }
            }
        }
    }

    emit_text_delta("Implementation complete.");
    emit_result(
        &format!("Task [{}] has been implemented successfully.", short_id),
        session_id,
    );
}

fn handle_verify_task(_parsed: &ParsedArgs, session_id: &str) {
    emit_text_delta("Verifying the implementation...");

    let verify_fail = env::var("MOCK_CLAUDE_VERIFY_FAIL").as_deref() == Ok("1");

    if verify_fail {
        emit_result("VERIFICATION FAILED: Tests do not pass.", session_id);
    } else {
        // Simulate running some verification commands
        emit_tool_use("Bash", &serde_json::json!({"command": "cargo test"}));
        emit_tool_result("All tests passed.");

        emit_result("VERIFICATION PASSED", session_id);
    }
}

fn handle_unknown(parsed: &ParsedArgs, session_id: &str) {
    // Fallback: just echo the prompt back
    emit_text_delta(&format!("Processing: {}", parsed.prompt));
    emit_result("Done.", session_id);
}

// ---------------------------------------------------------------------------
// Stream-JSON output helpers
// ---------------------------------------------------------------------------

fn emit_text_delta(text: &str) {
    let event = serde_json::json!({
        "type": "stream_event",
        "event": {
            "delta": {
                "type": "text_delta",
                "text": text
            }
        }
    });
    println!("{}", event);
}

fn emit_tool_use(name: &str, input: &serde_json::Value) {
    let event = serde_json::json!({
        "type": "tool_use",
        "name": name,
        "input": input
    });
    println!("{}", event);
}

fn emit_tool_result(content: &str) {
    let event = serde_json::json!({
        "type": "tool_result",
        "content": content
    });
    println!("{}", event);
}

fn emit_result(text: &str, session_id: &str) {
    let event = serde_json::json!({
        "type": "result",
        "result": text,
        "session_id": session_id
    });
    println!("{}", event);
}

fn emit_error(message: &str) {
    let event = serde_json::json!({
        "type": "error",
        "error": message
    });
    println!("{}", event);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the task short ID from a prompt string.
///
/// Looks for patterns like `[T1]`, `[W1-T1]`, `[W2-T3]` in the prompt.
fn extract_short_id(prompt: &str) -> Option<String> {
    let mut chars = prompt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '[' {
            let mut id = String::new();
            for inner in chars.by_ref() {
                if inner == ']' {
                    break;
                }
                id.push(inner);
            }
            // Validate it looks like a short ID: W?d*-?[EST]d+ or bare [EST]d+
            if is_valid_short_id(&id) {
                return Some(id);
            }
        }
    }
    None
}

fn is_valid_short_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    // Match patterns: E1, S1, T1, T1v, W1-E1, W1-S1, W1-T1, W1-T1v
    let check = if let Some(rest) = id.strip_prefix('W') {
        // Wave-prefixed: W{n}-{id}
        if let Some(dash_pos) = rest.find('-') {
            let wave_num = &rest[..dash_pos];
            let bare_id = &rest[dash_pos + 1..];
            !wave_num.is_empty()
                && wave_num.chars().all(|c| c.is_ascii_digit())
                && is_bare_short_id(bare_id)
        } else {
            false
        }
    } else {
        is_bare_short_id(id)
    };
    check
}

fn is_bare_short_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let first = id.chars().next().unwrap();
    if !matches!(first, 'E' | 'S' | 'T') {
        return false;
    }
    let rest = &id[1..];
    let rest = rest.strip_suffix('v').unwrap_or(rest);
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// Try to find the spec file path from the parsed arguments.
///
/// The spec file path is typically embedded in the prompt or can be derived
/// from the working directory and spec name.
fn find_spec_file_path(parsed: &ParsedArgs) -> Option<PathBuf> {
    // Check env var override first (for testing)
    if let Ok(path) = env::var("MOCK_CLAUDE_SPEC_FILE") {
        return Some(PathBuf::from(path));
    }

    // Look for a file path in the prompt (e.g., "spec file at /path/to/spec.md")
    for word in parsed.prompt.split_whitespace() {
        if word.ends_with(".md") && (word.contains('/') || word.contains('\\')) {
            let path = Path::new(word.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '/' && c != '.' && c != '-' && c != '_'
            }));
            return Some(path.to_path_buf());
        }
    }

    // Check if the system prompt file content references a path
    if let Some(ref spf) = parsed.system_prompt_file {
        if let Ok(content) = fs::read_to_string(spf) {
            for line in content.lines() {
                if line.contains("file_path") || line.contains("spec_file") {
                    for word in line.split_whitespace() {
                        if word.ends_with(".md") && word.contains('/') {
                            return Some(PathBuf::from(word.trim_matches(|c: char| {
                                !c.is_alphanumeric() && c != '/' && c != '.' && c != '-' && c != '_'
                            })));
                        }
                    }
                }
            }
        }
    }

    // Fallback: derive from working dir if available
    if let Some(ref cwd) = parsed.working_dir {
        return Some(cwd.join("spec.md"));
    }

    None
}

fn generate_spec_content(_parsed: &ParsedArgs) -> String {
    "# Specification\n\n\
     ## Overview\n\n\
     This is a generated specification from the mock Claude binary.\n\n\
     ## Requirements\n\n\
     - Requirement 1: The system shall work correctly\n\
     - Requirement 2: The system shall be tested\n\n\
     ## Acceptance Criteria\n\n\
     - All tests pass\n\
     - Code compiles without warnings\n"
        .to_string()
}

fn default_decomposition_json() -> String {
    serde_json::json!({
        "epics": [{
            "id": "E1",
            "title": "Core Implementation",
            "description": "Implement the core feature",
            "stories": [{
                "id": "S1",
                "title": "Implement main logic",
                "description": "Write the main logic for the feature",
                "acceptance_criteria": "Logic works correctly",
                "depends_on": [],
                "tasks": [{
                    "id": "T1",
                    "title": "Write core module",
                    "description": "Create the core module with main logic",
                    "acceptance_criteria": "Module compiles and core logic implemented"
                }]
            }, {
                "id": "S2",
                "title": "Add tests",
                "description": "Write tests for the feature",
                "acceptance_criteria": "All tests pass",
                "depends_on": ["S1"],
                "tasks": [{
                    "id": "T2",
                    "title": "Write unit tests",
                    "description": "Create unit tests for core module",
                    "acceptance_criteria": "Tests pass"
                }]
            }]
        }]
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args_basic() {
        let args = vec![
            "mock-claude".to_string(),
            "-p".to_string(),
            "hello world".to_string(),
        ];
        let parsed = parse_args(&args);
        assert_eq!(parsed.prompt, "hello world");
        assert!(parsed.system_prompt_file.is_none());
        assert!(parsed.resume_session.is_none());
    }

    #[test]
    fn test_parse_args_full() {
        let args = vec![
            "mock-claude".to_string(),
            "-p".to_string(),
            "test prompt".to_string(),
            "--append-system-prompt-file".to_string(),
            "/tmp/spec_session.md".to_string(),
            "--resume".to_string(),
            "session-123".to_string(),
            "--cwd".to_string(),
            "/tmp/workdir".to_string(),
            "--max-turns".to_string(),
            "50".to_string(),
            "--allowedTools".to_string(),
            "Read,Write,Edit".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
        ];
        let parsed = parse_args(&args);
        assert_eq!(parsed.prompt, "test prompt");
        assert_eq!(
            parsed.system_prompt_file,
            Some(PathBuf::from("/tmp/spec_session.md"))
        );
        assert_eq!(parsed.resume_session, Some("session-123".to_string()));
        assert_eq!(parsed.working_dir, Some(PathBuf::from("/tmp/workdir")));
        assert_eq!(parsed.max_turns, Some(50));
        assert_eq!(
            parsed.allowed_tools,
            vec!["Read".to_string(), "Write".to_string(), "Edit".to_string()]
        );
    }

    #[test]
    fn test_extract_short_id_bare() {
        assert_eq!(extract_short_id("Task: [T1] implement"), Some("T1".into()));
        assert_eq!(extract_short_id("Story [S3] test"), Some("S3".into()));
        assert_eq!(extract_short_id("Epic [E2] foo"), Some("E2".into()));
        assert_eq!(extract_short_id("[T1v] verify"), Some("T1v".into()));
    }

    #[test]
    fn test_extract_short_id_wave_prefixed() {
        assert_eq!(
            extract_short_id("Task: [W1-T1] implement"),
            Some("W1-T1".into())
        );
        assert_eq!(extract_short_id("[W2-S3] test story"), Some("W2-S3".into()));
    }

    #[test]
    fn test_extract_short_id_not_found() {
        assert_eq!(extract_short_id("no brackets here"), None);
        assert_eq!(extract_short_id("[invalid]"), None);
        assert_eq!(extract_short_id("[123]"), None);
    }

    #[test]
    fn test_is_valid_short_id() {
        assert!(is_valid_short_id("T1"));
        assert!(is_valid_short_id("S1"));
        assert!(is_valid_short_id("E1"));
        assert!(is_valid_short_id("T1v"));
        assert!(is_valid_short_id("T123"));
        assert!(is_valid_short_id("W1-T1"));
        assert!(is_valid_short_id("W2-S3"));
        assert!(is_valid_short_id("W1-T1v"));

        assert!(!is_valid_short_id(""));
        assert!(!is_valid_short_id("X1"));
        assert!(!is_valid_short_id("T"));
        assert!(!is_valid_short_id("123"));
        assert!(!is_valid_short_id("W-T1"));
        assert!(!is_valid_short_id("W1-"));
    }

    #[test]
    fn test_detect_mode_impl_task() {
        let parsed = ParsedArgs {
            prompt: "Implement the task as described. Task: [T1] Write core module".into(),
            ..Default::default()
        };
        assert!(matches!(detect_mode(&parsed), Mode::ImplTask));
    }

    #[test]
    fn test_detect_mode_verify_task() {
        let parsed = ParsedArgs {
            prompt: "Verify the task implementation".into(),
            system_prompt_file: Some(PathBuf::from("/tmp/verify_task.prompt.md")),
            ..Default::default()
        };
        assert!(matches!(detect_mode(&parsed), Mode::VerifyTask));
    }

    #[test]
    fn test_detect_mode_decompose() {
        let parsed = ParsedArgs {
            prompt: "Decompose the specs into tasks".into(),
            system_prompt_file: Some(PathBuf::from("/tmp/decompose.prompt.md")),
            ..Default::default()
        };
        assert!(matches!(detect_mode(&parsed), Mode::Decompose));
    }

    #[test]
    fn test_detect_mode_unknown() {
        let parsed = ParsedArgs {
            prompt: "Hello world".into(),
            ..Default::default()
        };
        assert!(matches!(detect_mode(&parsed), Mode::Unknown));
    }

    #[test]
    fn test_default_decomposition_json_is_valid() {
        let json = default_decomposition_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("epics").is_some());
        let epics = parsed["epics"].as_array().unwrap();
        assert!(!epics.is_empty());
        let stories = epics[0]["stories"].as_array().unwrap();
        assert!(stories.len() >= 2);
        // Verify dependency structure
        assert!(stories[0]["depends_on"].as_array().unwrap().is_empty());
        assert_eq!(stories[1]["depends_on"][0].as_str().unwrap(), "S1");
    }

    #[test]
    fn test_is_bare_short_id() {
        assert!(is_bare_short_id("T1"));
        assert!(is_bare_short_id("S1"));
        assert!(is_bare_short_id("E1"));
        assert!(is_bare_short_id("T1v"));
        assert!(is_bare_short_id("T123"));

        assert!(!is_bare_short_id(""));
        assert!(!is_bare_short_id("X1"));
        assert!(!is_bare_short_id("T"));
        assert!(!is_bare_short_id("1T"));
    }
}
