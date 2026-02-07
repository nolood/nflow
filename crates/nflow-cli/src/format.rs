//! Output formatting module for nflow CLI.
//!
//! Provides colored, human-readable output for daemon responses.
//! Supports `--json` (raw JSON), `--no-color` (strip ANSI codes), and
//! default pretty-printed output with status colors.

use std::sync::atomic::{AtomicBool, Ordering};

/// Global flag controlling whether ANSI color codes are emitted.
static NO_COLOR: AtomicBool = AtomicBool::new(false);

/// Initialize the color setting. Call once at startup.
pub fn init(no_color: bool) {
    let disable = no_color || std::env::var("NO_COLOR").is_ok() || !atty_stdout();
    NO_COLOR.store(disable, Ordering::Relaxed);
}

/// Check if stdout is a terminal (basic heuristic using libc isatty).
fn atty_stdout() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 }
}

fn color_enabled() -> bool {
    !NO_COLOR.load(Ordering::Relaxed)
}

// ANSI color codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const GRAY: &str = "\x1b[90m";

/// Apply ANSI styling to text, returning plain text if color is disabled.
fn styled(text: &str, codes: &[&str]) -> String {
    if !color_enabled() || codes.is_empty() {
        return text.to_string();
    }
    let prefix: String = codes.concat();
    format!("{}{}{}", prefix, text, RESET)
}

/// Return status icon + colored status string.
fn format_status(status: &str) -> String {
    match status.to_lowercase().as_str() {
        "done" => styled("\u{2714} done", &[GREEN]),
        "in_progress" | "inprogress" => styled("\u{25b6} running", &[YELLOW, BOLD]),
        "ready" => styled("\u{25cb} ready", &[YELLOW]),
        "pending" => styled("\u{00b7} pending", &[GRAY]),
        "failed" => styled("\u{2718} failed", &[RED, BOLD]),
        "cancelled" => styled("\u{2015} cancelled", &[DIM]),
        // Decomposition session statuses
        "approved" => styled("\u{2714} approved", &[GREEN]),
        "in_progress_session" => styled("\u{25b6} in progress", &[YELLOW, BOLD]),
        "discarded" => styled("\u{2015} discarded", &[DIM]),
        other => other.to_string(),
    }
}

/// Format and print a daemon response in human-readable form.
///
/// Dispatches to specialized formatters based on the response data shape.
pub fn print_response(data: &serde_json::Value, command: &str) {
    match data {
        serde_json::Value::Null => {}
        serde_json::Value::String(s) => println!("{}", s),
        serde_json::Value::Array(arr) => print_array(arr, command),
        serde_json::Value::Object(_) => print_object(data, command),
        other => println!("{}", other),
    }
}

/// Print a JSON array as formatted list items.
fn print_array(arr: &[serde_json::Value], command: &str) {
    if arr.is_empty() {
        println!("{}", styled("(no items)", &[DIM]));
        return;
    }
    for item in arr {
        print_list_item(item, command);
    }
}

/// Print a single list item (used for project.list, spec.list, etc.).
fn print_list_item(item: &serde_json::Value, _command: &str) {
    if let Some(obj) = item.as_object() {
        // Detect item shape and format accordingly
        if obj.contains_key("status") && obj.contains_key("short_id") {
            // Work item (story, task, epic)
            let short_id = obj.get("short_id").and_then(|v| v.as_str()).unwrap_or("?");
            let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            println!(
                "  {} {} {}",
                styled(short_id, &[BOLD, CYAN]),
                format_status(status),
                title,
            );
        } else if obj.contains_key("name") && obj.contains_key("status") {
            // Spec item
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let status = obj
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("draft");
            println!("  {} {}", styled(name, &[BOLD]), format_status(status));
        } else if obj.contains_key("name") && obj.contains_key("path") {
            // Project item
            let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("");
            println!("  {} {}", styled(name, &[BOLD, CYAN]), styled(path, &[DIM]),);
        } else {
            // Fallback: pretty-print the JSON
            println!("{}", serde_json::to_string_pretty(item).unwrap_or_default());
        }
    } else {
        // Non-object array items
        println!("  {}", item);
    }
}

/// Print a JSON object response with formatting.
fn print_object(data: &serde_json::Value, command: &str) {
    let obj = match data.as_object() {
        Some(o) => o,
        None => {
            println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
            return;
        }
    };

    // plan.show tree format
    if obj.contains_key("epics") && obj.contains_key("wave_number") {
        print_plan_tree(data);
        return;
    }

    // plan.show DAG format
    if obj.contains_key("adjacency") && obj.contains_key("wave_number") {
        print_plan_dag(data);
        return;
    }

    // exec.status format (waves array)
    if obj.contains_key("waves") {
        print_exec_status(data);
        return;
    }

    // Generic object: print key-value pairs
    for (key, value) in obj {
        match value {
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => {
                println!("{}: {}", styled(key, &[BOLD]), s);
            }
            serde_json::Value::Number(n) => {
                println!("{}: {}", styled(key, &[BOLD]), n);
            }
            serde_json::Value::Bool(b) => {
                println!("{}: {}", styled(key, &[BOLD]), b);
            }
            serde_json::Value::Array(arr) => {
                println!("{}:", styled(key, &[BOLD]));
                print_array(arr, command);
            }
            serde_json::Value::Object(_) => {
                println!("{}:", styled(key, &[BOLD]));
                println!(
                    "{}",
                    serde_json::to_string_pretty(value).unwrap_or_default()
                );
            }
        }
    }
}

/// Render plan.show tree output as an indented ASCII tree with status icons.
///
/// Format:
/// ```text
/// Wave 1 (approved)
/// ├── E1 ✔ done  — Epic title
/// │   ├── S1 ▶ running (2/4)  — Story title
/// │   │   ├── T1 ✔ done  — Task title
/// │   │   └── T1v · pending  — Verify: Task title
/// │   └── S2 ○ ready (0/2)  — Another story [← W1-S1]
/// └── E2 · pending  — Second epic
/// ```
fn print_plan_tree(data: &serde_json::Value) {
    let wave = data
        .get("wave_number")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let status = data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!(
        "{} ({})",
        styled(&format!("Wave {}", wave), &[BOLD]),
        format_status(status),
    );

    let epics = match data.get("epics").and_then(|v| v.as_array()) {
        Some(e) => e,
        None => return,
    };

    let epic_count = epics.len();
    for (ei, epic) in epics.iter().enumerate() {
        let is_last_epic = ei == epic_count - 1;
        let epic_prefix = if is_last_epic {
            "\u{2514}\u{2500}\u{2500}"
        } else {
            "\u{251c}\u{2500}\u{2500}"
        };
        let epic_cont = if is_last_epic { "    " } else { "\u{2502}   " };

        let eid = epic.get("short_id").and_then(|v| v.as_str()).unwrap_or("?");
        let etitle = epic.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let estatus = epic
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("pending");

        println!(
            "{} {} {}  \u{2014} {}",
            epic_prefix,
            styled(eid, &[BOLD, CYAN]),
            format_status(estatus),
            etitle,
        );

        let stories = match epic.get("stories").and_then(|v| v.as_array()) {
            Some(s) => s,
            None => continue,
        };

        let story_count = stories.len();
        for (si, story) in stories.iter().enumerate() {
            let is_last_story = si == story_count - 1;
            let story_prefix = if is_last_story {
                format!("{}\u{2514}\u{2500}\u{2500}", epic_cont)
            } else {
                format!("{}\u{251c}\u{2500}\u{2500}", epic_cont)
            };
            let story_cont = if is_last_story {
                format!("{}    ", epic_cont)
            } else {
                format!("{}\u{2502}   ", epic_cont)
            };

            let sid = story
                .get("short_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let stitle = story.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let sstatus = story
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            let progress = story.get("progress").and_then(|v| v.as_str()).unwrap_or("");

            let progress_str = if progress.is_empty() {
                String::new()
            } else {
                format!(" ({})", styled(progress, &[DIM]))
            };

            // Show dependencies
            let deps = story
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            let deps_str = if deps.is_empty() {
                String::new()
            } else {
                format!(
                    " {}",
                    styled(&format!("[\u{2190} {}]", deps.join(", ")), &[DIM],)
                )
            };

            println!(
                "{} {} {}{}  \u{2014} {}{}",
                story_prefix,
                styled(sid, &[BOLD, CYAN]),
                format_status(sstatus),
                progress_str,
                stitle,
                deps_str,
            );

            // Tasks under story
            let tasks = match story.get("tasks").and_then(|v| v.as_array()) {
                Some(t) => t,
                None => continue,
            };

            let task_count = tasks.len();
            for (ti, task) in tasks.iter().enumerate() {
                let is_last_task = ti == task_count - 1;
                let task_prefix = if is_last_task {
                    format!("{}\u{2514}\u{2500}\u{2500}", story_cont)
                } else {
                    format!("{}\u{251c}\u{2500}\u{2500}", story_cont)
                };

                let tid = task.get("short_id").and_then(|v| v.as_str()).unwrap_or("?");
                let ttitle = task.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let tstatus = task
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");

                println!(
                    "{} {} {}  \u{2014} {}",
                    task_prefix,
                    styled(tid, &[CYAN]),
                    format_status(tstatus),
                    ttitle,
                );
            }
        }
    }
}

/// Render plan.show DAG output as an adjacency list.
fn print_plan_dag(data: &serde_json::Value) {
    let wave = data
        .get("wave_number")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let status = data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!(
        "{} ({}) \u{2014} dependency graph",
        styled(&format!("Wave {}", wave), &[BOLD]),
        format_status(status),
    );

    let adjacency = match data.get("adjacency").and_then(|v| v.as_object()) {
        Some(a) => a,
        None => return,
    };

    for (node, targets) in adjacency {
        let blocked: Vec<&str> = targets
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        if blocked.is_empty() {
            println!(
                "  {} {} {}",
                styled(node, &[BOLD, CYAN]),
                styled("\u{2192}", &[DIM]),
                styled("(no dependents)", &[DIM]),
            );
        } else {
            println!(
                "  {} {} {}",
                styled(node, &[BOLD, CYAN]),
                styled("\u{2192}", &[DIM]),
                blocked
                    .iter()
                    .map(|b| styled(b, &[CYAN]))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
}

/// Render exec.status output.
fn print_exec_status(data: &serde_json::Value) {
    let running_agents = data
        .get("running_agents")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let execution = data
        .get("execution_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let exec_str = if execution {
        styled("enabled", &[GREEN, BOLD])
    } else {
        styled("paused", &[YELLOW])
    };
    println!(
        "Execution: {}  |  Running agents: {}",
        exec_str,
        styled(&running_agents.to_string(), &[BOLD]),
    );
    println!();

    let waves = match data.get("waves").and_then(|v| v.as_array()) {
        Some(w) => w,
        None => return,
    };

    for wave in waves {
        let wn = wave
            .get("wave_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let wstatus = wave
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        println!(
            "{} ({})",
            styled(&format!("Wave {}", wn), &[BOLD]),
            format_status(wstatus),
        );

        if let Some(summary) = wave.get("summary").and_then(|v| v.as_object()) {
            let total = summary.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
            let done = summary.get("done").and_then(|v| v.as_u64()).unwrap_or(0);
            let running = summary.get("running").and_then(|v| v.as_u64()).unwrap_or(0);
            let failed = summary.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
            let pending = summary.get("pending").and_then(|v| v.as_u64()).unwrap_or(0);
            let ready = summary.get("ready").and_then(|v| v.as_u64()).unwrap_or(0);
            let cancelled = summary
                .get("cancelled")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let mut parts = Vec::new();
            parts.push(format!(
                "{}: {}",
                styled("done", &[GREEN]),
                styled(&done.to_string(), &[GREEN, BOLD]),
            ));
            if running > 0 {
                parts.push(format!(
                    "{}: {}",
                    styled("running", &[YELLOW]),
                    styled(&running.to_string(), &[YELLOW, BOLD]),
                ));
            }
            if failed > 0 {
                parts.push(format!(
                    "{}: {}",
                    styled("failed", &[RED]),
                    styled(&failed.to_string(), &[RED, BOLD]),
                ));
            }
            if ready > 0 {
                parts.push(format!("ready: {}", ready));
            }
            if pending > 0 {
                parts.push(format!("{}: {}", styled("pending", &[GRAY]), pending,));
            }
            if cancelled > 0 {
                parts.push(format!("{}: {}", styled("cancelled", &[DIM]), cancelled,));
            }

            println!("  Stories: {}/{}  [{}]", done, total, parts.join(" | "),);
        }

        // Print individual stories if present
        if let Some(stories) = wave.get("stories").and_then(|v| v.as_array()) {
            for story in stories {
                let sid = story
                    .get("short_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let stitle = story.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let sstatus = story
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");
                println!(
                    "    {} {} {}",
                    styled(sid, &[BOLD, CYAN]),
                    format_status(sstatus),
                    stitle,
                );
            }
        }
        println!();
    }
}

/// Format an error message from the daemon with a suggested action.
///
/// Error messages from the daemon use prefixes like "NOT_FOUND:", "ALREADY_EXISTS:",
/// "INVALID_STATE:", "INVALID_PARAMS:" to classify errors.
pub fn format_error(msg: &str) -> String {
    let (prefix, detail) = if let Some(rest) = msg.strip_prefix("NOT_FOUND:") {
        ("not found", rest.trim())
    } else if let Some(rest) = msg.strip_prefix("ALREADY_EXISTS:") {
        ("conflict", rest.trim())
    } else if let Some(rest) = msg.strip_prefix("INVALID_STATE:") {
        ("invalid state", rest.trim())
    } else if let Some(rest) = msg.strip_prefix("INVALID_PARAMS:") {
        ("invalid input", rest.trim())
    } else {
        ("error", msg)
    };

    let header = styled(&format!("{}: ", prefix), &[RED, BOLD]);
    let suggestion = suggest_action(msg);
    if suggestion.is_empty() {
        format!("{}{}", header, detail)
    } else {
        format!(
            "{}{}\n  {} {}",
            header,
            detail,
            styled("hint:", &[DIM]),
            suggestion,
        )
    }
}

/// Return a suggested action for common error patterns.
fn suggest_action(msg: &str) -> &'static str {
    if msg.contains("NOT_FOUND: project") {
        "run `nflow init --name <name>` to create a project"
    } else if msg.contains("NOT_FOUND: spec") {
        "run `nflow spec list` to see available specs"
    } else if msg.contains("NOT_FOUND: wave") {
        "run `nflow plan show` to see available waves"
    } else if msg.contains("daemon not running") || msg.contains("connection refused") {
        "run `nflow daemon start` to start the daemon"
    } else if msg.contains("ALREADY_EXISTS") {
        "use a different name or delete the existing item first"
    } else if msg.contains("INVALID_STATE") {
        "check the current state with `nflow status`"
    } else {
        ""
    }
}

/// Format a streaming log event with colors.
pub fn format_log_event(data: &serde_json::Value) {
    let event_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "text" => {
            if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                print!("{}", text);
            }
        }
        "tool_use" => {
            let name = data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!("{}", styled(&format!("[tool: {}]", name), &[DIM]));
        }
        "tool_result" => {
            if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
                let truncated = if content.len() > 200 {
                    format!("{}...", &content[..200])
                } else {
                    content.to_string()
                };
                println!("{}", styled(&format!("[result: {}]", truncated), &[DIM]));
            }
        }
        "result" => {
            if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                println!("\n{}\n{}", styled("--- Result ---", &[BOLD]), text,);
            }
        }
        "error" => {
            if let Some(msg) = data.get("message").and_then(|v| v.as_str()) {
                eprintln!("{}", styled(&format!("error: {}", msg), &[RED, BOLD]));
            }
        }
        _ => {
            // Unknown event type — print raw JSON for debugging
            println!("{}", data);
        }
    }
}

/// Format a streaming spec event with colors.
pub fn format_stream_event(data: &serde_json::Value) {
    let event_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "text" => {
            if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                print!("{}", text);
            }
        }
        "tool_use" => {
            let name = data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!("{}", styled(&format!("[tool: {}]", name), &[DIM]));
        }
        "tool_result" => {
            if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
                let truncated = if content.len() > 200 {
                    format!("{}...", &content[..200])
                } else {
                    content.to_string()
                };
                println!("{}", styled(&format!("[result: {}]", truncated), &[DIM]));
            }
        }
        "result" => {
            if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                println!("\n{}", text);
            }
        }
        "error" => {
            if let Some(msg) = data.get("message").and_then(|v| v.as_str()) {
                eprintln!("{}", styled(&format!("error: {}", msg), &[RED, BOLD]));
            }
        }
        _ => {
            // Unknown — skip silently for spec dialogue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_status_done() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_status("done");
        assert!(result.contains("done"));
    }

    #[test]
    fn test_format_status_failed() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_status("failed");
        assert!(result.contains("failed"));
    }

    #[test]
    fn test_format_status_in_progress() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_status("in_progress");
        assert!(result.contains("running"));
    }

    #[test]
    fn test_format_status_pending() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_status("pending");
        assert!(result.contains("pending"));
    }

    #[test]
    fn test_format_status_ready() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_status("ready");
        assert!(result.contains("ready"));
    }

    #[test]
    fn test_format_status_cancelled() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_status("cancelled");
        assert!(result.contains("cancelled"));
    }

    #[test]
    fn test_styled_no_color() {
        NO_COLOR.store(true, Ordering::Relaxed);
        assert_eq!(styled("hello", &[RED, BOLD]), "hello");
    }

    #[test]
    fn test_styled_with_color() {
        NO_COLOR.store(false, Ordering::Relaxed);
        let result = styled("hello", &[RED]);
        assert!(result.contains("\x1b[31m"));
        assert!(result.contains("\x1b[0m"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_format_error_not_found() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_error("NOT_FOUND: project 'foo' not found");
        assert!(result.contains("not found"));
        assert!(result.contains("nflow init"));
    }

    #[test]
    fn test_format_error_already_exists() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_error("ALREADY_EXISTS: project already exists");
        assert!(result.contains("conflict"));
        assert!(result.contains("different name"));
    }

    #[test]
    fn test_format_error_invalid_state() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_error("INVALID_STATE: spec is not in draft");
        assert!(result.contains("invalid state"));
        assert!(result.contains("nflow status"));
    }

    #[test]
    fn test_format_error_generic() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let result = format_error("something went wrong");
        assert!(result.contains("error"));
        assert!(result.contains("something went wrong"));
    }

    #[test]
    fn test_print_response_null() {
        NO_COLOR.store(true, Ordering::Relaxed);
        print_response(&serde_json::Value::Null, "test");
    }

    #[test]
    fn test_print_response_string() {
        NO_COLOR.store(true, Ordering::Relaxed);
        print_response(&serde_json::json!("hello"), "test");
    }

    #[test]
    fn test_print_response_array() {
        NO_COLOR.store(true, Ordering::Relaxed);
        print_response(
            &serde_json::json!([{"name": "proj", "path": "/tmp"}]),
            "project.list",
        );
    }

    #[test]
    fn test_print_response_empty_array() {
        NO_COLOR.store(true, Ordering::Relaxed);
        print_response(&serde_json::json!([]), "spec.list");
    }

    #[test]
    fn test_print_plan_tree() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "epics": [{
                "short_id": "W1-E1",
                "title": "My Epic",
                "status": "in_progress",
                "stories": [{
                    "short_id": "W1-S1",
                    "title": "My Story",
                    "status": "done",
                    "progress": "2/2",
                    "depends_on": [],
                    "tasks": [{
                        "short_id": "W1-T1",
                        "title": "Impl task",
                        "status": "done",
                        "kind": "impl"
                    }, {
                        "short_id": "W1-T1v",
                        "title": "Verify task",
                        "status": "done",
                        "kind": "verify"
                    }]
                }]
            }]
        });
        print_plan_tree(&data);
    }

    #[test]
    fn test_print_plan_dag() {
        NO_COLOR.store(true, Ordering::Relaxed);
        let data = serde_json::json!({
            "wave_number": 1,
            "status": "approved",
            "adjacency": {
                "W1-S1": ["W1-S2", "W1-S3"],
                "W1-S2": [],
                "W1-S3": []
            }
        });
        print_plan_dag(&data);
    }

    #[test]
    fn test_format_log_event_text() {
        NO_COLOR.store(true, Ordering::Relaxed);
        format_log_event(&serde_json::json!({"type": "text", "text": "hello"}));
    }

    #[test]
    fn test_format_log_event_tool_use() {
        NO_COLOR.store(true, Ordering::Relaxed);
        format_log_event(&serde_json::json!({"type": "tool_use", "name": "Read"}));
    }

    #[test]
    fn test_format_log_event_error() {
        NO_COLOR.store(true, Ordering::Relaxed);
        format_log_event(&serde_json::json!({"type": "error", "message": "oops"}));
    }

    #[test]
    fn test_format_stream_event_text() {
        NO_COLOR.store(true, Ordering::Relaxed);
        format_stream_event(&serde_json::json!({"type": "text", "text": "spec output"}));
    }

    #[test]
    fn test_format_stream_event_unknown_skipped() {
        NO_COLOR.store(true, Ordering::Relaxed);
        format_stream_event(&serde_json::json!({"type": "custom", "value": 42}));
    }

    #[test]
    fn test_suggest_action_patterns() {
        assert!(!suggest_action("NOT_FOUND: project 'x' not found").is_empty());
        assert!(!suggest_action("NOT_FOUND: spec not found").is_empty());
        assert!(!suggest_action("NOT_FOUND: wave not found").is_empty());
        assert!(!suggest_action("daemon not running").is_empty());
        assert!(!suggest_action("ALREADY_EXISTS: x").is_empty());
        assert!(!suggest_action("INVALID_STATE: x").is_empty());
        assert!(suggest_action("random error").is_empty());
    }
}
