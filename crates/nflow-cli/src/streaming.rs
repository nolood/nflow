use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::{CliError, Result};
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

/// Handle a streaming pipeline start session (`nflow pipeline start`).
///
/// Streams pipeline progress events to stdout. Stops on:
/// - Pipeline completion (`done: true`)
/// - Ctrl+C (graceful exit)
/// - Daemon error
pub async fn handle_pipeline_stream(
    client: &mut SocketClient,
    params: serde_json::Value,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let mut reader = client
        .send_streaming_command("pipeline.start", params)
        .await?;

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

        crate::format::format_pipeline_event(&line.data);

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
pub async fn handle_spec_dialogue(
    client: &mut SocketClient,
    command: &str,
    params: serde_json::Value,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
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
