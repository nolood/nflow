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

use std::time::Instant;

use app::{
    App, DecompositionPhase, DialogueSessionState, PipelineDetailState, PipelineRunItem,
    PipelineStageItem, View,
};
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

    // Connect to daemon (retry a few times — the socket file may exist
    // before the daemon is actually listening)
    let mut client = {
        let max_retries = 10;
        let mut last_err = None;
        let mut connected = None;
        for i in 0..max_retries {
            match SocketClient::connect().await {
                Ok(c) => {
                    connected = Some(c);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    if i < max_retries - 1 {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
        match connected {
            Some(c) => c,
            None => return Err(last_err.unwrap()),
        }
    };

    // Auto-init project if it doesn't exist yet
    ensure_project_initialized(&project, &mut client).await;

    // Create app state
    let mut app = App::new(project);

    // Try initial connection
    app.connect(&mut client).await.ok();

    // Fetch plan tree data
    app.fetch_plan(&mut client).await.ok();

    // Detect if decomposition is currently in progress:
    // A wave with status "in_progress" and no epics means Claude is still generating.
    if app.plan_tree.wave_status.as_deref() == Some("in_progress") && !app.plan_tree.has_epics() {
        app.decomposition_phase = DecompositionPhase::Analyzing;
        app.decomposition_last_event_at = Some(Instant::now());
        app.status_message = "Plan generation in progress...".to_string();
    }

    // Fetch execute tree data
    app.fetch_execute(&mut client).await.ok();

    // Subscribe to daemon events for real-time updates
    app.subscribe_events(&mut client).await.ok();

    // Set up terminal
    let mut tui = terminal::setup()?;

    // Main event loop
    let result = event_loop(&mut tui, &mut app, &mut client).await;

    // Always restore terminal on exit
    terminal::restore(&mut tui)?;

    result
}

/// Check if the project exists in the daemon DB; if not, auto-initialize it.
async fn ensure_project_initialized(project: &str, client: &mut SocketClient) {
    // Try to get project status — if it fails with NOT_FOUND, init
    let resp = client
        .send_command(
            "exec.status",
            serde_json::json!({ "project_name": project }),
        )
        .await;

    let needs_init = match resp {
        Ok(r) => {
            if r.status == ResponseStatus::Ok {
                false
            } else {
                let msg = r.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
                msg.contains("NOT_FOUND")
            }
        }
        Err(_) => false, // connection error, don't try init
    };

    if needs_init {
        let cwd = env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let params = serde_json::json!({
            "name": project,
            "path": cwd,
            "base_branch": "main",
        });

        match client.send_command("project.init", params).await {
            Ok(r) if r.status == ResponseStatus::Ok => {
                eprintln!("Project '{}' initialized automatically", project);
            }
            Ok(r) => {
                let msg = r
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                eprintln!("Warning: auto-init failed: {}", msg);
            }
            Err(e) => {
                eprintln!("Warning: auto-init failed: {}", e);
            }
        }
    }
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

        // Auto-dismiss completion/failure banner after ~5 seconds
        if matches!(
            &app.decomposition_phase,
            DecompositionPhase::Completed(_) | DecompositionPhase::Failed(_)
        ) {
            if let Some(shown_at) = app.completion_shown_at {
                if shown_at.elapsed() > Duration::from_secs(5) {
                    app.decomposition_phase = DecompositionPhase::Idle;
                    app.completion_shown_at = None;
                }
            }
        }

        // Stale stream detection: 120s of no events while decomposition is active
        if app.is_decomposition_active() {
            if let Some(last_event) = app.decomposition_last_event_at {
                if last_event.elapsed() > Duration::from_secs(120) {
                    app.decomposition_output
                        .append_line("[Stream timed out -- no data for 120s]".to_string());
                    app.decomposition_output.stop_streaming();
                    app.decomposition_phase =
                        DecompositionPhase::Failed("Stream timed out".to_string());
                    app.completion_shown_at = Some(Instant::now());
                    app.decomposition_last_event_at = None;
                }
            }
        }

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

        // If in logs view with streaming output, poll for logs streaming data
        if app.in_logs_streaming() {
            poll_logs_streaming_data(app, client).await;
        }

        // If in plan view with decomposition streaming, poll for decomposition streaming data
        if app.in_decomposition_streaming() {
            poll_decomposition_streaming_data(app, client).await;
        }

        // If pipeline is streaming, poll for pipeline data
        if app.in_pipeline_streaming() {
            poll_pipeline_streaming_data(app, client).await;
        }

        // Poll for daemon events when subscribed and not actively streaming
        // (when streaming, events arrive interleaved and are handled there)
        if app.is_event_subscribed()
            && !app.in_execute_streaming()
            && !app.in_logs_streaming()
            && !app.in_dialogue()
            && !app.in_decomposition_streaming()
            && !app.in_pipeline_streaming()
        {
            poll_daemon_events(app, client).await;
        }

        // Poll for input events (16ms ≈ 60fps)
        let evt = event::poll_event(Duration::from_millis(16))
            .map_err(|e| TuiError::Terminal(format!("failed to poll events: {}", e)))?;

        if let Some(Event::Key(key)) = evt {
            let (cont, action) = event::handle_key_event(app, key);

            // Handle view actions
            match action {
                event::ViewAction::SpecNew(name) => {
                    handle_spec_new(app, client, &name).await;
                }
                event::ViewAction::SpecResume => {
                    handle_spec_resume(app, client).await;
                }
                event::ViewAction::DialogueSendAnswer(text) => {
                    handle_dialogue_send_answer(app, client, &text).await;
                }
                event::ViewAction::DialogueEndSession => {
                    deactivate_spec_session(app, client).await;
                    handle_dialogue_end(app);
                }
                event::ViewAction::DialogueExit => {
                    deactivate_spec_session(app, client).await;
                    app.exit_dialogue();
                    // Refresh specs list after dialogue
                    app.fetch_specs(client).await.ok();
                }
                event::ViewAction::SpecApprove => {
                    handle_spec_approve_action(app, client).await;
                }
                event::ViewAction::SpecDelete => {
                    handle_spec_delete_action(app, client).await;
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
                event::ViewAction::ExecuteRun => {
                    handle_execute_run(app, client).await;
                }
                event::ViewAction::ExecuteStopStory(story_id) => {
                    handle_execute_stop_story(app, client, &story_id).await;
                }
                event::ViewAction::ExecuteCancelStory(story_id) => {
                    handle_execute_cancel_story(app, client, &story_id).await;
                }
                event::ViewAction::ExecuteEscalate(story_id) => {
                    handle_execute_escalate(app, client, &story_id).await;
                }
                event::ViewAction::ExecuteConfirmExit => {
                    // Confirmation popup was dismissed — no action needed
                }
                event::ViewAction::LogsSelectTask(task_id) => {
                    handle_logs_select_task(app, client, &task_id).await;
                }
                event::ViewAction::LogsExit => {
                    app.exit_logs_view();
                }
                event::ViewAction::ProjectSwitcherOpen => {
                    handle_project_switcher_open(app, client).await;
                }
                event::ViewAction::ProjectSelect(name) => {
                    app.switch_project(name, client).await;
                }
                event::ViewAction::PipelineNew { name, goal } => {
                    handle_pipeline_start(app, client, &name, &goal).await;
                }
                event::ViewAction::PipelineCancel(id) => {
                    handle_pipeline_cancel(app, client, &id).await;
                }
                event::ViewAction::PipelineViewDetail(id) => {
                    handle_pipeline_view_detail(app, client, &id).await;
                }
                event::ViewAction::PipelineSelectStage => {
                    // Just refresh detail display -- no daemon call needed
                }
                event::ViewAction::PipelineSubViewExit => {
                    // Clear streaming state (Issue 1 fix)
                    app.pipeline_streaming_request_id = None;
                    // Refresh list
                    fetch_pipeline_list(app, client).await;
                }
                event::ViewAction::TabSwitched(view) => match view {
                    View::Execute | View::Logs => {
                        app.fetch_execute(client).await.ok();
                    }
                    View::Plan => {
                        app.fetch_plan(client).await.ok();
                    }
                    View::Specs => {
                        app.fetch_specs(client).await.ok();
                    }
                    View::Pipeline => {
                        fetch_pipeline_list(app, client).await;
                    }
                },
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

/// Handle opening the project switcher: fetch projects then open overlay.
async fn handle_project_switcher_open(app: &mut App, client: &mut SocketClient) {
    match app.fetch_projects(client).await {
        Ok(projects) => {
            app.open_project_switcher(projects);
        }
        Err(e) => {
            app.status_message = format!("Failed to load projects: {}", e);
        }
    }
}

/// Poll for streaming data from the daemon (non-blocking).
async fn poll_streaming_data(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            // Check if this is a daemon event rather than dialogue data
            if line.event.is_some() {
                handle_daemon_event(app, client, &line).await;
                return;
            }

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
async fn handle_spec_new(app: &mut App, client: &mut SocketClient, spec_name: &str) {
    app.enter_dialogue(spec_name.to_string());

    let params = serde_json::json!({
        "project_name": &app.project,
        "spec_name": spec_name,
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

/// Notify daemon to deactivate the spec session (set session_active=false).
/// Called when user exits dialogue via Esc or Ctrl+D.
async fn deactivate_spec_session(app: &App, client: &mut SocketClient) {
    let spec_name = match &app.spec_dialogue {
        Some(d) => d.spec_name.clone(),
        None => return,
    };

    let _ = client
        .send_command(
            "spec.deactivate",
            serde_json::json!({
                "project_name": &app.project,
                "spec_name": &spec_name,
            }),
        )
        .await;
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

    match client.send_streaming_command("plan.generate", params).await {
        Ok(request_id) => {
            app.status_message = "Plan generation started".to_string();
            app.decomposition_phase = DecompositionPhase::Starting;
            app.streaming_preview_dismissed = false;
            app.decomposition_last_event_at = Some(Instant::now());
            app.decomposition_output.start_streaming(request_id);
            app.plan_tree.clear();
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

    match client.send_streaming_command("plan.feedback", params).await {
        Ok(request_id) => {
            app.status_message = "Feedback sent".to_string();
            app.decomposition_phase = DecompositionPhase::Starting;
            app.streaming_preview_dismissed = false;
            app.decomposition_last_event_at = Some(Instant::now());
            app.decomposition_output.start_streaming(request_id);
            app.plan_tree.clear();
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

/// Handle starting/resuming execution.
async fn handle_execute_run(app: &mut App, client: &mut SocketClient) {
    let params = serde_json::json!({
        "project_name": &app.project,
    });

    match client.send_command("exec.run", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            let running = resp
                .data
                .get("running_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let pending = resp
                .data
                .get("pending_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            app.status_message = format!(
                "Execution started ({} running, {} pending)",
                running, pending
            );
            // Refresh execute tree
            app.fetch_execute(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to start execution");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle stopping a story.
async fn handle_execute_stop_story(app: &mut App, client: &mut SocketClient, story_id: &str) {
    let params = serde_json::json!({
        "project_name": &app.project,
        "story_id": story_id,
    });

    match client.send_command("exec.stop", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = format!("Story {} stopped", story_id);
            // Refresh execute tree
            app.fetch_execute(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to stop story");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle cancelling a story.
async fn handle_execute_cancel_story(app: &mut App, client: &mut SocketClient, story_id: &str) {
    let params = serde_json::json!({
        "project_name": &app.project,
        "story_id": story_id,
    });

    match client.send_command("exec.cancel", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = format!("Story {} cancelled", story_id);
            // Refresh execute tree
            app.fetch_execute(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to cancel story");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle escalating a story: stop it and display the worktree path for manual intervention.
async fn handle_execute_escalate(app: &mut App, client: &mut SocketClient, story_id: &str) {
    // First stop the story
    let params = serde_json::json!({
        "project_name": &app.project,
        "story_id": story_id,
    });

    match client.send_command("exec.stop", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            // Extract worktree path from response if available
            let worktree = resp
                .data
                .get("worktree_path")
                .and_then(|v| v.as_str())
                .unwrap_or("(worktree path not available)");
            app.status_message = format!("Story {} escalated — worktree: {}", story_id, worktree);
            // Refresh execute tree
            app.fetch_execute(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to escalate story");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Format structured stage output from output_result JSON.
/// Parses StageOutput and formats it as readable text.
fn format_stage_output(_stage_type: &str, output_json: &str) -> Vec<String> {
    let mut lines = Vec::new();

    if let Ok(output) = serde_json::from_str::<serde_json::Value>(output_json) {
        // Summary
        if let Some(summary) = output.get("summary").and_then(|v| v.as_str()) {
            lines.push(format!("Summary: {}", summary));
            lines.push(String::new());
        }

        // Plan details
        if let Some(plan) = output.get("plan") {
            if let Some(rationale) = plan.get("rationale").and_then(|v| v.as_str()) {
                lines.push(format!("Rationale: {}", rationale));
                lines.push(String::new());
            }
            if let Some(steps) = plan.get("steps").and_then(|v| v.as_array()) {
                lines.push("Steps:".to_string());
                for (i, step) in steps.iter().enumerate() {
                    if let Some(desc) = step.get("description").and_then(|v| v.as_str()) {
                        lines.push(format!("  {}. {}", i + 1, desc));
                    }
                }
                lines.push(String::new());
            }
            if let Some(files) = plan.get("files_to_modify").and_then(|v| v.as_array()) {
                lines.push("Files to modify:".to_string());
                for f in files {
                    if let Some(path) = f.as_str() {
                        lines.push(format!("  - {}", path));
                    }
                }
                lines.push(String::new());
            }
        }

        // Implementation details
        if let Some(impl_data) = output.get("implementation") {
            if let Some(summary) = impl_data.get("changes_summary").and_then(|v| v.as_str()) {
                lines.push(format!("Changes: {}", summary));
                lines.push(String::new());
            }
            if let Some(files) = impl_data.get("files_changed").and_then(|v| v.as_array()) {
                lines.push("Files changed:".to_string());
                for f in files {
                    if let Some(path) = f.as_str() {
                        lines.push(format!("  - {}", path));
                    }
                }
                lines.push(String::new());
            }
            if let Some(tests_passed) = impl_data.get("tests_passed").and_then(|v| v.as_bool()) {
                lines.push(format!("Tests: {}", if tests_passed { "PASSED" } else { "FAILED" }));
                lines.push(String::new());
            }
        }

        // Review details
        if let Some(review) = output.get("review") {
            if let Some(passed) = review.get("passed").and_then(|v| v.as_bool()) {
                lines.push(format!("Review: {}", if passed { "PASSED" } else { "FAILED" }));
            }
            if let Some(feedback) = review.get("feedback").and_then(|v| v.as_str()) {
                lines.push(format!("Feedback: {}", feedback));
                lines.push(String::new());
            }
            if let Some(issues) = review.get("issues").and_then(|v| v.as_array()) {
                if !issues.is_empty() {
                    lines.push("Issues:".to_string());
                    for issue in issues {
                        let severity = issue.get("severity").and_then(|v| v.as_str()).unwrap_or("?");
                        let desc = issue.get("description").and_then(|v| v.as_str()).unwrap_or("");
                        lines.push(format!("  [{}] {}", severity, desc));
                        if let Some(file) = issue.get("file_path").and_then(|v| v.as_str()) {
                            lines.push(format!("      File: {}", file));
                        }
                    }
                    lines.push(String::new());
                }
            }
        }
    }

    lines
}

/// Clean pipeline log content by removing internal markers.
/// Handles [text], [tool_use], [tool_result], [result] prefixes.
fn clean_pipeline_log(content: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current_text = String::new();

    for line in content.lines() {
        if let Some(text) = line.strip_prefix("[text] ") {
            // Accumulate text content (concatenate consecutive text lines)
            if !current_text.is_empty() {
                current_text.push(' ');
            }
            current_text.push_str(text);
        } else if line == "[text]" {
            // Empty text delta, skip
        } else {
            // Flush accumulated text before processing other line types
            if !current_text.is_empty() {
                lines.push(current_text.clone());
                current_text.clear();
            }
            // Handle other line types
            if let Some(tool) = line.strip_prefix("[tool_use] ") {
                // Show tool usage as a visual marker
                lines.push(format!(">>> {}", tool));
            } else if let Some(result) = line.strip_prefix("[tool_result] ") {
                // Show abbreviated tool result
                if result.len() > 100 {
                    lines.push(format!("  Result: {}...", &result[..100]));
                } else {
                    lines.push(format!("  Result: {}", result));
                }
            } else if line.starts_with("[result]") {
                // Skip internal result markers (too verbose)
            } else if !line.is_empty() {
                // Keep other lines as-is
                lines.push(line.to_string());
            }
        }
    }
    // Flush remaining accumulated text
    if !current_text.is_empty() {
        lines.push(current_text);
    }
    lines
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
            // Check if this is a daemon event (from subscription) rather than streaming data
            if line.event.is_some() {
                handle_daemon_event(app, client, &line).await;
                return;
            }

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

/// Handle selecting a task in the logs view to display its full-screen log.
async fn handle_logs_select_task(app: &mut App, client: &mut SocketClient, task_id: &str) {
    let is_running = app.execute_tree.selected_task_status().as_deref() == Some("in_progress");
    let task_status = app
        .execute_tree
        .selected_task_status()
        .unwrap_or_else(|| "unknown".to_string());

    let params = serde_json::json!({
        "project_name": &app.project,
        "task_id": task_id,
        "follow": is_running,
    });

    if is_running {
        // Start streaming for live output
        match client.send_streaming_command("exec.log", params).await {
            Ok(request_id) => {
                app.logs_view
                    .start_streaming(task_id.to_string(), request_id);
            }
            Err(e) => {
                app.logs_view.set_historical(
                    task_id.to_string(),
                    task_status,
                    vec![format!("[Failed to start streaming: {}]", e)],
                );
            }
        }
    } else {
        // Fetch historical log
        match client.send_command("exec.log", params).await {
            Ok(resp) if resp.status == ResponseStatus::Ok => {
                let lines = parse_log_lines(&resp.data);
                app.logs_view
                    .set_historical(task_id.to_string(), task_status, lines);
            }
            Ok(resp) => {
                let msg = resp
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Failed to fetch log");
                app.logs_view.set_historical(
                    task_id.to_string(),
                    task_status,
                    vec![msg.to_string()],
                );
            }
            Err(e) => {
                app.logs_view.set_historical(
                    task_id.to_string(),
                    task_status,
                    vec![format!("[Error: {}]", e)],
                );
            }
        }
    }
}

/// Poll for streaming logs data in the full-screen logs view (non-blocking).
async fn poll_logs_streaming_data(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = std::time::Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            // Check if this is a daemon event (from subscription) rather than streaming data
            if line.event.is_some() {
                handle_daemon_event(app, client, &line).await;
                return;
            }

            if line.status == ResponseStatus::Error {
                let msg = line
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                app.logs_view.append_line(format!("[Error: {}]", msg));
                app.logs_view.stop_streaming();
                return;
            }

            if line.done {
                app.logs_view.stop_streaming();
                return;
            }

            // Append text content from streaming events
            if let Some(text) = line.data.get("text").and_then(|v| v.as_str()) {
                app.logs_view.append_line(text.to_string());
            }
        }
        Ok(None) => {
            // Timeout — no data yet
        }
        Err(_) => {
            app.logs_view.append_line("[Connection lost]".to_string());
            app.logs_view.stop_streaming();
        }
    }
}

/// Poll for streaming decomposition data (plan generation/feedback) (non-blocking).
async fn poll_decomposition_streaming_data(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            // Check if this is a daemon event (from subscription) rather than streaming data
            if line.event.is_some() {
                handle_daemon_event(app, client, &line).await;
                return;
            }

            if line.status == ResponseStatus::Error {
                let msg = line
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                app.decomposition_output
                    .append_line(format!("[Error: {}]", msg));
                app.decomposition_output.stop_streaming();
                app.decomposition_phase = DecompositionPhase::Failed(msg.to_string());
                app.completion_shown_at = Some(Instant::now());
                app.viewing_decomposition_output = false;
                return;
            }

            if line.done {
                app.decomposition_output.stop_streaming();
                app.viewing_decomposition_output = false;
                app.fetch_plan(client).await.ok();
                let summary = app.count_plan_items();
                app.decomposition_phase = DecompositionPhase::Completed(summary);
                app.completion_shown_at = Some(Instant::now());
                app.decomposition_last_event_at = None;
                app.status_message = "Plan generation complete".to_string();
                return;
            }

            // Update last event timestamp
            app.decomposition_last_event_at = Some(Instant::now());

            // Detect phase transitions from the event type BEFORE delegating to append logic
            let event_type = line.data.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "text" if app.decomposition_phase == DecompositionPhase::Starting => {
                    app.decomposition_phase = DecompositionPhase::Analyzing;
                }
                "tool_use" | "input_json_delta"
                    if matches!(
                        app.decomposition_phase,
                        DecompositionPhase::Starting | DecompositionPhase::Analyzing
                    ) =>
                {
                    app.decomposition_phase = DecompositionPhase::BuildingItems;
                }
                _ => {}
            }

            // Append the data as a decomposition event
            app.decomposition_output
                .append_decomposition_event(&line.data);
        }
        Ok(None) => {
            // Timeout — no data yet
        }
        Err(_) => {
            app.decomposition_output
                .append_line("[Connection lost]".to_string());
            app.decomposition_output.stop_streaming();
            app.decomposition_phase = DecompositionPhase::Failed("Connection lost".to_string());
            app.completion_shown_at = Some(Instant::now());
            app.viewing_decomposition_output = false;
        }
    }
}

/// Poll for daemon events when not actively streaming (non-blocking).
async fn poll_daemon_events(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = std::time::Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            if line.event.is_some() {
                handle_daemon_event(app, client, &line).await;
            }
            // Non-event lines while not streaming are unexpected; ignore them.
        }
        Ok(None) => {
            // Timeout — no events available
        }
        Err(_) => {
            // Connection error — mark as disconnected
            app.event_subscription.subscribed = false;
        }
    }
}

/// Handle a daemon event received via the event subscription.
async fn handle_daemon_event(
    app: &mut App,
    client: &mut SocketClient,
    line: &socket_client::StreamingResponseLine,
) {
    let event = match &line.event {
        Some(e) => e,
        None => return,
    };

    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "status_change" => {
            let new_status = event
                .get("new_status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let item_type = event
                .get("item_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            app.apply_status_change("", new_status, item_type);

            // Refresh the execute tree to pick up the status change
            app.fetch_execute(client).await.ok();
        }
        "agent_output" => {
            let task_id = event.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let output_line = event.get("line").and_then(|v| v.as_str()).unwrap_or("");

            app.apply_agent_output(task_id, output_line);
        }
        "story_completed" => {
            let story_id = event.get("story_id").and_then(|v| v.as_str()).unwrap_or("");
            let mr_url = event.get("mr_url").and_then(|v| v.as_str());

            app.apply_story_completed(story_id, mr_url);

            // Refresh the execute tree to pick up completion status
            app.fetch_execute(client).await.ok();
        }
        "decomposition_completed" => {
            app.decomposition_output.stop_streaming();
            app.viewing_decomposition_output = false;
            app.fetch_plan(client).await.ok();
            let summary = app.count_plan_items();
            app.decomposition_phase = DecompositionPhase::Completed(summary);
            app.completion_shown_at = Some(Instant::now());
            app.decomposition_last_event_at = None;
            app.status_message = "Plan generation complete".to_string();
            app.fetch_execute(client).await.ok();
        }
        "pipeline_stage_change" => {
            if let Some(run_id) = event.get("pipeline_run_id").and_then(|v| v.as_str()) {
                let stage_type = event
                    .get("stage_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let iteration =
                    event.get("iteration").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let new_status = event
                    .get("new_status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // Update list view
                if let Some(item) = app.pipeline_list.runs.iter_mut().find(|r| r.id == run_id) {
                    item.current_stage = Some(stage_type.to_string());
                    item.iteration = iteration;
                    if new_status == "running" {
                        item.status = "running".to_string();
                    }
                }

                // Update detail view if open
                if let Some(detail) = &mut app.pipeline_detail {
                    if run_id == detail.run.id {
                        // Update run metadata
                        detail.run.current_stage = Some(stage_type.to_string());
                        detail.run.iteration = iteration;

                        // Update existing stage or add new one
                        let stage_exists = detail.stages.iter_mut().find(|s|
                            s.stage_type == stage_type && s.iteration == iteration
                        );
                        if let Some(existing) = stage_exists {
                            existing.status = new_status.to_string();
                        } else {
                            detail.stages.push(PipelineStageItem {
                                stage_type: stage_type.to_string(),
                                iteration,
                                status: new_status.to_string(),
                            });
                        }
                        detail.append_stage_output(
                            stage_type,
                            iteration,
                            format!("--- {} (iter {}) -> {} ---", stage_type, iteration, new_status)
                        );
                    }
                }
            }
        }
        "pipeline_completed" => {
            if let Some(run_id) = event.get("pipeline_run_id").and_then(|v| v.as_str()) {
                let final_status = event
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("completed");

                // Update list view
                if let Some(item) = app.pipeline_list.runs.iter_mut().find(|r| r.id == run_id) {
                    item.status = final_status.to_string();
                }

                // Update detail view if open
                if let Some(detail) = &mut app.pipeline_detail {
                    if run_id == detail.run.id {
                        detail.run.status = final_status.to_string();
                        detail.is_streaming = false;
                        // Append completion message to the last stage
                        if let Some(last_stage) = detail.stages.last().cloned() {
                            detail.append_stage_output(
                                &last_stage.stage_type,
                                last_stage.iteration,
                                format!("\n=== Pipeline {} ===\n", final_status)
                            );
                        }
                    }
                }
            }

            app.status_message = "Pipeline completed".to_string();
            fetch_pipeline_list(app, client).await;
        }
        "pipeline_agent_output" => {
            if let Some(detail) = &mut app.pipeline_detail {
                if let Some(run_id) = event.get("pipeline_run_id").and_then(|v| v.as_str()) {
                    if run_id == detail.run.id {
                        let stage_type = event.get("stage_type").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let iteration = event.get("iteration").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                        if let Some(line) = event.get("line").and_then(|v| v.as_str()) {
                            detail.append_stage_output(stage_type, iteration, line.to_string());
                            // Push to ring buffer (cap at 1000)
                            if app.pipeline_output_buffer.len() >= 1000 {
                                app.pipeline_output_buffer.pop_front();
                            }
                            app.pipeline_output_buffer.push_back(line.to_string());
                        }
                    }
                }
            }
        }
        _ => {
            // Unknown event type -- ignore
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

/// Handle approving the selected spec.
async fn handle_spec_approve_action(app: &mut App, client: &mut SocketClient) {
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

    match client.send_command("spec.approve", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = format!("Spec '{}' approved", spec_name);
            app.fetch_specs(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to approve spec");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Handle deleting the selected spec.
async fn handle_spec_delete_action(app: &mut App, client: &mut SocketClient) {
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

    match client.send_command("spec.delete", params).await {
        Ok(resp) if resp.status == ResponseStatus::Ok => {
            app.status_message = format!("Spec '{}' deleted", spec_name);
            app.fetch_specs(client).await.ok();
        }
        Ok(resp) => {
            let msg = resp
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to delete spec");
            app.status_message = msg.to_string();
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Start a new pipeline run (streaming).
async fn handle_pipeline_start(app: &mut App, client: &mut SocketClient, name: &str, goal: &str) {
    let params = serde_json::json!({
        "project_name": &app.project,
        "name": name,
        "goal": goal,
    });

    match client
        .send_streaming_command("pipeline.start", params)
        .await
    {
        Ok(request_id) => {
            app.status_message = "Pipeline started".to_string();
            app.pipeline_streaming_request_id = Some(request_id);
            app.pipeline_new = None;
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Cancel a running pipeline.
async fn handle_pipeline_cancel(app: &mut App, client: &mut SocketClient, id: &str) {
    let params = serde_json::json!({
        "pipeline_run_id": id,
    });

    match client.send_command("pipeline.cancel", params).await {
        Ok(resp) => {
            if resp.status == ResponseStatus::Ok {
                app.status_message = "Pipeline cancelled".to_string();
                fetch_pipeline_list(app, client).await;
            } else {
                let msg = resp
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                app.status_message = format!("Error: {}", msg);
            }
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// View details of a pipeline run.
async fn handle_pipeline_view_detail(app: &mut App, client: &mut SocketClient, id: &str) {
    let params = serde_json::json!({
        "pipeline_run_id": id,
    });

    match client.send_command("pipeline.status", params).await {
        Ok(resp) => {
            if resp.status == ResponseStatus::Ok {
                // Parse run info
                if let Some(run_data) = resp.data.get("run") {
                    let run_item = PipelineRunItem {
                        id: run_data
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: run_data
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        status: run_data
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        current_stage: run_data
                            .get("current_stage")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        iteration: run_data
                            .get("iteration")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32,
                        max_iterations: run_data
                            .get("max_iterations")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(5) as u32,
                        created_at: run_data
                            .get("created_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    };
                    let mut detail = PipelineDetailState::new(run_item);

                    // Set is_streaming based on run status
                    detail.is_streaming = detail.run.status == "running";

                    // Parse stages and populate output from output_result if available
                    if let Some(stages) = resp.data.get("stages").and_then(|v| v.as_array()) {
                        for stage_data in stages {
                            let stage_type = stage_data
                                .get("stage_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let iteration = stage_data
                                .get("iteration")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32;
                            let status = stage_data
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            detail.stages.push(PipelineStageItem {
                                stage_type: stage_type.clone(),
                                iteration,
                                status: status.clone(),
                            });

                            // If output_result is available, use it to populate stage output
                            if let Some(output_str) = stage_data
                                .get("output_result")
                                .and_then(|v| v.as_str())
                            {
                                if !output_str.is_empty() {
                                    // Add stage header
                                    detail.append_stage_output(
                                        &stage_type,
                                        iteration,
                                        format!("\n=== Stage: {} (iteration {}) ===\n", stage_type, iteration)
                                    );

                                    // Format and add structured output
                                    let formatted = format_stage_output(&stage_type, output_str);
                                    for line in formatted {
                                        detail.append_stage_output(&stage_type, iteration, line);
                                    }
                                }
                            }
                        }
                    }

                    // Load historical logs for completed pipelines (fallback for stages without output_result)
                    if !detail.is_streaming {
                        let log_params = serde_json::json!({
                            "pipeline_run_id": id,
                        });

                        if let Ok(log_resp) = client.send_command("pipeline.log", log_params).await {
                            if log_resp.status == ResponseStatus::Ok {
                                if let Some(logs) = log_resp.data.get("logs").and_then(|v| v.as_array()) {
                                    for log_entry in logs {
                                        let stage_type = log_entry
                                            .get("stage_type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let iteration = log_entry
                                            .get("iteration")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0) as u32;

                                        // Only use logs if output_result wasn't already populated for this stage
                                        let stage_key = format!("{}_{}", stage_type, iteration);
                                        if !detail.stage_outputs.contains_key(&stage_key) {
                                            let content = log_entry
                                                .get("content")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");

                                            // Add stage header
                                            detail.append_stage_output(
                                                stage_type,
                                                iteration,
                                                format!("\n=== Stage: {} (iteration {}) ===\n", stage_type, iteration)
                                            );

                                            // Add log content (cleaned to remove internal markers)
                                            if !content.is_empty() {
                                                let cleaned_lines = clean_pipeline_log(content);
                                                for line in cleaned_lines {
                                                    detail.append_stage_output(stage_type, iteration, line);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    app.pipeline_detail = Some(detail);
                    // Clear output buffer when switching pipeline runs
                    app.pipeline_output_buffer.clear();
                    app.pipeline_output_scroll = 0;
                    app.pipeline_auto_scroll = true;
                }
            }
        }
        Err(e) => {
            app.status_message = format!("Error: {}", e);
        }
    }
}

/// Fetch the pipeline list from the daemon.
async fn fetch_pipeline_list(app: &mut App, client: &mut SocketClient) {
    let params = serde_json::json!({
        "project_name": &app.project,
    });

    if let Ok(resp) = client.send_command("pipeline.list", params).await {
        if resp.status == ResponseStatus::Ok {
            if let Some(runs) = resp.data.get("runs").and_then(|v| v.as_array()) {
                app.pipeline_list.runs = runs
                    .iter()
                    .map(|r| PipelineRunItem {
                        id: r
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: r
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        status: r
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        current_stage: r
                            .get("current_stage")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        iteration: r.get("iteration").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                        max_iterations: r
                            .get("max_iterations")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(5) as u32,
                        created_at: r
                            .get("created_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect();
            }
        }
    }
}

/// Poll for pipeline streaming data (non-blocking).
async fn poll_pipeline_streaming_data(app: &mut App, client: &mut SocketClient) {
    let poll_timeout = Duration::from_millis(10);

    match client.try_read_streaming_line(poll_timeout).await {
        Ok(Some(line)) => {
            // Check if this is a daemon event
            if line.event.is_some() {
                handle_daemon_event(app, client, &line).await;
                return;
            }

            if line.done {
                app.pipeline_streaming_request_id = None;
                app.status_message = "Pipeline completed".to_string();
                fetch_pipeline_list(app, client).await;
                // Stop streaming in detail view
                if let Some(detail) = &mut app.pipeline_detail {
                    detail.is_streaming = false;
                }
                return;
            }

            if line.status == ResponseStatus::Error {
                let msg = line
                    .data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                app.status_message = format!("Pipeline error: {}", msg);
                app.pipeline_streaming_request_id = None;
                return;
            }

            // Check if this is the first streaming response with pipeline_run_id
            if app.pipeline_detail.is_none() {
                if let Some(pipeline_run_id) = line.data.get("pipeline_run_id").and_then(|v| v.as_str()) {
                    // Create detail state from first streaming response
                    let run_item = PipelineRunItem {
                        id: pipeline_run_id.to_string(),
                        name: line.data
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Pipeline")
                            .to_string(),
                        status: line.data
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("running")
                            .to_string(),
                        current_stage: None,
                        iteration: 0,
                        max_iterations: line.data
                            .get("max_iterations")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(5) as u32,
                        created_at: String::new(),
                    };

                    // Add to list view if not already present
                    if !app.pipeline_list.runs.iter().any(|r| r.id == pipeline_run_id) {
                        app.pipeline_list.runs.insert(0, run_item.clone());
                    }

                    let mut detail = PipelineDetailState::new(run_item);
                    detail.is_streaming = true;
                    app.pipeline_detail = Some(detail);
                    // Clear output buffer for new streaming pipeline
                    app.pipeline_output_buffer.clear();
                    app.pipeline_output_scroll = 0;
                    app.pipeline_auto_scroll = true;
                }
            }

            // Append output to detail view if open
            if let Some(detail) = &mut app.pipeline_detail {
                // Handle different event types from streaming data
                let event_type = line.data.get("type").and_then(|v| v.as_str());
                match event_type {
                    Some("text") => {
                        if let Some(text) = line.data.get("text").and_then(|v| v.as_str()) {
                            // Append to current streaming stage if known
                            if let Some((stage_type, iteration)) = detail.current_streaming_stage.clone() {
                                detail.append_stage_output(&stage_type, iteration, text.to_string());
                            }
                            // Push to ring buffer
                            if app.pipeline_output_buffer.len() >= 1000 {
                                app.pipeline_output_buffer.pop_front();
                            }
                            app.pipeline_output_buffer.push_back(text.to_string());
                        }
                    }
                    Some("stage_start") => {
                        let stage = line.data.get("stage_type").and_then(|v| v.as_str()).unwrap_or("?");
                        let iter = line.data.get("iteration").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        // Track current stage
                        detail.current_streaming_stage = Some((stage.to_string(), iter));
                        detail.append_stage_output(
                            stage,
                            iter,
                            format!("\n=== Stage: {} (iteration {}) ===\n", stage, iter)
                        );
                    }
                    Some("stage_complete") => {
                        let stage = line.data.get("stage_type").and_then(|v| v.as_str()).unwrap_or("?");
                        if let Some((_, iteration)) = &detail.current_streaming_stage {
                            detail.append_stage_output(
                                stage,
                                *iteration,
                                format!("=== {} completed ===\n", stage)
                            );
                        }
                    }
                    _ => {
                        // For other data types (or no type), try to append as text
                        if let Some(text) = line.data.get("text").and_then(|v| v.as_str()) {
                            // Append to current streaming stage if known
                            if let Some((stage_type, iteration)) = detail.current_streaming_stage.clone() {
                                detail.append_stage_output(&stage_type, iteration, text.to_string());
                            }
                        }
                    }
                }
            }
        }
        Ok(None) => {}
        Err(_) => {
            app.pipeline_streaming_request_id = None;
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
