use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::{CliError, Result};
use crate::format;
use crate::socket_client::{ResponseStatus, SocketClient};

/// Handle a streaming log follow session (`nflow log <task> -f`).
///
/// Prints each streaming line as received. Stops on:
/// - Stream completion (`done: true`)
/// - Ctrl+C (graceful exit)
/// - Daemon error
pub async fn handle_log_follow(
    client: &mut SocketClient,
    task_id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let params = serde_json::json!({
        "task_id": task_id,
        "follow": true,
    });

    let mut reader = client.send_streaming_command("exec.log", params).await?;

    while let Some(line) = reader.next_line().await? {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        if line.status == ResponseStatus::Error {
            let msg = line
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            eprintln!("error: {}", msg);
            return Err(CliError::Socket(msg.to_string()));
        }

        crate::format::format_log_event(&line.data);

        if line.done {
            break;
        }
    }

    Ok(())
}

/// Check if stdin is a terminal (for interactive mode detection).
fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}

/// Handle a streaming pipeline start session (`nflow pipeline start`).
///
/// Streams pipeline progress events to stdout. In manual mode with a tty stdin,
/// questions and approval gates are handled interactively:
/// - PipelineQuestion events prompt the user for answers
/// - WaitingForApproval/WaitingForFinalApproval prompt for approve/reject
///
/// If stdin is not a tty, interactive prompts are skipped (non-interactive fallback).
pub async fn handle_pipeline_stream(
    client: &mut SocketClient,
    params: serde_json::Value,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let is_manual = params
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        == "manual";
    let interactive = is_manual && stdin_is_tty();

    let mut reader = client
        .send_streaming_command("pipeline.start", params)
        .await?;

    let mut pipeline_run_id: Option<String> = None;

    while let Some(line) = reader.next_line().await? {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        if line.status == ResponseStatus::Error {
            let msg = line
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            eprintln!("error: {}", msg);
            return Err(CliError::Socket(msg.to_string()));
        }

        // Track pipeline_run_id from events
        if let Some(id) = line.data.get("pipeline_run_id").and_then(|v| v.as_str()) {
            pipeline_run_id = Some(id.to_string());
        }

        let event_type = line.data.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if interactive && event_type == "pipeline_question" {
            // Interactive question handling
            let question = line
                .data
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let context = line.data.get("context").and_then(|v| v.as_str());
            let question_id = line
                .data
                .get("question_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let run_id = line
                .data
                .get("pipeline_run_id")
                .and_then(|v| v.as_str())
                .or(pipeline_run_id.as_deref())
                .unwrap_or("");

            println!("\nQuestion: {}", question);
            if let Some(ctx) = context {
                if !ctx.is_empty() {
                    println!("Context: {}", ctx);
                }
            }

            let answer = read_user_input_with_prompt("Your answer: ", cancelled.clone()).await?;
            match answer {
                Some(text) => {
                    // Send pipeline.answer command
                    let answer_params = serde_json::json!({
                        "pipeline_run_id": run_id,
                        "question_id": question_id,
                        "answer": text,
                    });
                    // Use a separate connection for the answer command
                    let mut answer_client = SocketClient::connect().await?;
                    let resp = answer_client
                        .send_command("pipeline.answer", answer_params)
                        .await?;
                    if resp.status == ResponseStatus::Error {
                        let msg = resp
                            .data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("failed to submit answer");
                        eprintln!("warning: {}", msg);
                    }
                }
                None => {
                    // User cancelled — continue streaming without answering
                    eprintln!("(skipped question)");
                }
            }
        } else if interactive
            && (event_type == "waiting_for_approval"
                || event_type == "waiting_for_final_approval")
        {
            let run_id = line
                .data
                .get("pipeline_run_id")
                .and_then(|v| v.as_str())
                .or(pipeline_run_id.as_deref())
                .unwrap_or("");

            let label = if event_type == "waiting_for_approval" {
                "Plan"
            } else {
                "Final result"
            };

            // Print the plan/summary if available
            if let Some(summary) = line.data.get("plan_summary").and_then(|v| v.as_str()) {
                println!("\n{}", summary);
            } else if let Some(summary) = line.data.get("summary").and_then(|v| v.as_str()) {
                println!("\n{}", summary);
            }

            println!("\n{} ready for review.", label);
            let answer =
                read_user_input_with_prompt("Approve? (y/n/feedback): ", cancelled.clone())
                    .await?;
            match answer {
                Some(text) => {
                    let trimmed = text.trim().to_lowercase();
                    let mut cmd_client = SocketClient::connect().await?;
                    if trimmed == "y" || trimmed == "yes" {
                        let approve_params =
                            serde_json::json!({ "pipeline_run_id": run_id });
                        let resp = cmd_client
                            .send_command("pipeline.approve", approve_params)
                            .await?;
                        if resp.status == ResponseStatus::Ok {
                            println!("{} approved.", label);
                        } else {
                            let msg = resp
                                .data
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("failed to approve");
                            eprintln!("error: {}", msg);
                        }
                    } else {
                        // Treat as rejection with feedback
                        let feedback = if trimmed == "n" || trimmed == "no" {
                            // Prompt for feedback
                            let fb = read_user_input_with_prompt(
                                "Feedback: ",
                                cancelled.clone(),
                            )
                            .await?;
                            fb.unwrap_or_default()
                        } else {
                            // The text itself is the feedback
                            text
                        };
                        let reject_params = serde_json::json!({
                            "pipeline_run_id": run_id,
                            "feedback": feedback,
                        });
                        let resp = cmd_client
                            .send_command("pipeline.reject", reject_params)
                            .await?;
                        if resp.status == ResponseStatus::Ok {
                            println!("{} rejected.", label);
                        } else {
                            let msg = resp
                                .data
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("failed to reject");
                            eprintln!("error: {}", msg);
                        }
                    }
                }
                None => {
                    eprintln!("(skipped approval)");
                }
            }
        } else {
            format::format_pipeline_event(&line.data);
        }

        if line.done {
            break;
        }
    }

    Ok(())
}

/// Handle an interactive spec dialogue session.
///
/// The session flow:
/// 1. Send initial command (spec.new or spec.resume)
/// 2. Stream output, printing text events and collecting question events
/// 3. When stream completes (done: true), extract spec_id
/// 4. If a question was pending and session not completed, prompt user for input
/// 5. Send the answer via `spec.answer` and start a new stream
/// 6. Repeat until session completes or user sends Ctrl+D
///
/// In non-interactive mode (or when stdin is not a TTY), questions are stored
/// in the daemon and the user is directed to `nflow spec questions`.
pub async fn handle_spec_dialogue(
    client: &mut SocketClient,
    command: &str,
    params: serde_json::Value,
    cancelled: Arc<AtomicBool>,
    non_interactive: bool,
) -> Result<()> {
    let interactive = !non_interactive && stdin_is_tty();
    let spec_name_hint = params
        .get("spec_name")
        .and_then(|v| v.as_str())
        .unwrap_or("<name>")
        .to_string();
    let mut reader = client.send_streaming_command(command, params).await?;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }

        // Read all streaming events until done
        let mut pending_question: Option<serde_json::Value> = None;
        let mut spec_id: Option<String> = None;
        #[allow(unused_assignments)]
        let mut completed = false;

        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            match reader.next_line().await? {
                None => {
                    // Stream ended without done line
                    return Ok(());
                }
                Some(line) => {
                    if line.status == ResponseStatus::Error {
                        let msg = line
                            .data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        eprintln!("error: {}", msg);
                        return Err(CliError::Socket(msg.to_string()));
                    }

                    if line.done {
                        // Extract metadata from the done line
                        if let Some(id) = line.data.get("spec_id").and_then(|v| v.as_str()) {
                            spec_id = Some(id.to_string());
                        }
                        completed = line
                            .data
                            .get("completed")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        break;
                    }

                    // Check event type
                    let event_type = line.data.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    if event_type == "question" {
                        // Store question for prompting after stream ends
                        pending_question = Some(line.data.clone());
                    } else {
                        crate::format::format_stream_event(&line.data);
                    }
                }
            }
        }

        // Stream ended — handle completion or prompt for answer
        if completed {
            println!("\nSpec session completed.");
            break;
        }

        if let Some(question) = pending_question {
            if !interactive {
                // Non-interactive mode: questions stored in daemon, print hint and exit
                println!(
                    "\nSpec session paused with pending questions.\nView with: nflow spec questions {}",
                    spec_name_hint
                );
                break;
            }

            // Display the question
            let question_text = question.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !question_text.is_empty() {
                println!("\n{}", question_text);
            }

            // Display options if any
            if let Some(options) = question.get("options").and_then(|v| v.as_array()) {
                for (i, opt) in options.iter().enumerate() {
                    let label = opt.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                    println!("  {}. {}", i + 1, label);
                }
            }

            let sid = match &spec_id {
                Some(id) => id.clone(),
                None => {
                    eprintln!("warning: no spec_id in done line, cannot send answer");
                    break;
                }
            };

            // Prompt for input
            let answer = read_user_input(cancelled.clone()).await?;
            match answer {
                Some(text) => {
                    let answer_params = serde_json::json!({
                        "spec_id": sid,
                        "answer_text": text,
                    });
                    reader = client
                        .send_streaming_command("spec.answer", answer_params)
                        .await?;
                    // Continue the outer loop — read the next stream
                }
                None => {
                    // User sent Ctrl+D or cancelled
                    println!("\nSession ended by user.");
                    break;
                }
            }
        } else {
            // No question pending, session paused without a question
            println!("\nSpec session paused. Resume with: nflow spec resume");
            break;
        }
    }

    Ok(())
}

/// Read a line of input from the user via stdin with a custom prompt.
///
/// Returns `None` on EOF (Ctrl+D) or if cancelled (Ctrl+C).
async fn read_user_input_with_prompt(
    prompt: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<String>> {
    eprint!("{}", prompt);

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    tokio::select! {
        result = reader.read_line(&mut line) => {
            match result {
                Ok(0) => Ok(None), // EOF (Ctrl+D)
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    Ok(Some(trimmed.to_string()))
                }
                Err(e) => Err(CliError::Io(e)),
            }
        }
        _ = wait_for_cancel(cancelled) => {
            Ok(None)
        }
    }
}

/// Read a line of input from the user via stdin.
///
/// Returns `None` on EOF (Ctrl+D) or if cancelled (Ctrl+C).
async fn read_user_input(cancelled: Arc<AtomicBool>) -> Result<Option<String>> {
    eprint!("> ");

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    tokio::select! {
        result = reader.read_line(&mut line) => {
            match result {
                Ok(0) => Ok(None), // EOF (Ctrl+D)
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    Ok(Some(trimmed.to_string()))
                }
                Err(e) => Err(CliError::Io(e)),
            }
        }
        _ = wait_for_cancel(cancelled) => {
            Ok(None)
        }
    }
}

/// Wait until the cancellation flag is set.
async fn wait_for_cancel(cancelled: Arc<AtomicBool>) {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::format;

    #[test]
    fn test_format_log_event_text() {
        format::init(true);
        let data = serde_json::json!({"type": "text", "text": "hello world"});
        format::format_log_event(&data);
    }

    #[test]
    fn test_format_log_event_tool_use() {
        format::init(true);
        let data = serde_json::json!({"type": "tool_use", "name": "Read"});
        format::format_log_event(&data);
    }

    #[test]
    fn test_format_log_event_error() {
        format::init(true);
        let data = serde_json::json!({"type": "error", "message": "something failed"});
        format::format_log_event(&data);
    }

    #[test]
    fn test_format_log_event_unknown() {
        format::init(true);
        let data = serde_json::json!({"type": "custom", "value": 42});
        format::format_log_event(&data);
    }

    #[test]
    fn test_format_stream_event_text() {
        format::init(true);
        let data = serde_json::json!({"type": "text", "text": "spec output"});
        format::format_stream_event(&data);
    }

    #[test]
    fn test_format_stream_event_result() {
        format::init(true);
        let data = serde_json::json!({"type": "result", "text": "final answer"});
        format::format_stream_event(&data);
    }

    #[test]
    fn test_format_log_event_tool_result_truncation() {
        format::init(true);
        let long_content = "x".repeat(300);
        let data = serde_json::json!({"type": "tool_result", "content": long_content});
        format::format_log_event(&data);
    }

    #[test]
    fn test_format_log_event_tool_result_short() {
        format::init(true);
        let data = serde_json::json!({"type": "tool_result", "content": "short result"});
        format::format_log_event(&data);
    }

    #[test]
    fn test_format_stream_event_unknown_skipped() {
        format::init(true);
        // Unknown events should be silently skipped in spec dialogue
        let data = serde_json::json!({"type": "custom_internal", "value": 42});
        format::format_stream_event(&data);
        // No panic, no output
    }

    #[test]
    fn test_format_stream_event_tool_result() {
        format::init(true);
        let data = serde_json::json!({"type": "tool_result", "content": "some tool output"});
        format::format_stream_event(&data);
    }

    #[test]
    fn test_format_stream_event_error() {
        format::init(true);
        let data = serde_json::json!({"type": "error", "message": "oops"});
        format::format_stream_event(&data);
    }
}
