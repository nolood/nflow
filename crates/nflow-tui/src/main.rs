mod app;
mod daemon_client;
mod error;
mod event;
mod socket_client;
mod terminal;
mod ui;

use std::env;
use std::time::Duration;

use clap::Parser;
use crossterm::event::Event;

use app::{App, DialogueSessionState, View};
use daemon_client::ensure_daemon;
use error::TuiError;
use socket_client::{ResponseStatus, SocketClient};

/// nflow TUI — interactive terminal interface for nflow orchestrator
#[derive(Debug, Parser)]
#[command(name = "nflow-tui", version, about)]
struct Args {
    /// Override project name (default: detected from current directory)
    #[arg(long)]
    project: Option<String>,
}

/// Resolve the project name from CLI flag or auto-detection from cwd.
fn resolve_project(flag: &Option<String>) -> error::Result<String> {
    if let Some(name) = flag {
        return Ok(name.clone());
    }
    let cwd = env::current_dir().map_err(|e| {
        TuiError::Io(std::io::Error::new(
            e.kind(),
            "failed to determine current directory",
        ))
    })?;
    let dir_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    Ok(dir_name.to_string())
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if let Err(e) = run(args).await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn run(args: Args) -> error::Result<()> {
    let project = resolve_project(&args.project)?;

    // Ensure daemon is running (auto-start if needed)
    eprintln!("Starting daemon...");
    ensure_daemon()?;

    // Connect to daemon
    let mut client = SocketClient::connect().await?;

    // Create app state
    let mut app = App::new(project);

    // Try initial connection
    app.connect(&mut client).await.ok();

    // Fetch plan tree data
    app.fetch_plan(&mut client).await.ok();

    // Fetch execute tree data
    app.fetch_execute(&mut client).await.ok();

    // Set up terminal
    let mut tui = terminal::setup()?;

    // Main event loop
    let result = event_loop(&mut tui, &mut app, &mut client).await;

    // Always restore terminal on exit
    terminal::restore(&mut tui)?;

    result
}

/// Main event loop: render frame, poll events, handle input.
async fn event_loop(
    tui: &mut terminal::Tui,
    app: &mut App,
    client: &mut SocketClient,
) -> error::Result<()> {
    loop {
        // Render current frame
        tui.draw(|frame| ui::render(app, frame))
            .map_err(|e| TuiError::Terminal(format!("failed to draw frame: {}", e)))?;

        // If in dialogue streaming mode, poll for streaming data
        if app.in_dialogue() {
            let is_streaming = app
                .spec_dialogue
                .as_ref()
                .is_some_and(|d| d.session_state == DialogueSessionState::Streaming);

            if is_streaming {
                poll_streaming_data(app, client).await;
            }
        }

        // If in execute view with streaming output, poll for execute streaming data
        if app.current_view == View::Execute && app.in_execute_streaming() {
            poll_execute_streaming_data(app, client).await;
        }

        // Poll for input events (16ms ≈ 60fps)
        let evt = event::poll_event(Duration::from_millis(16))
            .map_err(|e| TuiError::Terminal(format!("failed to poll events: {}", e)))?;

        if let Some(Event::Key(key)) = evt {
            let (cont, action) = event::handle_key_event(app, key);

            // Handle view actions
            match action {
                event::ViewAction::SpecNew => {
                    handle_spec_new(app, client).await;
                }
                event::ViewAction::SpecResume => {
                    handle_spec_resume(app, client).await;
                }
                event::ViewAction::DialogueSendAnswer(text) => {
                    handle_dialogue_send_answer(app, client, &text).await;
                }
                event::ViewAction::DialogueEndSession => {
                    handle_dialogue_end(app);
                }
                event::ViewAction::DialogueExit => {
                    app.exit_dialogue();
                    // Refresh specs list after dialogue
                    app.fetch_specs(client).await.ok();
                }
                event::ViewAction::SpecView => {
                    handle_spec_view(app, client).await;
                }
                event::ViewAction::PagerExit => {
                    app.exit_pager();
                }
                event::ViewAction::PlanGenerate {
                    spec_names,
                    with_codebase,
                } => {
                    handle_plan_generate(app, client, spec_names, with_codebase).await;
                }
                event::ViewAction::PlanFeedback(text) => {
                    handle_plan_feedback(app, client, &text).await;
                }
                event::ViewAction::PlanApprove => {
                    handle_plan_approve(app, client).await;
                }
                event::ViewAction::PlanDiscard => {
                    handle_plan_discard(app, client).await;
                }
                event::ViewAction::PlanSubViewExit => {
                    app.close_plan_sub_view();
                }
                event::ViewAction::ExecuteSelectTask(task_id) => {
                    handle_execute_select_task(app, client, &task_id).await;
                }
                _ => {}
            }

            if !cont {
                break;
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Poll for streaming data from the daemon (non-blocking).
async fn poll_streaming_data(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            let dialogue = match &mut app.spec_dialogue {
                Some(d) => d,
                None => return,
            };

            if line.status == ResponseStatus::Error {
                let msg = line
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                dialogue.add_claude_message(format!("[Error: {}]", msg));
                dialogue.session_state = DialogueSessionState::Completed;
                return;
            }

            if line.done {
                // Extract spec_id from done line
                if let Some(id) = line.data.get("spec_id").and_then(|v| v.as_str()) {
                    dialogue.spec_id = Some(id.to_string());
                }
                let completed = line
                    .data
                    .get("completed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if completed {
                    dialogue.add_claude_message("\n[Session completed]".to_string());
                    dialogue.session_state = DialogueSessionState::Completed;
                } else {
                    // Claude is waiting for user input
                    dialogue.session_state = DialogueSessionState::WaitingForInput;
                }
                return;
            }

            // Process event by type
            let event_type = line.data.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "text" | "result" => {
                    if let Some(text) = line.data.get("text").and_then(|v| v.as_str()) {
                        dialogue.add_claude_message(text.to_string());
                    }
                }
                "question" => {
                    // Display question text as a Claude message
                    if let Some(text) = line.data.get("text").and_then(|v| v.as_str()) {
                        dialogue.add_claude_message(text.to_string());
                    }
                    // Show options if any
                    if let Some(options) = line.data.get("options").and_then(|v| v.as_array()) {
                        let mut opts_text = String::new();
                        for (i, opt) in options.iter().enumerate() {
                            let label = opt.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                            opts_text.push_str(&format!("\n  {}. {}", i + 1, label));
                        }
                        if !opts_text.is_empty() {
                            dialogue.add_claude_message(opts_text);
                        }
                    }
                }
                _ => {
                    // Silently skip unknown event types
                }
            }
        }
        Ok(None) => {
            // No data available yet — normal timeout
        }
        Err(_) => {
            // Connection error during streaming
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.add_claude_message("[Connection lost]".to_string());
                dialogue.session_state = DialogueSessionState::Completed;
            }
        }
    }
}

/// Handle starting a new spec dialogue.
async fn handle_spec_new(app: &mut App, client: &mut SocketClient) {
    // Enter dialogue mode with a placeholder name
    app.enter_dialogue("New Spec".to_string());

    let params = serde_json::json!({
        "project_name": &app.project,
    });

    match client.send_streaming_command("spec.new", params).await {
        Ok(request_id) => {
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.request_id = Some(request_id);
            }
        }
        Err(e) => {
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.add_claude_message(format!("[Failed to start: {}]", e));
                dialogue.session_state = DialogueSessionState::Completed;
            }
        }
    }
}

/// Handle resuming a spec dialogue.
async fn handle_spec_resume(app: &mut App, client: &mut SocketClient) {
    let spec_name = app
        .specs_list
        .selected_item()
        .map(|s| s.name.clone())
        .unwrap_or_default();

    if spec_name.is_empty() {
        app.status_message = "No spec selected".to_string();
        return;
    }

    app.enter_dialogue(spec_name.clone());

    let params = serde_json::json!({
        "project_name": &app.project,
        "spec_name": &spec_name,
    });

    match client.send_streaming_command("spec.resume", params).await {
        Ok(request_id) => {
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.request_id = Some(request_id);
            }
        }
        Err(e) => {
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.add_claude_message(format!("[Failed to resume: {}]", e));
                dialogue.session_state = DialogueSessionState::Completed;
            }
        }
    }
}

/// Handle sending an answer in the dialogue.
async fn handle_dialogue_send_answer(app: &mut App, client: &mut SocketClient, text: &str) {
    let spec_id = app
        .spec_dialogue
        .as_ref()
        .and_then(|d| d.spec_id.clone())
        .unwrap_or_default();

    if spec_id.is_empty() {
        if let Some(dialogue) = &mut app.spec_dialogue {
            dialogue.add_claude_message("[Cannot send: no spec_id available yet]".to_string());
            dialogue.session_state = DialogueSessionState::WaitingForInput;
        }
        return;
    }

    let params = serde_json::json!({
        "spec_id": &spec_id,
        "answer_text": text,
    });

    match client.send_streaming_command("spec.answer", params).await {
        Ok(request_id) => {
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.request_id = Some(request_id);
            }
        }
        Err(e) => {
            if let Some(dialogue) = &mut app.spec_dialogue {
                dialogue.add_claude_message(format!("[Failed to send answer: {}]", e));
                dialogue.session_state = DialogueSessionState::WaitingForInput;
            }
        }
    }
}

/// Handle ending a dialogue session.
fn handle_dialogue_end(app: &mut App) {
    if let Some(dialogue) = &mut app.spec_dialogue {
        dialogue.add_claude_message("[Session ended by user]".to_string());
        dialogue.session_state = DialogueSessionState::Completed;
    }
}

/// Handle plan generate action.
async fn handle_plan_generate(
    app: &mut App,
    client: &mut SocketClient,
    spec_names: Vec<String>,
    with_codebase: bool,
) {
    let params = serde_json::json!({
        "project_name": &app.project,
        "spec_names": spec_names,
        "with_codebase": with_codebase,
    });

    match client.send_command("plan.generate", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = "Plan generation started".to_string();
            // Refresh plan tree
            app.fetch_plan(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to generate plan");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle plan feedback action.
async fn handle_plan_feedback(app: &mut App, client: &mut SocketClient, text: &str) {
    let params = serde_json::json!({
        "project_name": &app.project,
        "feedback": text,
    });

    match client.send_command("plan.feedback", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = "Feedback sent".to_string();
            // Refresh plan tree
            app.fetch_plan(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to send feedback");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle plan approve action.
async fn handle_plan_approve(app: &mut App, client: &mut SocketClient) {
    let params = serde_json::json!({
        "project_name": &app.project,
    });

    match client.send_command("plan.approve", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = "Plan approved".to_string();
            // Refresh plan tree
            app.fetch_plan(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to approve plan");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle plan discard action.
async fn handle_plan_discard(app: &mut App, client: &mut SocketClient) {
    let params = serde_json::json!({
        "project_name": &app.project,
    });

    match client.send_command("plan.discard", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = "Plan discarded".to_string();
            // Refresh plan tree
            app.fetch_plan(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to discard plan");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle selecting a task in the execute view to display its log output.
async fn handle_execute_select_task(app: &mut App, client: &mut SocketClient, task_id: &str) {
    let is_running = app.execute_tree.selected_task_status().as_deref() == Some("in_progress");

    let params = serde_json::json!({
        "project_name": &app.project,
        "task_id": task_id,
        "follow": is_running,
    });

    if is_running {
        // Start streaming for live output
        match client.send_streaming_command("exec.log", params).await {
            Ok(request_id) => {
                app.execute_output
                    .start_streaming(task_id.to_string(), request_id);
            }
            Err(e) => {
                app.execute_output.set_historical(
                    task_id.to_string(),
                    vec![format!("[Failed to start streaming: {}]", e)],
                );
            }
        }
    } else {
        // Fetch historical log
        match client.send_command("exec.log", params).await {
            Ok(resp) if resp.status == ResponseStatus::Ok => {
                let lines = parse_log_lines(&resp.data);
                app.execute_output
                    .set_historical(task_id.to_string(), lines);
            }
            Ok(resp) => {
                let msg = resp
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Failed to fetch log");
                app.execute_output
                    .set_historical(task_id.to_string(), vec![msg.to_string()]);
            }
            Err(e) => {
                app.execute_output
                    .set_historical(task_id.to_string(), vec![format!("[Error: {}]", e)]);
            }
        }
    }
}

/// Parse log lines from exec.log response data.
fn parse_log_lines(data: &serde_json::Value) -> Vec<String> {
    // The exec.log command returns events as an array
    if let Some(events) = data.get("events").and_then(|v| v.as_array()) {
        events
            .iter()
            .filter_map(|evt| {
                let text = evt.get("text").and_then(|v| v.as_str())?;
                Some(text.to_string())
            })
            .collect()
    } else if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
        // Fallback: plain text content
        content.lines().map(|l| l.to_string()).collect()
    } else {
        vec!["No log content available.".to_string()]
    }
}

/// Poll for streaming execute output data (non-blocking).
async fn poll_execute_streaming_data(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = std::time::Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            if line.status == ResponseStatus::Error {
                let msg = line
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                app.execute_output.append_line(format!("[Error: {}]", msg));
                app.execute_output.stop_streaming();
                return;
            }

            if line.done {
                app.execute_output.stop_streaming();
                return;
            }

            // Append text content from streaming events
            if let Some(text) = line.data.get("text").and_then(|v| v.as_str()) {
                app.execute_output.append_line(text.to_string());
            }
        }
        Ok(None) => {
            // Timeout — no data yet
        }
        Err(_) => {
            app.execute_output
                .append_line("[Connection lost]".to_string());
            app.execute_output.stop_streaming();
        }
    }
}

/// Handle viewing a spec's content in the pager.
async fn handle_spec_view(app: &mut App, client: &mut SocketClient) {
    let spec_name = app
        .specs_list
        .selected_item()
        .map(|s| s.name.clone())
        .unwrap_or_default();

    if spec_name.is_empty() {
        app.status_message = "No spec selected".to_string();
        return;
    }

    let params = serde_json::json!({
        "project_name": &app.project,
        "spec_name": &spec_name,
    });

    match client.send_command("spec.view", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            let content = resp
                .data
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            app.enter_pager(spec_name, content);
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to load spec");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_project_explicit() {
        let result = resolve_project(&Some("my-project".to_string())).unwrap();
        assert_eq!(result, "my-project");
    }

    #[test]
    fn test_resolve_project_auto() {
        let result = resolve_project(&None).unwrap();
        assert!(!result.is_empty());
    }
}
