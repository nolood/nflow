use std::path::{Path, PathBuf};
use std::sync::Arc;

use nflow_core::decomposition::{DecompositionSession, DecompositionSpec, DecompositionStatus};
use nflow_core::project::{CreateProjectParams, GitProvider, ProjectService};
use nflow_core::spec::{Spec, SpecStatus};
use nflow_core::work_item::{
    auto_generate_verify_tasks, Dependency, ItemType, TaskKind, WorkItem, WorkItemStatus,
};

use crate::db;
use crate::socket::{HandlerResult, Request, Response, StreamingResponseLine};

/// Helper to convert GitProvider to string for JSON response.
fn git_provider_str(provider: GitProvider) -> &'static str {
    match provider {
        GitProvider::Github => "github",
        GitProvider::Gitlab => "gitlab",
    }
}

/// Shared state available to all command handlers.
pub struct HandlerState {
    /// Path to the SQLite database file.
    pub db_path: PathBuf,
}

/// Creates the main command handler that dispatches to specific handlers.
pub fn create_handler(state: Arc<HandlerState>) -> crate::socket::CommandHandler {
    Box::new(move |req: Request| {
        let state = Arc::clone(&state);
        dispatch(req, &state)
    })
}

/// Dispatch a request to the appropriate command handler.
fn dispatch(req: Request, state: &HandlerState) -> HandlerResult {
    match req.command.as_str() {
        "project.init" => HandlerResult::Single(handle_project_init(req, state)),
        "project.list" => HandlerResult::Single(handle_project_list(req, state)),
        "project.delete" => HandlerResult::Single(handle_project_delete(req, state)),
        "spec.new" => handle_spec_new(req, state),
        "spec.answer" => handle_spec_answer(req, state),
        "spec.list" => HandlerResult::Single(handle_spec_list(req, state)),
        "spec.view" => HandlerResult::Single(handle_spec_view(req, state)),
        "spec.approve" => HandlerResult::Single(handle_spec_approve(req, state)),
        "spec.reopen" => HandlerResult::Single(handle_spec_reopen(req, state)),
        "spec.delete" => HandlerResult::Single(handle_spec_delete(req, state)),
        "spec.resume" => handle_spec_resume(req, state),
        "plan.generate" => handle_plan_generate(req, state),
        "plan.show" => HandlerResult::Single(handle_plan_show(req, state)),
        "plan.feedback" => handle_plan_feedback(req, state),
        "plan.approve" => HandlerResult::Single(handle_plan_approve(req, state)),
        "plan.discard" => HandlerResult::Single(handle_plan_discard(req, state)),
        "exec.run" => HandlerResult::Single(handle_exec_run(req, state)),
        "exec.pause" => HandlerResult::Single(handle_exec_pause(req, state)),
        "exec.status" => HandlerResult::Single(handle_exec_status(req, state)),
        "exec.retry" => HandlerResult::Single(handle_exec_retry(req, state)),
        "exec.skip" => HandlerResult::Single(handle_exec_skip(req, state)),
        "exec.continue" => HandlerResult::Single(handle_exec_continue(req, state)),
        "exec.stop" => HandlerResult::Single(handle_exec_stop(req, state)),
        "exec.cancel" => HandlerResult::Single(handle_exec_cancel(req, state)),
        _ => HandlerResult::Single(Response::error(
            req.id,
            &format!("unknown command: {}", req.command),
        )),
    }
}

/// Handle "project.init" command.
///
/// Receives: { name, path, base_branch?, git_provider? }
/// Returns: { project_id, name, path, base_branch, git_provider }
fn handle_project_init(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    // Parse required params
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: name");
        }
    };

    let path = match req.params.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => {
            return Response::error(id, "missing required parameter: path");
        }
    };

    // Validate the path is a git repo
    if !is_git_repo(Path::new(&path)) {
        return Response::error(
            id,
            &format!("INVALID_PARAMS: path '{}' is not a git repository", path),
        );
    }

    // Parse optional params
    let base_branch = req
        .params
        .get("base_branch")
        .and_then(|v| v.as_str())
        .map(String::from);

    let git_provider_override = req
        .params
        .get("git_provider")
        .and_then(|v| v.as_str())
        .and_then(parse_git_provider);

    // Get remote URL for provider auto-detection
    let remote_url = get_remote_url(Path::new(&path));

    // Get HEAD ref for base branch detection
    let head_ref = base_branch.or_else(|| get_head_ref(Path::new(&path)));

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Check name uniqueness via DB
    let name_exists = match db::projects::project_name_exists(&conn, &name) {
        Ok(exists) => exists,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    if name_exists {
        return Response::error(
            id,
            &format!(
                "ALREADY_EXISTS: project with name '{}' already exists",
                name
            ),
        );
    }

    // Use ProjectService::create() for validation
    let params = CreateProjectParams {
        name: name.clone(),
        path: path.clone(),
        remote_url,
        git_provider_override,
        head_ref,
    };

    let project = match ProjectService::create(params, |n| {
        // We already checked — but the service requires a checker
        n == name && name_exists
    }) {
        Ok(p) => p,
        Err(e) => return Response::error(id, &format!("{}", e)),
    };

    // Create project directories
    if let Err(e) = create_project_dirs(&name) {
        return Response::error(id, &format!("failed to create project directories: {}", e));
    }

    // Insert into SQLite
    if let Err(e) = db::projects::insert_project(&conn, &project) {
        return Response::error(id, &format!("database error: {}", e));
    }

    // Return success
    Response::ok(
        id,
        serde_json::json!({
            "project_id": project.id.to_string(),
            "name": project.name,
            "path": project.path,
            "base_branch": project.base_branch,
            "git_provider": git_provider_str(project.git_provider),
        }),
    )
}

/// Handle "project.list" command.
///
/// Returns all projects with: name, path, base_branch, git_provider, execution_enabled, spec_count, story_count
fn handle_project_list(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // List all projects
    let projects = match db::projects::list_projects(&conn) {
        Ok(p) => p,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Build response with counts for each project
    let mut project_list = Vec::new();
    for project in &projects {
        let spec_count = db::specs::count_specs_by_project(&conn, &project.id).unwrap_or(0);
        let story_count = db::work_items::count_stories_by_project(&conn, &project.id).unwrap_or(0);

        project_list.push(serde_json::json!({
            "name": project.name,
            "path": project.path,
            "base_branch": project.base_branch,
            "git_provider": git_provider_str(project.git_provider),
            "execution_enabled": project.execution_enabled,
            "spec_count": spec_count,
            "story_count": story_count,
        }));
    }

    Response::ok(id, serde_json::json!({ "projects": project_list }))
}

/// Handle "project.delete" command.
///
/// Receives: { name, force? }
/// Without force: returns error if project has running agents.
/// With force: marks in-progress items as cancelled and proceeds.
/// Deletes: project record (cascades), project dir, worktrees.
fn handle_project_delete(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    // Parse required params
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: name");
        }
    };

    let force = req
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Find the project by name
    let project = match db::projects::get_project_by_name(&conn, &name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: project '{}' not found", name));
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Check for running agents
    let running_agents =
        match db::agent_runs::find_running_agent_runs_by_project(&conn, &project.id) {
            Ok(runs) => runs,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

    if !running_agents.is_empty() && !force {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: project '{}' has {} running agent(s). Use force=true to override",
                name,
                running_agents.len()
            ),
        );
    }

    // With force: cancel in-progress items and running agents
    if force && !running_agents.is_empty() {
        if let Err(e) = db::work_items::cancel_in_progress_items_by_project(&conn, &project.id) {
            return Response::error(id, &format!("database error while cancelling items: {}", e));
        }
        if let Err(e) = db::agent_runs::cancel_running_agent_runs_by_project(&conn, &project.id) {
            return Response::error(
                id,
                &format!("database error while cancelling agents: {}", e),
            );
        }
    }

    // Collect worktree paths before deleting DB records
    let worktree_paths =
        db::work_items::list_worktree_paths_by_project(&conn, &project.id).unwrap_or_default();

    // Delete project from DB (cascades to specs, work_items, agent_runs, decomposition_sessions)
    if let Err(e) = db::projects::delete_project(&conn, &project.id) {
        return Response::error(id, &format!("database error: {}", e));
    }

    // Remove worktrees via git
    for worktree_path in &worktree_paths {
        remove_worktree_sync(Path::new(&project.path), Path::new(worktree_path));
    }

    // Remove project directory (~/.nflow/projects/{name}/)
    remove_project_dirs(&name);

    Response::ok(
        id,
        serde_json::json!({
            "name": name,
            "deleted": true,
            "worktrees_removed": worktree_paths.len(),
        }),
    )
}

/// Handle "spec.new" command — start a new spec session.
///
/// Receives: { project_name, spec_name, with_codebase? }
/// Returns streaming: initial { spec_id, name, status: "draft" }, then Claude output lines.
fn handle_spec_new(req: Request, state: &HandlerState) -> HandlerResult {
    let id = req.id.clone();

    // Parse required params
    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: project_name",
            ));
        }
    };

    let spec_name = match req.params.get("spec_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: spec_name",
            ));
        }
    };

    let with_codebase = req
        .params
        .get("with_codebase")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Find project by name
    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            ));
        }
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Validate no active spec session for this project
    match db::specs::has_active_spec_session(&conn, &project.id) {
        Ok(true) => {
            return HandlerResult::Single(Response::error(
                id,
                "INVALID_STATE: a spec session is already active for this project",
            ));
        }
        Ok(false) => {}
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    }

    // Build the spec file path
    let nflow_home = match crate::daemon::nflow_home() {
        Ok(h) => h,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to get nflow home: {}", e),
            ));
        }
    };
    let spec_file_path = nflow_home
        .join("projects")
        .join(&project.name)
        .join("specs")
        .join(format!("{}.md", &spec_name));

    // Create spec record with status=draft, session_active=true
    let mut spec = Spec::new(
        project.id,
        spec_name.clone(),
        spec_file_path.to_string_lossy().to_string(),
    );
    spec.start_session().unwrap(); // safe: session_active starts as false in new()

    // Insert spec into DB
    if let Err(e) = db::specs::insert_spec(&conn, &spec) {
        return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
    }

    // Load and render the spec_session template
    let override_dir = nflow_home.join("prompts");
    let override_path = if override_dir.is_dir() {
        Some(override_dir)
    } else {
        None
    };
    let template =
        match nflow_claude::prompt::load_template("spec_session", override_path.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("failed to load template: {}", e),
                ));
            }
        };

    let context = nflow_claude::context::build_spec_context(&spec, &project);
    let vars: std::collections::HashMap<&str, &str> = context
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let prompt = match nflow_claude::prompt::render_template(&template, &vars) {
        Ok(p) => p,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to render template: {}", e),
            ));
        }
    };

    // Determine working directory
    let working_dir = if with_codebase {
        PathBuf::from(&project.path)
    } else {
        nflow_home
            .join("projects")
            .join(&project.name)
            .join("specs")
    };

    // Build RunConfig for spec session
    let mut run_config = nflow_claude::runner::RunConfig::for_spec(prompt, with_codebase);
    run_config.working_dir = Some(working_dir);
    run_config.system_prompt_file = override_path.map(|d| d.join("spec_session.md"));

    // Create streaming channel and spawn Claude streaming
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let spec_id = spec.id;
    let spec_file_path_str = spec.file_path.clone();
    let db_path = state.db_path.clone();

    // Send initial response with spec info
    let initial_id = id.clone();
    tokio::spawn(async move {
        // Send initial spec info
        let _ = tx
            .send(StreamingResponseLine::data(
                initial_id.clone(),
                serde_json::json!({
                    "spec_id": spec_id.to_string(),
                    "name": spec_name,
                    "status": "draft",
                }),
            ))
            .await;

        stream_claude_spec_session(
            tx,
            initial_id,
            run_config,
            spec_id,
            &spec_file_path_str,
            &db_path,
        )
        .await;
    });

    HandlerResult::Streaming(rx)
}

/// Handle "spec.answer" command — answer a question from spec Q&A flow.
///
/// Receives: { spec_id, answer_text }
/// Spawns new Claude with --resume {session_id} and -p {answer_text}.
fn handle_spec_answer(req: Request, state: &HandlerState) -> HandlerResult {
    let id = req.id.clone();

    // Parse required params
    let spec_id_str = match req.params.get("spec_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: spec_id",
            ));
        }
    };

    let answer_text = match req.params.get("answer_text").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: answer_text",
            ));
        }
    };

    let spec_id = match uuid::Uuid::parse_str(&spec_id_str) {
        Ok(id) => id,
        Err(_) => {
            return HandlerResult::Single(Response::error(
                id,
                "INVALID_PARAMS: invalid spec_id format",
            ));
        }
    };

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Look up the spec
    let spec = match db::specs::get_spec_by_id(&conn, &spec_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("NOT_FOUND: spec '{}' not found", spec_id_str),
            ));
        }
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Validate session is active
    if !spec.session_active {
        return HandlerResult::Single(Response::error(
            id,
            "INVALID_STATE: no active session for this spec",
        ));
    }

    // Must have a claude_session_id to resume
    let claude_session_id = match &spec.claude_session_id {
        Some(sid) => sid.clone(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "INVALID_STATE: spec has no claude session to resume",
            ));
        }
    };

    // Build RunConfig for resumed spec session — uses answer_text as prompt, resumes previous session
    let mut run_config = nflow_claude::runner::RunConfig::for_spec(answer_text, true);
    run_config.resume_session = Some(claude_session_id);

    // Find the project for working_dir
    let project = match db::projects::get_project_by_id(&conn, &spec.project_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HandlerResult::Single(Response::error(
                id,
                "NOT_FOUND: project for this spec not found",
            ));
        }
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };
    run_config.working_dir = Some(PathBuf::from(&project.path));

    // Create streaming channel and spawn Claude streaming
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let spec_file_path_str = spec.file_path.clone();
    let db_path = state.db_path.clone();

    let initial_id = id.clone();
    tokio::spawn(async move {
        stream_claude_spec_session(
            tx,
            initial_id,
            run_config,
            spec_id,
            &spec_file_path_str,
            &db_path,
        )
        .await;
    });

    HandlerResult::Streaming(rx)
}

/// Shared streaming logic for spec sessions (used by both spec.new and spec.answer).
///
/// Spawns Claude, streams output detecting AskUserQuestion events, and handles
/// session completion (exit code 0 + spec file exists).
async fn stream_claude_spec_session(
    tx: tokio::sync::mpsc::Sender<StreamingResponseLine>,
    req_id: String,
    run_config: nflow_claude::runner::RunConfig,
    spec_id: uuid::Uuid,
    spec_file_path: &str,
    db_path: &Path,
) {
    // Spawn Claude process
    let runner = nflow_claude::runner::ClaudeRunner::default();
    let process = match runner.spawn(&run_config) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx
                .send(StreamingResponseLine::error(
                    req_id,
                    &format!("failed to spawn Claude: {}", e),
                ))
                .await;
            // Mark session as inactive on failure
            if let Ok(conn) = db::open_connection(db_path) {
                let _ = db::specs::update_spec_session(&conn, &spec_id, false, None);
            }
            return;
        }
    };

    // Stream Claude output
    let mut parser = nflow_claude::stream::StreamParser::new(process.stdout);
    loop {
        match parser.next_event().await {
            Ok(Some(event)) => {
                // Detect AskUserQuestion tool calls and send as question event
                let event_json = if event.is_ask_user_question() {
                    if let nflow_claude::stream::StreamEvent::ToolUse { input, .. } = &event {
                        let question = input.get("question").and_then(|v| v.as_str()).unwrap_or("");
                        let options = input.get("options");
                        let mut q = serde_json::json!({
                            "type": "question",
                            "text": question,
                        });
                        if let Some(opts) = options {
                            q["options"] = opts.clone();
                        }
                        q
                    } else {
                        serde_json::json!({"type": "tool_use", "name": "AskUserQuestion", "input": {}})
                    }
                } else {
                    match &event {
                        nflow_claude::stream::StreamEvent::TextDelta { text } => {
                            serde_json::json!({"type": "text", "text": text})
                        }
                        nflow_claude::stream::StreamEvent::ToolUse { name, input } => {
                            serde_json::json!({"type": "tool_use", "name": name, "input": input})
                        }
                        nflow_claude::stream::StreamEvent::ToolResult { content } => {
                            serde_json::json!({"type": "tool_result", "content": content})
                        }
                        nflow_claude::stream::StreamEvent::Result { text, session_id } => {
                            serde_json::json!({"type": "result", "text": text, "session_id": session_id})
                        }
                        nflow_claude::stream::StreamEvent::Error { message } => {
                            serde_json::json!({"type": "error", "message": message})
                        }
                        nflow_claude::stream::StreamEvent::ParseError { line, reason } => {
                            serde_json::json!({"type": "parse_error", "line": line, "reason": reason})
                        }
                    }
                };

                if tx
                    .send(StreamingResponseLine::data(req_id.clone(), event_json))
                    .await
                    .is_err()
                {
                    break; // Client disconnected
                }
            }
            Ok(None) => break, // Stream ended
            Err(e) => {
                let _ = tx
                    .send(StreamingResponseLine::error(
                        req_id.clone(),
                        &format!("stream error: {}", e),
                    ))
                    .await;
                break;
            }
        }
    }

    // Get session ID from parser
    let session_id = parser.session_id().map(|s| s.to_string());

    // Check session completion: spec file exists means Claude finished writing the spec
    let spec_file_exists = Path::new(spec_file_path).exists();
    let session_completed = spec_file_exists;

    // Update DB: store claude_session_id; if completed, mark session inactive
    if let Ok(conn) = db::open_connection(db_path) {
        if session_completed {
            let _ = db::specs::update_spec_session(&conn, &spec_id, false, session_id.as_deref());
        } else {
            // Session still active (waiting for user answer), just store the session ID
            let _ = db::specs::update_spec_session(&conn, &spec_id, true, session_id.as_deref());
        }
    }

    // Send done line
    let _ = tx
        .send(StreamingResponseLine::done(
            req_id,
            serde_json::json!({
                "spec_id": spec_id.to_string(),
                "session_id": session_id,
                "completed": session_completed,
            }),
        ))
        .await;
}

/// Handle "spec.list" command.
///
/// Receives: { project_name }
/// Returns: { specs: [{ name, status, session_active, created_at }] }
/// Hidden: specs with status = "deleted" are excluded from listings.
fn handle_spec_list(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: project_name");
        }
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            );
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let specs = match db::specs::list_specs_by_project(&conn, &project.id) {
        Ok(s) => s,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Filter out deleted specs (soft-delete: hidden from listings)
    let spec_list: Vec<serde_json::Value> = specs
        .iter()
        .filter(|s| s.status != SpecStatus::Deleted)
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "status": s.status.to_string(),
                "session_active": s.session_active,
                "created_at": s.created_at.to_rfc3339(),
            })
        })
        .collect();

    Response::ok(id, serde_json::json!({ "specs": spec_list }))
}

/// Handle "spec.view" command.
///
/// Receives: { project_name, spec_name }
/// Returns: { name, status, content } where content is the markdown file contents.
fn handle_spec_view(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: project_name");
        }
    };

    let spec_name = match req.params.get("spec_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: spec_name");
        }
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            );
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let spec = match db::specs::get_spec_by_name(&conn, &project.id, &spec_name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: spec '{}' not found", spec_name));
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Read the spec file from disk
    let content = match std::fs::read_to_string(&spec.file_path) {
        Ok(c) => c,
        Err(e) => {
            return Response::error(
                id,
                &format!("failed to read spec file '{}': {}", spec.file_path, e),
            );
        }
    };

    Response::ok(
        id,
        serde_json::json!({
            "name": spec.name,
            "status": spec.status.to_string(),
            "content": content,
        }),
    )
}

/// Handle "spec.approve" command.
///
/// Receives: { project_name, spec_name }
/// Transitions draft -> approved. If spec has active session, stops it first.
fn handle_spec_approve(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: project_name");
        }
    };

    let spec_name = match req.params.get("spec_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: spec_name");
        }
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            );
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut spec = match db::specs::get_spec_by_name(&conn, &project.id, &spec_name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: spec '{}' not found", spec_name));
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // If spec has active session, stop it first
    if spec.session_active {
        if let Err(e) = db::specs::update_spec_session(
            &conn,
            &spec.id,
            false,
            spec.claude_session_id.as_deref(),
        ) {
            return Response::error(id, &format!("database error: {}", e));
        }
        spec.session_active = false;
    }

    // Validate state machine transition
    if let Err(e) = spec.approve() {
        return Response::error(id, &format!("INVALID_STATE: {}", e));
    }

    // Persist status change
    if let Err(e) = db::specs::update_spec_status(&conn, &spec.id, spec.status) {
        return Response::error(id, &format!("database error: {}", e));
    }

    Response::ok(
        id,
        serde_json::json!({
            "name": spec.name,
            "status": spec.status.to_string(),
        }),
    )
}

/// Handle "spec.reopen" command.
///
/// Receives: { project_name, spec_name }
/// Transitions approved -> draft (validates not decomposed).
fn handle_spec_reopen(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: project_name");
        }
    };

    let spec_name = match req.params.get("spec_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: spec_name");
        }
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            );
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut spec = match db::specs::get_spec_by_name(&conn, &project.id, &spec_name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: spec '{}' not found", spec_name));
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Validate state machine transition
    if let Err(e) = spec.reopen() {
        return Response::error(id, &format!("INVALID_STATE: {}", e));
    }

    // Persist status change
    if let Err(e) = db::specs::update_spec_status(&conn, &spec.id, spec.status) {
        return Response::error(id, &format!("database error: {}", e));
    }

    Response::ok(
        id,
        serde_json::json!({
            "name": spec.name,
            "status": spec.status.to_string(),
        }),
    )
}

/// Handle "spec.delete" command.
///
/// Receives: { project_name, spec_name, force? }
/// Soft-delete: sets status to 'deleted', record remains in DB, markdown file preserved.
/// - If draft with active session: stops session first.
/// - If approved: requires force=true (confirmation).
/// - If decomposed: fails (must discard wave first).
fn handle_spec_delete(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: project_name");
        }
    };

    let spec_name = match req.params.get("spec_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: spec_name");
        }
    };

    let force = req
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            );
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut spec = match db::specs::get_spec_by_name(&conn, &project.id, &spec_name) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: spec '{}' not found", spec_name));
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // If approved and no force: require confirmation
    if spec.status == SpecStatus::Approved && !force {
        return Response::error(
            id,
            "INVALID_STATE: spec is approved; use force=true to confirm deletion",
        );
    }

    // If draft with active session: stop session first
    if spec.session_active {
        if let Err(e) = db::specs::update_spec_session(
            &conn,
            &spec.id,
            false,
            spec.claude_session_id.as_deref(),
        ) {
            return Response::error(id, &format!("database error: {}", e));
        }
        spec.session_active = false;
    }

    // Validate state machine transition (decomposed -> deleted fails)
    if let Err(e) = spec.delete() {
        return Response::error(id, &format!("INVALID_STATE: {}", e));
    }

    // Persist status change (soft-delete: record stays, file preserved)
    if let Err(e) = db::specs::update_spec_status(&conn, &spec.id, spec.status) {
        return Response::error(id, &format!("database error: {}", e));
    }

    Response::ok(
        id,
        serde_json::json!({
            "name": spec.name,
            "status": spec.status.to_string(),
            "deleted": true,
        }),
    )
}

/// Handle "spec.resume" command — resume a spec session.
///
/// Receives: { project_name, spec_name? }
/// If spec_name is provided, uses that spec. Otherwise finds latest draft.
/// Validates session not active, starts Claude with --resume.
fn handle_spec_resume(req: Request, state: &HandlerState) -> HandlerResult {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: project_name",
            ));
        }
    };

    let spec_name = req
        .params
        .get("spec_name")
        .and_then(|v| v.as_str())
        .map(String::from);

    let with_codebase = req
        .params
        .get("with_codebase")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            ));
        }
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Find the spec: by name or latest draft
    let spec = if let Some(name) = &spec_name {
        match db::specs::get_spec_by_name(&conn, &project.id, name) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("NOT_FOUND: spec '{}' not found", name),
                ));
            }
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("database error: {}", e),
                ));
            }
        }
    } else {
        match db::specs::find_latest_draft_spec(&conn, &project.id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return HandlerResult::Single(Response::error(
                    id,
                    "NOT_FOUND: no draft spec found for this project",
                ));
            }
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("database error: {}", e),
                ));
            }
        }
    };

    // Validate spec is a draft
    if spec.status != SpecStatus::Draft {
        return HandlerResult::Single(Response::error(
            id,
            &format!(
                "INVALID_STATE: spec '{}' is {} (must be draft to resume)",
                spec.name, spec.status
            ),
        ));
    }

    // Validate session not already active
    if spec.session_active {
        return HandlerResult::Single(Response::error(
            id,
            &format!(
                "INVALID_STATE: spec '{}' already has an active session",
                spec.name
            ),
        ));
    }

    // Must have a claude_session_id to resume
    let claude_session_id = match &spec.claude_session_id {
        Some(sid) => sid.clone(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                &format!(
                    "INVALID_STATE: spec '{}' has no previous session to resume",
                    spec.name
                ),
            ));
        }
    };

    // Mark session as active
    if let Err(e) = db::specs::update_spec_session(&conn, &spec.id, true, Some(&claude_session_id))
    {
        return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
    }

    // Build RunConfig for resumed spec session
    let nflow_home = match crate::daemon::nflow_home() {
        Ok(h) => h,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to get nflow home: {}", e),
            ));
        }
    };

    let override_dir = nflow_home.join("prompts");
    let override_path = if override_dir.is_dir() {
        Some(override_dir)
    } else {
        None
    };

    let template =
        match nflow_claude::prompt::load_template("spec_session", override_path.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("failed to load template: {}", e),
                ));
            }
        };

    let context = nflow_claude::context::build_spec_context(&spec, &project);
    let vars: std::collections::HashMap<&str, &str> = context
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let prompt = match nflow_claude::prompt::render_template(&template, &vars) {
        Ok(p) => p,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to render template: {}", e),
            ));
        }
    };

    let working_dir = if with_codebase {
        PathBuf::from(&project.path)
    } else {
        nflow_home
            .join("projects")
            .join(&project.name)
            .join("specs")
    };

    let mut run_config = nflow_claude::runner::RunConfig::for_spec(prompt, with_codebase);
    run_config.resume_session = Some(claude_session_id);
    run_config.working_dir = Some(working_dir);
    run_config.system_prompt_file = override_path.map(|d| d.join("spec_session.md"));

    // Create streaming channel and spawn Claude streaming
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let spec_id = spec.id;
    let spec_name_owned = spec.name.clone();
    let spec_file_path_str = spec.file_path.clone();
    let db_path = state.db_path.clone();

    let initial_id = id.clone();
    tokio::spawn(async move {
        // Send initial response with spec info
        let _ = tx
            .send(StreamingResponseLine::data(
                initial_id.clone(),
                serde_json::json!({
                    "spec_id": spec_id.to_string(),
                    "name": spec_name_owned,
                    "status": "draft",
                    "resumed": true,
                }),
            ))
            .await;

        stream_claude_spec_session(
            tx,
            initial_id,
            run_config,
            spec_id,
            &spec_file_path_str,
            &db_path,
        )
        .await;
    });

    HandlerResult::Streaming(rx)
}

/// Handle "plan.generate" command — decompose specs into a work item DAG.
///
/// Receives: { project_name, spec_names?, with_codebase? }
/// Returns (streaming): Claude output during decomposition, then final summary.
/// Final summary: { wave_number, epic_count, story_count, task_count }
fn handle_plan_generate(req: Request, state: &HandlerState) -> HandlerResult {
    let id = req.id.clone();

    // Parse required params
    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: project_name",
            ));
        }
    };

    let spec_names: Option<Vec<String>> = req
        .params
        .get("spec_names")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });

    let with_codebase = req
        .params
        .get("with_codebase")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Find project by name
    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            ));
        }
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Validate no draft wave exists for this project
    match db::decomposition_sessions::find_draft_session(&conn, &project.id) {
        Ok(Some(_)) => {
            return HandlerResult::Single(Response::error(
                id,
                "INVALID_STATE: project already has a draft wave (in_progress). Approve or discard it first.",
            ));
        }
        Ok(None) => {}
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    }

    // Find specs to decompose
    let specs = if let Some(names) = &spec_names {
        // Specific specs requested — look them up by name
        let mut found = Vec::new();
        for name in names {
            match db::specs::get_spec_by_name(&conn, &project.id, name) {
                Ok(Some(s)) => {
                    if s.status != SpecStatus::Approved {
                        return HandlerResult::Single(Response::error(
                            id,
                            &format!(
                                "INVALID_STATE: spec '{}' is {} (must be approved)",
                                s.name, s.status
                            ),
                        ));
                    }
                    found.push(s);
                }
                Ok(None) => {
                    return HandlerResult::Single(Response::error(
                        id,
                        &format!("NOT_FOUND: spec '{}' not found", name),
                    ));
                }
                Err(e) => {
                    return HandlerResult::Single(Response::error(
                        id,
                        &format!("database error: {}", e),
                    ));
                }
            }
        }
        found
    } else {
        // Find all unassigned approved specs
        match db::specs::find_unassigned_approved_specs(&conn, &project.id) {
            Ok(s) => s,
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("database error: {}", e),
                ));
            }
        }
    };

    if specs.is_empty() {
        return HandlerResult::Single(Response::error(
            id,
            "NOT_FOUND: no approved specs available for decomposition",
        ));
    }

    // Read spec file contents
    let mut specs_with_content: Vec<(Spec, String)> = Vec::new();
    for spec in &specs {
        match std::fs::read_to_string(&spec.file_path) {
            Ok(content) => specs_with_content.push((spec.clone(), content)),
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("failed to read spec file '{}': {}", spec.file_path, e),
                ));
            }
        }
    }

    // Get next wave number
    let wave_number = match db::decomposition_sessions::next_wave_number(&conn, &project.id) {
        Ok(n) => n,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Create decomposition session
    let has_in_progress = |_pid: uuid::Uuid| -> bool { false }; // Already checked above
    let session = match DecompositionSession::new(project.id, has_in_progress, wave_number - 1) {
        Ok(s) => s,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to create session: {}", e),
            ));
        }
    };

    // Insert session into DB
    if let Err(e) = db::decomposition_sessions::insert_decomposition_session(&conn, &session) {
        return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
    }

    // Link specs to session
    for spec in &specs {
        let ds = DecompositionSpec::new(session.id, spec.id);
        if let Err(e) = db::decomposition_sessions::insert_decomposition_spec(&conn, &ds) {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    }

    // Build the decomposition prompt
    let decompose_prompt = nflow_claude::context::build_decompose_prompt(&specs_with_content);

    // Load and render the decompose template
    let nflow_home = match crate::daemon::nflow_home() {
        Ok(h) => h,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to get nflow home: {}", e),
            ));
        }
    };
    let override_dir = nflow_home.join("prompts");
    let override_path = if override_dir.is_dir() {
        Some(override_dir)
    } else {
        None
    };

    let template = match nflow_claude::prompt::load_template("decompose", override_path.as_deref())
    {
        Ok(t) => t,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to load template: {}", e),
            ));
        }
    };

    let mut vars = std::collections::HashMap::new();
    vars.insert("specs_content", decompose_prompt.as_str());
    vars.insert("project_name", project.name.as_str());
    let prompt = match nflow_claude::prompt::render_template(&template, &vars) {
        Ok(p) => p,
        Err(e) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("failed to render template: {}", e),
            ));
        }
    };

    // Determine working directory
    let working_dir = if with_codebase {
        PathBuf::from(&project.path)
    } else {
        nflow_home
            .join("projects")
            .join(&project.name)
            .join("specs")
    };

    // Build RunConfig for decomposition
    let run_config =
        nflow_claude::runner::RunConfig::for_decompose(prompt, with_codebase, working_dir);

    // Create streaming channel and spawn Claude streaming
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let session_id = session.id;
    let db_path = state.db_path.clone();
    let spec_ids: Vec<uuid::Uuid> = specs.iter().map(|s| s.id).collect();

    let initial_id = id.clone();
    tokio::spawn(async move {
        // Send initial response with session info
        let _ = tx
            .send(StreamingResponseLine::data(
                initial_id.clone(),
                serde_json::json!({
                    "session_id": session_id.to_string(),
                    "wave_number": wave_number,
                    "spec_count": spec_ids.len(),
                }),
            ))
            .await;

        stream_claude_decompose(
            tx,
            initial_id,
            run_config,
            session_id,
            wave_number,
            spec_ids,
            &db_path,
        )
        .await;
    });

    HandlerResult::Streaming(rx)
}

/// Stream Claude decomposition output and process the result.
///
/// On completion, parses the Claude output as JSON containing epics, stories,
/// tasks, and dependencies. Creates work items, auto-generates verify tasks,
/// validates the DAG, and persists everything to the database.
async fn stream_claude_decompose(
    tx: tokio::sync::mpsc::Sender<StreamingResponseLine>,
    req_id: String,
    run_config: nflow_claude::runner::RunConfig,
    session_id: uuid::Uuid,
    wave_number: u32,
    spec_ids: Vec<uuid::Uuid>,
    db_path: &Path,
) {
    // Spawn Claude process
    let runner = nflow_claude::runner::ClaudeRunner::default();
    let process = match runner.spawn(&run_config) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx
                .send(StreamingResponseLine::error(
                    req_id,
                    &format!("failed to spawn Claude: {}", e),
                ))
                .await;
            return;
        }
    };

    // Stream Claude output and collect the final result text
    let mut parser = nflow_claude::stream::StreamParser::new(process.stdout);
    let mut result_text = String::new();

    loop {
        match parser.next_event().await {
            Ok(Some(event)) => {
                // Capture result text for parsing
                if let nflow_claude::stream::StreamEvent::Result { ref text, .. } = event {
                    result_text = text.clone();
                }

                let event_json = match &event {
                    nflow_claude::stream::StreamEvent::TextDelta { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    nflow_claude::stream::StreamEvent::ToolUse { name, input } => {
                        serde_json::json!({"type": "tool_use", "name": name, "input": input})
                    }
                    nflow_claude::stream::StreamEvent::ToolResult { content } => {
                        serde_json::json!({"type": "tool_result", "content": content})
                    }
                    nflow_claude::stream::StreamEvent::Result { text, session_id } => {
                        serde_json::json!({"type": "result", "text": text, "session_id": session_id})
                    }
                    nflow_claude::stream::StreamEvent::Error { message } => {
                        serde_json::json!({"type": "error", "message": message})
                    }
                    nflow_claude::stream::StreamEvent::ParseError { line, reason } => {
                        serde_json::json!({"type": "parse_error", "line": line, "reason": reason})
                    }
                };

                if tx
                    .send(StreamingResponseLine::data(req_id.clone(), event_json))
                    .await
                    .is_err()
                {
                    break; // Client disconnected
                }
            }
            Ok(None) => break, // Stream ended
            Err(e) => {
                let _ = tx
                    .send(StreamingResponseLine::error(
                        req_id.clone(),
                        &format!("stream error: {}", e),
                    ))
                    .await;
                break;
            }
        }
    }

    // Store claude_session_id
    let claude_session_id = parser.session_id().map(|s| s.to_string());
    if let (Ok(conn), Some(ref csid)) = (db::open_connection(db_path), &claude_session_id) {
        let _ = db::decomposition_sessions::update_session_claude_id(&conn, &session_id, csid);
    }

    // Parse Claude's JSON output into work items
    if result_text.is_empty() {
        let _ = tx
            .send(StreamingResponseLine::error(
                req_id,
                "Claude did not produce a result — decomposition failed",
            ))
            .await;
        return;
    }

    // Extract JSON from result text (may be wrapped in markdown code fences)
    let json_text = extract_json_from_text(&result_text);

    let parsed: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx
                .send(StreamingResponseLine::error(
                    req_id,
                    &format!("failed to parse Claude output as JSON: {}", e),
                ))
                .await;
            return;
        }
    };

    // Build work items from parsed JSON
    let conn = match db::open_connection(db_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(StreamingResponseLine::error(
                    req_id,
                    &format!("database error: {}", e),
                ))
                .await;
            return;
        }
    };

    match build_and_persist_work_items(&conn, &parsed, session_id, wave_number, &spec_ids) {
        Ok(counts) => {
            let _ = tx
                .send(StreamingResponseLine::done(
                    req_id,
                    serde_json::json!({
                        "session_id": session_id.to_string(),
                        "wave_number": wave_number,
                        "epic_count": counts.0,
                        "story_count": counts.1,
                        "task_count": counts.2,
                    }),
                ))
                .await;
        }
        Err(e) => {
            let _ = tx.send(StreamingResponseLine::error(req_id, &e)).await;
        }
    }
}

/// Extract JSON from text that may contain markdown code fences.
fn extract_json_from_text(text: &str) -> &str {
    let trimmed = text.trim();
    // Try to find JSON within ```json ... ``` blocks
    if let Some(start) = trimmed.find("```json") {
        let json_start = start + 7; // skip "```json"
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim();
        }
    }
    // Try plain ``` blocks
    if let Some(start) = trimmed.find("```") {
        let json_start = start + 3;
        // Skip optional language identifier on the same line
        let after_backticks = &trimmed[json_start..];
        let content_start = after_backticks.find('\n').map_or(0, |i| i + 1);
        if let Some(end) = after_backticks[content_start..].find("```") {
            return after_backticks[content_start..content_start + end].trim();
        }
    }
    // Return as-is
    trimmed
}

/// Build work items from Claude's JSON output, validate DAG, and persist to DB.
///
/// Expected JSON format:
/// ```json
/// {
///   "epics": [{
///     "id": "E1",
///     "title": "...",
///     "description": "...",
///     "stories": [{
///       "id": "S1",
///       "title": "...",
///       "description": "...",
///       "acceptance_criteria": "...",
///       "depends_on": ["S2"],
///       "tasks": [{
///         "id": "T1",
///         "title": "...",
///         "description": "...",
///         "acceptance_criteria": "..."
///       }]
///     }]
///   }]
/// }
/// ```
///
/// Returns (epic_count, story_count, task_count) on success.
fn build_and_persist_work_items(
    conn: &rusqlite::Connection,
    parsed: &serde_json::Value,
    session_id: uuid::Uuid,
    wave_number: u32,
    spec_ids: &[uuid::Uuid],
) -> std::result::Result<(usize, usize, usize), String> {
    let epics_arr = parsed
        .get("epics")
        .and_then(|v| v.as_array())
        .ok_or("JSON missing 'epics' array")?;

    let mut all_stories: Vec<WorkItem> = Vec::new();
    let mut all_impl_tasks: Vec<WorkItem> = Vec::new();
    let mut dependency_refs: Vec<(String, Vec<String>)> = Vec::new(); // (story_short_id, depends_on_short_ids)
    let mut short_id_to_uuid: std::collections::HashMap<String, uuid::Uuid> =
        std::collections::HashMap::new();

    let mut epic_sort = 0i32;
    let mut story_sort = 0i32;

    for epic_val in epics_arr {
        let epic_short_id = epic_val
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(&format!("E{}", epic_sort + 1))
            .to_string();
        let epic_title = epic_val
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled Epic")
            .to_string();
        let epic_description = epic_val
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let epic = WorkItem::new_epic(
            session_id,
            epic_title,
            epic_description,
            epic_short_id.clone(),
            epic_sort,
        );
        epic_sort += 1;

        db::work_items::insert_work_item(conn, &epic)
            .map_err(|e| format!("failed to insert epic: {}", e))?;

        // Process stories under this epic
        if let Some(stories_arr) = epic_val.get("stories").and_then(|v| v.as_array()) {
            for story_val in stories_arr {
                let story_short_id = story_val
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&format!("S{}", story_sort + 1))
                    .to_string();
                let story_title = story_val
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled Story")
                    .to_string();
                let story_description = story_val
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let story_ac = story_val
                    .get("acceptance_criteria")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // Wave-prefix the short_id
                let wave_short_id = format!("W{}-{}", wave_number, story_short_id);

                let story = WorkItem::new_story(
                    epic.id,
                    session_id,
                    story_title,
                    story_description,
                    story_ac,
                    wave_short_id.clone(),
                    story_sort,
                );
                story_sort += 1;

                short_id_to_uuid.insert(story_short_id.clone(), story.id);

                // Collect dependency references
                let depends_on: Vec<String> = story_val
                    .get("depends_on")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if !depends_on.is_empty() {
                    dependency_refs.push((story_short_id.clone(), depends_on));
                }

                db::work_items::insert_work_item(conn, &story)
                    .map_err(|e| format!("failed to insert story: {}", e))?;

                all_stories.push(story.clone());

                // Process tasks under this story
                let mut task_sort = 0i32;
                if let Some(tasks_arr) = story_val.get("tasks").and_then(|v| v.as_array()) {
                    for task_val in tasks_arr {
                        let task_short_id = task_val
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&format!("T{}", task_sort / 2 + 1))
                            .to_string();
                        let task_title = task_val
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Untitled Task")
                            .to_string();
                        let task_description = task_val
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let task_ac = task_val
                            .get("acceptance_criteria")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        let task = WorkItem::new_task(
                            story.id,
                            session_id,
                            task_title,
                            task_description,
                            task_ac,
                            task_short_id,
                            task_sort,
                        );
                        task_sort += 2; // Leave room for verify tasks (sort_order + 1)

                        db::work_items::insert_work_item(conn, &task)
                            .map_err(|e| format!("failed to insert task: {}", e))?;

                        all_impl_tasks.push(task);
                    }
                }
            }
        }
    }

    // Auto-generate verify tasks
    let verify_tasks = auto_generate_verify_tasks(&all_impl_tasks);
    let task_count = all_impl_tasks.len() + verify_tasks.len();
    for vt in &verify_tasks {
        db::work_items::insert_work_item(conn, vt)
            .map_err(|e| format!("failed to insert verify task: {}", e))?;
    }

    // Resolve and insert dependencies
    let mut dependencies: Vec<Dependency> = Vec::new();
    for (blocked_short_id, blocker_short_ids) in &dependency_refs {
        let blocked_uuid = short_id_to_uuid
            .get(blocked_short_id)
            .ok_or(format!("dependency: unknown story '{}'", blocked_short_id))?;
        for blocker_short_id in blocker_short_ids {
            let blocker_uuid = short_id_to_uuid.get(blocker_short_id).ok_or(format!(
                "dependency: unknown blocker story '{}'",
                blocker_short_id
            ))?;
            let dep = Dependency::new(*blocker_uuid, *blocked_uuid);
            db::work_items::insert_dependency(conn, &dep)
                .map_err(|e| format!("failed to insert dependency: {}", e))?;
            dependencies.push(dep);
        }
    }

    // Validate DAG (cycle check)
    nflow_core::dag::build_dag(&all_stories, &dependencies)
        .map_err(|e| format!("DAG validation failed: {}", e))?;

    // Mark specs as decomposed
    for spec_id in spec_ids {
        db::specs::update_spec_status(conn, spec_id, SpecStatus::Decomposed)
            .map_err(|e| format!("failed to update spec status: {}", e))?;
    }

    Ok((epic_sort as usize, all_stories.len(), task_count))
}

/// Check if a path is a git repository using synchronous git command.
fn is_git_repo(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the remote URL from a git repo (origin remote).
fn get_remote_url(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if url.is_empty() {
            None
        } else {
            Some(url)
        }
    } else {
        None
    }
}

/// Get the current HEAD ref (branch name) from a git repo.
///
/// Uses `git symbolic-ref --short HEAD` which works even in repos with no commits.
fn get_head_ref(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

/// Parse a git provider string to the enum.
fn parse_git_provider(s: &str) -> Option<GitProvider> {
    match s.to_lowercase().as_str() {
        "github" => Some(GitProvider::Github),
        "gitlab" => Some(GitProvider::Gitlab),
        _ => None,
    }
}

/// Remove a git worktree synchronously.
fn remove_worktree_sync(repo_path: &Path, worktree_path: &Path) {
    // Try git worktree remove --force first
    let _ = std::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Prune any stale worktree entries
    let _ = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Remove the project directories under ~/.nflow/projects/{name}/.
fn remove_project_dirs(project_name: &str) {
    if let Ok(home) = crate::daemon::nflow_home() {
        let project_dir = home.join("projects").join(project_name);
        let _ = std::fs::remove_dir_all(project_dir);
    }
}

/// Create the project directories under ~/.nflow/projects/{name}/.
fn create_project_dirs(project_name: &str) -> std::io::Result<()> {
    let home = crate::daemon::nflow_home()
        .map_err(|e| std::io::Error::other(format!("failed to get nflow home: {}", e)))?;
    let project_dir = home.join("projects").join(project_name);
    std::fs::create_dir_all(project_dir.join("specs"))?;
    std::fs::create_dir_all(project_dir.join("agent-logs"))?;
    Ok(())
}

/// Handle "plan.show" command.
///
/// Receives: { project_name, wave?, dag? }
/// Returns:
///   - Tree format (default): { wave_number, status, epics: [{ short_id, title, status, stories }] }
///   - DAG format (with dag=true): { wave_number, adjacency: { short_id: [blocked_ids] } }
fn handle_plan_show(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, "missing required parameter: project_name");
        }
    };

    let wave_number: Option<u32> = req
        .params
        .get("wave")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let use_dag = req
        .params
        .get("dag")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            );
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let all_sessions =
        match db::decomposition_sessions::list_sessions_by_project(&conn, &project.id) {
            Ok(s) => s,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

    if all_sessions.is_empty() {
        return Response::error(
            id,
            &format!(
                "NOT_FOUND: project '{}' has no decomposition waves",
                project_name
            ),
        );
    }

    let session = if let Some(wave) = wave_number {
        match all_sessions.iter().find(|s| s.wave_number == wave) {
            Some(s) => s.clone(),
            None => {
                return Response::error(id, &format!("NOT_FOUND: wave W{} not found", wave));
            }
        }
    } else {
        all_sessions.last().unwrap().clone()
    };

    let all_items = match db::work_items::list_work_items_by_session(&conn, &session.id) {
        Ok(items) => items,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let dependencies = match db::work_items::list_dependencies_by_session(&conn, &session.id) {
        Ok(deps) => deps,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    if use_dag {
        // Build DAG adjacency list
        let stories: Vec<&WorkItem> = all_items
            .iter()
            .filter(|item| item.item_type == ItemType::Story)
            .collect();

        let mut adjacency = serde_json::Map::new();
        for story in &stories {
            let display_id = format!("W{}-{}", session.wave_number, story.short_id);
            let blocked: Vec<serde_json::Value> = dependencies
                .iter()
                .filter(|dep| dep.blocker_id == story.id)
                .filter_map(|dep| {
                    all_items
                        .iter()
                        .find(|item| item.id == dep.blocked_id)
                        .map(|item| {
                            serde_json::Value::String(format!(
                                "W{}-{}",
                                session.wave_number, item.short_id
                            ))
                        })
                })
                .collect();
            adjacency.insert(display_id, serde_json::Value::Array(blocked));
        }

        return Response::ok(
            id,
            serde_json::json!({
                "wave_number": session.wave_number,
                "status": session.status.to_string(),
                "adjacency": adjacency,
            }),
        );
    }

    // Build hierarchical tree
    let epics: Vec<&WorkItem> = all_items
        .iter()
        .filter(|item| item.item_type == ItemType::Epic)
        .collect();

    let mut epics_data = Vec::new();
    for epic in &epics {
        let epic_stories: Vec<&WorkItem> = all_items
            .iter()
            .filter(|item| item.item_type == ItemType::Story && item.parent_id == Some(epic.id))
            .collect();

        let mut stories_data = Vec::new();
        for story in &epic_stories {
            let story_tasks: Vec<&WorkItem> = all_items
                .iter()
                .filter(|item| item.item_type == ItemType::Task && item.parent_id == Some(story.id))
                .collect();

            let total_tasks = story_tasks.len() as u32;
            let done_tasks = story_tasks
                .iter()
                .filter(|t| t.status == WorkItemStatus::Done)
                .count() as u32;

            let depends_on: Vec<String> = dependencies
                .iter()
                .filter(|dep| dep.blocked_id == story.id)
                .filter_map(|dep| {
                    all_items
                        .iter()
                        .find(|item| item.id == dep.blocker_id)
                        .map(|item| format!("W{}-{}", session.wave_number, item.short_id))
                })
                .collect();

            let mut tasks_data = Vec::new();
            for task in &story_tasks {
                tasks_data.push(serde_json::json!({
                    "short_id": format!("W{}-{}", session.wave_number, task.short_id),
                    "title": task.title,
                    "status": task.status.to_string(),
                    "kind": task.kind.as_ref().map(|k| k.to_string()),
                }));
            }

            stories_data.push(serde_json::json!({
                "short_id": format!("W{}-{}", session.wave_number, story.short_id),
                "title": story.title,
                "status": story.status.to_string(),
                "depends_on": depends_on,
                "progress": format!("{}/{}", done_tasks, total_tasks),
                "tasks": tasks_data,
            }));
        }

        epics_data.push(serde_json::json!({
            "short_id": format!("W{}-{}", session.wave_number, epic.short_id),
            "title": epic.title,
            "status": epic.status.to_string(),
            "stories": stories_data,
        }));
    }

    Response::ok(
        id,
        serde_json::json!({
            "wave_number": session.wave_number,
            "status": session.status.to_string(),
            "epics": epics_data,
        }),
    )
}

/// Handle "plan.feedback" command.
///
/// Sends feedback to an existing draft wave's decomposition session via Claude --resume.
/// Deletes existing work items and replaces them with the new Claude output.
///
/// Params:
///   - project_name (required): project name
///   - message (required): feedback message to send to Claude
///   - wave_number (optional): specific wave to target (defaults to latest draft)
fn handle_plan_feedback(req: Request, state: &HandlerState) -> HandlerResult {
    let id = req.id.clone();

    // Parse required params
    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: project_name",
            ));
        }
    };

    let message = match req.params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                "missing required parameter: message",
            ));
        }
    };

    let wave_number: Option<u32> = req
        .params
        .get("wave_number")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Find project
    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return HandlerResult::Single(Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            ));
        }
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Find draft session (by wave_number or latest draft)
    let session = if let Some(wave) = wave_number {
        match db::decomposition_sessions::get_session_by_wave(&conn, &project.id, wave) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("NOT_FOUND: wave W{} not found", wave),
                ));
            }
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("database error: {}", e),
                ));
            }
        }
    } else {
        match db::decomposition_sessions::find_draft_session(&conn, &project.id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return HandlerResult::Single(Response::error(
                    id,
                    "NOT_FOUND: no draft wave found for this project",
                ));
            }
            Err(e) => {
                return HandlerResult::Single(Response::error(
                    id,
                    &format!("database error: {}", e),
                ));
            }
        }
    };

    // Validate session is in draft (in_progress) state
    if session.status != nflow_core::decomposition::DecompositionStatus::InProgress {
        return HandlerResult::Single(Response::error(
            id,
            &format!(
                "INVALID_STATE: wave W{} is {} (must be in_progress for feedback)",
                session.wave_number, session.status
            ),
        ));
    }

    // Get the Claude session ID for --resume
    let claude_session_id = match &session.claude_session_id {
        Some(csid) => csid.clone(),
        None => {
            return HandlerResult::Single(Response::error(
                id,
                &format!(
                    "INVALID_STATE: wave W{} has no Claude session ID (cannot resume)",
                    session.wave_number
                ),
            ));
        }
    };

    // Get spec IDs linked to this session
    let spec_ids = match db::decomposition_sessions::list_spec_ids_by_session(&conn, &session.id) {
        Ok(ids) => ids,
        Err(e) => {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    };

    // Delete existing work items for this session (feedback = full regeneration)
    if let Err(e) = db::work_items::delete_work_items_by_session(&conn, &session.id) {
        return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
    }

    // Revert specs from decomposed back to approved so build_and_persist can re-mark them
    for spec_id in &spec_ids {
        if let Err(e) = db::specs::update_spec_status(&conn, spec_id, SpecStatus::Approved) {
            return HandlerResult::Single(Response::error(id, &format!("database error: {}", e)));
        }
    }

    // Determine working directory
    let working_dir = PathBuf::from(&project.path);

    // Build RunConfig for decomposition with --resume
    let mut run_config = nflow_claude::runner::RunConfig::for_decompose(message, true, working_dir);
    run_config.resume_session = Some(claude_session_id);

    // Create streaming channel and spawn Claude streaming
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let session_id = session.id;
    let w = session.wave_number;
    let db_path = state.db_path.clone();

    let initial_id = id.clone();
    tokio::spawn(async move {
        // Send initial response with session info
        let _ = tx
            .send(StreamingResponseLine::data(
                initial_id.clone(),
                serde_json::json!({
                    "session_id": session_id.to_string(),
                    "wave_number": w,
                    "spec_count": spec_ids.len(),
                    "feedback": true,
                }),
            ))
            .await;

        stream_claude_decompose(
            tx, initial_id, run_config, session_id, w, spec_ids, &db_path,
        )
        .await;
    });

    HandlerResult::Streaming(rx)
}

/// Handle "plan.approve" command.
///
/// Transitions a draft wave (in_progress) to approved, making its stories schedulable.
/// If auto_execute param is true, sets execution_enabled = true on the project.
///
/// Params: project_name (required), wave_number (optional — defaults to latest draft),
///         auto_execute (optional bool — if true, enables execution on project)
fn handle_plan_approve(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    // Parse required params
    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::error(id, "missing required parameter: project_name"),
    };

    let wave_number: Option<u32> = req
        .params
        .get("wave_number")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let auto_execute = req
        .params
        .get("auto_execute")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Find project
    let mut project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Find session (by wave_number or latest draft)
    let mut session = if let Some(wave) = wave_number {
        match db::decomposition_sessions::get_session_by_wave(&conn, &project.id, wave) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::error(id, &format!("NOT_FOUND: wave W{} not found", wave))
            }
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        }
    } else {
        match db::decomposition_sessions::find_draft_session(&conn, &project.id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::error(id, "NOT_FOUND: no draft wave found for this project")
            }
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        }
    };

    // Validate session is in draft (in_progress) state
    if session.status != DecompositionStatus::InProgress {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: wave W{} is {} (must be in_progress for approval)",
                session.wave_number, session.status
            ),
        );
    }

    // Approve the session
    if let Err(e) = session.approve() {
        return Response::error(id, &format!("INVALID_STATE: {}", e));
    }

    // Persist session status change
    if let Err(e) =
        db::decomposition_sessions::update_session_status(&conn, &session.id, session.status)
    {
        return Response::error(id, &format!("database error: {}", e));
    }

    // If auto_execute is true, enable execution on the project
    if auto_execute {
        project.execution_enabled = true;
        project.updated_at = chrono::Utc::now();
        if let Err(e) = db::projects::update_project(&conn, &project) {
            return Response::error(id, &format!("database error: {}", e));
        }
    }

    Response::ok(
        id,
        serde_json::json!({
            "wave_number": session.wave_number,
            "status": session.status.to_string(),
            "execution_enabled": project.execution_enabled,
        }),
    )
}

/// Handle "plan.discard" command.
///
/// Discards a wave (draft or approved), deleting all work items and reverting
/// associated specs from decomposed back to approved.
/// Fails if the wave has stories with in_progress status.
///
/// Params: project_name (required), wave_number (optional — defaults to latest draft)
fn handle_plan_discard(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    // Parse required params
    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::error(id, "missing required parameter: project_name"),
    };

    let wave_number: Option<u32> = req
        .params
        .get("wave_number")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    // Open DB connection
    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Find project
    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Find session (by wave_number or latest non-discarded)
    let session = if let Some(wave) = wave_number {
        match db::decomposition_sessions::get_session_by_wave(&conn, &project.id, wave) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::error(id, &format!("NOT_FOUND: wave W{} not found", wave))
            }
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        }
    } else {
        match db::decomposition_sessions::find_draft_session(&conn, &project.id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::error(id, "NOT_FOUND: no draft wave found for this project")
            }
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        }
    };

    // Validate session is not already discarded
    if session.status == DecompositionStatus::Discarded {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: wave W{} is already discarded",
                session.wave_number
            ),
        );
    }

    // Check for in_progress stories — must stop them first
    match db::work_items::has_in_progress_stories_by_session(&conn, &session.id) {
        Ok(true) => {
            return Response::error(
                id,
                &format!(
                    "INVALID_STATE: wave W{} has stories in progress (stop them first)",
                    session.wave_number
                ),
            );
        }
        Ok(false) => {}
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    }

    // Get spec IDs linked to this session
    let spec_ids = match db::decomposition_sessions::list_spec_ids_by_session(&conn, &session.id) {
        Ok(ids) => ids,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Delete all work items for this session
    if let Err(e) = db::work_items::delete_work_items_by_session(&conn, &session.id) {
        return Response::error(id, &format!("database error: {}", e));
    }

    // Revert specs from decomposed back to approved
    for spec_id in &spec_ids {
        if let Err(e) = db::specs::update_spec_status(&conn, spec_id, SpecStatus::Approved) {
            return Response::error(id, &format!("database error: {}", e));
        }
    }

    // Update session status to discarded
    if let Err(e) = db::decomposition_sessions::update_session_status(
        &conn,
        &session.id,
        DecompositionStatus::Discarded,
    ) {
        return Response::error(id, &format!("database error: {}", e));
    }

    Response::ok(
        id,
        serde_json::json!({
            "wave_number": session.wave_number,
            "status": "discarded",
        }),
    )
}

/// Handle "exec.run" command.
///
/// Sets execution_enabled = true for the project. Optionally overrides
/// max_parallel (stored in memory — not persisted to DB) and limits execution
/// to a specific story.
///
/// Params: project_name (required), parallel (optional u32), story (optional short_id string)
fn handle_exec_run(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::error(id, "missing required parameter: project_name"),
    };

    let _parallel: Option<u32> = req
        .params
        .get("parallel")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let _story: Option<String> = req
        .params
        .get("story")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Set execution_enabled = true
    if !project.execution_enabled {
        project.execution_enabled = true;
        project.updated_at = chrono::Utc::now();
        if let Err(e) = db::projects::update_project(&conn, &project) {
            return Response::error(id, &format!("database error: {}", e));
        }
    }

    // Count running and pending for status response
    let running_count = match db::agent_runs::find_running_agent_runs_by_project(&conn, &project.id)
    {
        Ok(runs) => runs.len() as u32,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let pending_count = match db::work_items::count_pending_stories_by_project(&conn, &project.id) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    Response::ok(
        id,
        serde_json::json!({
            "execution_enabled": true,
            "running_count": running_count,
            "pending_count": pending_count,
        }),
    )
}

/// Handle "exec.pause" command.
///
/// Sets execution_enabled = false for the project. Running agents continue,
/// but no new agents will be started by the scheduler.
///
/// Params: project_name (required)
fn handle_exec_pause(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::error(id, "missing required parameter: project_name"),
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Set execution_enabled = false
    if project.execution_enabled {
        project.execution_enabled = false;
        project.updated_at = chrono::Utc::now();
        if let Err(e) = db::projects::update_project(&conn, &project) {
            return Response::error(id, &format!("database error: {}", e));
        }
    }

    // Count running and pending for status response
    let running_count = match db::agent_runs::find_running_agent_runs_by_project(&conn, &project.id)
    {
        Ok(runs) => runs.len() as u32,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let pending_count = match db::work_items::count_pending_stories_by_project(&conn, &project.id) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    Response::ok(
        id,
        serde_json::json!({
            "execution_enabled": false,
            "running_count": running_count,
            "pending_count": pending_count,
        }),
    )
}

/// Handle "exec.status" command.
///
/// Returns hierarchical status of all work items for a project.
/// Waves -> epics -> stories -> tasks, with summary counts per wave.
///
/// Params:
///   - project_name (required): project name
///   - wave (optional): filter to specific wave number
fn handle_exec_status(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let project_name = match req.params.get("project_name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::error(id, "missing required parameter: project_name"),
    };

    let wave_filter: Option<u32> = req
        .params
        .get("wave")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let all_sessions =
        match db::decomposition_sessions::list_sessions_by_project(&conn, &project.id) {
            Ok(s) => s,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

    // Filter sessions by wave number if specified
    let sessions: Vec<_> = if let Some(wave) = wave_filter {
        match all_sessions.iter().find(|s| s.wave_number == wave) {
            Some(s) => vec![s.clone()],
            None => {
                return Response::error(id, &format!("NOT_FOUND: wave W{} not found", wave));
            }
        }
    } else {
        all_sessions
    };

    let running_count = match db::agent_runs::find_running_agent_runs_by_project(&conn, &project.id)
    {
        Ok(runs) => runs.len() as u32,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut waves_data = Vec::new();

    for session in &sessions {
        let all_items = match db::work_items::list_work_items_by_session(&conn, &session.id) {
            Ok(items) => items,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        // Compute summary counts for this wave
        let mut summary = serde_json::Map::new();
        let stories: Vec<&WorkItem> = all_items
            .iter()
            .filter(|item| item.item_type == ItemType::Story)
            .collect();
        let total = stories.len() as u32;
        let mut pending: u32 = 0;
        let mut ready: u32 = 0;
        let mut running: u32 = 0;
        let mut done: u32 = 0;
        let mut failed: u32 = 0;
        let mut cancelled: u32 = 0;
        for story in &stories {
            match story.status {
                WorkItemStatus::Pending => pending += 1,
                WorkItemStatus::Ready => ready += 1,
                WorkItemStatus::InProgress => running += 1,
                WorkItemStatus::Done => done += 1,
                WorkItemStatus::Failed => failed += 1,
                WorkItemStatus::Cancelled => cancelled += 1,
            }
        }
        summary.insert("total".to_string(), serde_json::json!(total));
        summary.insert("pending".to_string(), serde_json::json!(pending));
        summary.insert("ready".to_string(), serde_json::json!(ready));
        summary.insert("running".to_string(), serde_json::json!(running));
        summary.insert("done".to_string(), serde_json::json!(done));
        summary.insert("failed".to_string(), serde_json::json!(failed));
        summary.insert("cancelled".to_string(), serde_json::json!(cancelled));

        // Build hierarchical tree
        let epics: Vec<&WorkItem> = all_items
            .iter()
            .filter(|item| item.item_type == ItemType::Epic)
            .collect();

        let mut epics_data = Vec::new();
        for epic in &epics {
            let epic_stories: Vec<&WorkItem> = all_items
                .iter()
                .filter(|item| item.item_type == ItemType::Story && item.parent_id == Some(epic.id))
                .collect();

            let mut stories_data = Vec::new();
            for story in &epic_stories {
                let story_tasks: Vec<&WorkItem> = all_items
                    .iter()
                    .filter(|item| {
                        item.item_type == ItemType::Task && item.parent_id == Some(story.id)
                    })
                    .collect();

                let mut tasks_data = Vec::new();
                for task in &story_tasks {
                    // Get agent run info for exit_code and retry_count
                    let retry_count =
                        db::agent_runs::count_agent_runs_for_task(&conn, &task.id).unwrap_or(0);
                    let latest_run =
                        db::agent_runs::find_latest_agent_run_for_task(&conn, &task.id)
                            .ok()
                            .flatten();

                    let exit_code = latest_run.as_ref().and_then(|r| r.exit_code);
                    let task_started_at = latest_run.as_ref().map(|r| r.started_at.to_rfc3339());
                    let task_finished_at = latest_run
                        .as_ref()
                        .and_then(|r| r.finished_at.map(|t| t.to_rfc3339()));

                    tasks_data.push(serde_json::json!({
                        "short_id": format!("W{}-{}", session.wave_number, task.short_id),
                        "title": task.title,
                        "status": task.status.to_string(),
                        "kind": task.kind.as_ref().map(|k| k.to_string()),
                        "exit_code": exit_code,
                        "retry_count": retry_count,
                        "started_at": task_started_at,
                        "finished_at": task_finished_at,
                    }));
                }

                stories_data.push(serde_json::json!({
                    "short_id": format!("W{}-{}", session.wave_number, story.short_id),
                    "title": story.title,
                    "status": story.status.to_string(),
                    "branch_name": story.branch_name,
                    "mr_url": story.mr_url,
                    "worktree_path": story.worktree_path,
                    "started_at": story.created_at.to_rfc3339(),
                    "finished_at": if story.status == WorkItemStatus::Done || story.status == WorkItemStatus::Failed || story.status == WorkItemStatus::Cancelled {
                        Some(story.updated_at.to_rfc3339())
                    } else {
                        None
                    },
                    "tasks": tasks_data,
                }));
            }

            epics_data.push(serde_json::json!({
                "short_id": format!("W{}-{}", session.wave_number, epic.short_id),
                "title": epic.title,
                "status": epic.status.to_string(),
                "started_at": epic.created_at.to_rfc3339(),
                "finished_at": if epic.status == WorkItemStatus::Done || epic.status == WorkItemStatus::Failed || epic.status == WorkItemStatus::Cancelled {
                    Some(epic.updated_at.to_rfc3339())
                } else {
                    None
                },
                "stories": stories_data,
            }));
        }

        waves_data.push(serde_json::json!({
            "wave_number": session.wave_number,
            "status": session.status.to_string(),
            "summary": summary,
            "epics": epics_data,
        }));
    }

    Response::ok(
        id,
        serde_json::json!({
            "project_name": project.name,
            "execution_enabled": project.execution_enabled,
            "running_count": running_count,
            "waves": waves_data,
        }),
    )
}

/// Handle "exec.retry" command.
///
/// Retries a failed task by resetting state and spawning a new Claude agent.
/// For impl tasks, resets the worktree to the previous task's commit (or base if first).
/// For verify tasks, no worktree reset is needed.
///
/// Params:
///   - task_id (required): wave-prefixed short ID (e.g., "W1-T1")
fn handle_exec_retry(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let task_id_str = match req.params.get("task_id").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return Response::error(id, "missing required parameter: task_id"),
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 1. Find the task by wave-prefixed short ID
    let task = match db::work_items::find_work_item_by_wave_short_id(&conn, &task_id_str) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: task '{}' not found", task_id_str))
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 2. Validate the task is in Failed state
    if task.status != WorkItemStatus::Failed {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: task '{}' is in '{}' state, expected 'failed'",
                task_id_str, task.status
            ),
        );
    }

    // Validate it's actually a task
    if task.item_type != ItemType::Task {
        return Response::error(
            id,
            &format!("INVALID_PARAMS: '{}' is not a task", task_id_str),
        );
    }

    // 3. Find the parent story
    let story_id = match task.parent_id {
        Some(sid) => sid,
        None => {
            return Response::error(
                id,
                &format!("INVALID_STATE: task '{}' has no parent story", task_id_str),
            )
        }
    };

    let story = match db::work_items::get_work_item_by_id(&conn, &story_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(
                id,
                &format!(
                    "NOT_FOUND: parent story for task '{}' not found",
                    task_id_str
                ),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 4. For impl tasks: reset worktree to previous task's commit_hash (or base if first)
    let is_impl = task.kind == Some(TaskKind::Impl);
    if is_impl {
        if let Some(ref worktree_path) = story.worktree_path {
            // Find the previous impl task in this story (lower sort_order, impl kind, done status)
            let sibling_tasks = match db::work_items::list_work_items_by_parent(&conn, &story_id) {
                Ok(t) => t,
                Err(e) => return Response::error(id, &format!("database error: {}", e)),
            };

            let previous_commit: Option<String> = sibling_tasks
                .iter()
                .filter(|t| {
                    t.item_type == ItemType::Task
                        && t.kind == Some(TaskKind::Impl)
                        && t.sort_order < task.sort_order
                        && t.status == WorkItemStatus::Done
                })
                .max_by_key(|t| t.sort_order)
                .and_then(|t| t.commit_hash.clone());

            // Reset worktree: to previous task's commit or to base commit
            let reset_target = if let Some(ref commit) = previous_commit {
                commit.clone()
            } else {
                // Get base commit (the commit the worktree was created from)
                let output = std::process::Command::new("git")
                    .args(["reflog", "show", "HEAD", "--format=%H"])
                    .current_dir(worktree_path)
                    .output();

                match output {
                    Ok(out) if out.status.success() => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        match stdout.trim().lines().last() {
                            Some(hash) => hash.trim().to_string(),
                            None => {
                                return Response::error(
                                    id,
                                    "failed to determine base commit for worktree reset",
                                )
                            }
                        }
                    }
                    _ => {
                        return Response::error(
                            id,
                            "failed to determine base commit for worktree reset",
                        )
                    }
                }
            };

            // git reset --hard <commit>
            let reset_result = std::process::Command::new("git")
                .args(["reset", "--hard", &reset_target])
                .current_dir(worktree_path)
                .output();

            if let Err(e) = reset_result {
                return Response::error(id, &format!("failed to reset worktree: {}", e));
            }
            let reset_output = reset_result.unwrap();
            if !reset_output.status.success() {
                let stderr = String::from_utf8_lossy(&reset_output.stderr);
                return Response::error(
                    id,
                    &format!("failed to reset worktree: {}", stderr.trim()),
                );
            }

            // git clean -fd
            let _ = std::process::Command::new("git")
                .args(["clean", "-fd"])
                .current_dir(worktree_path)
                .output();
        }
    }

    // 5. Count retries
    let retry_count = match db::agent_runs::count_agent_runs_for_task(&conn, &task.id) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let warning = if retry_count >= 3 {
        Some(format!(
            "task has been retried {} times already",
            retry_count
        ))
    } else {
        None
    };

    // 6. Set task status to in_progress
    if let Err(e) =
        db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::InProgress)
    {
        return Response::error(id, &format!("database error: {}", e));
    }

    // 7. Set story status back to in_progress (if it was failed)
    if story.status == WorkItemStatus::Failed {
        if let Err(e) =
            db::work_items::update_work_item_status(&conn, &story_id, WorkItemStatus::InProgress)
        {
            return Response::error(id, &format!("database error: {}", e));
        }
    }

    // 8. Find project for agent spawning
    let session = match db::decomposition_sessions::get_decomposition_session(
        &conn,
        &task.decomposition_session_id,
    ) {
        Ok(Some(s)) => s,
        Ok(None) => return Response::error(id, "NOT_FOUND: decomposition session not found"),
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_id(&conn, &session.project_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Response::error(id, "NOT_FOUND: project not found"),
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 9. Spawn Claude agent (sync — handlers run in spawn_blocking)
    let worktree_path = match &story.worktree_path {
        Some(p) => PathBuf::from(p),
        None => {
            return Response::error(
                id,
                &format!(
                    "INVALID_STATE: story has no worktree, cannot retry task '{}'",
                    task_id_str,
                ),
            )
        }
    };

    let nflow_home = match crate::daemon::nflow_home() {
        Ok(h) => h,
        Err(e) => return Response::error(id, &format!("failed to get nflow home: {}", e)),
    };

    let is_verify = task.kind == Some(TaskKind::Verify);
    let override_dir = nflow_home.join("prompts");
    let override_path = if override_dir.is_dir() {
        Some(override_dir)
    } else {
        None
    };

    let template_name = if is_verify {
        "verify_task"
    } else {
        "task_execution"
    };
    let template =
        match nflow_claude::prompt::load_template(template_name, override_path.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                return Response::error(id, &format!("failed to load prompt template: {}", e))
            }
        };

    // Build template context
    let sibling_tasks = match db::work_items::list_work_items_by_parent(&conn, &story_id) {
        Ok(t) => t,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let vars_map = if is_verify {
        let impl_task = sibling_tasks
            .iter()
            .find(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Impl)
                    && t.parent_id == task.parent_id
                    && t.sort_order == task.sort_order - 1
            })
            .unwrap_or(&task);
        nflow_claude::context::build_verify_context(&task, impl_task, &project.name)
    } else {
        let completed: Vec<_> = sibling_tasks
            .iter()
            .filter(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Impl)
                    && t.status == WorkItemStatus::Done
            })
            .cloned()
            .collect();

        let previous_error = match db::agent_runs::find_latest_agent_run_for_task(&conn, &task.id) {
            Ok(Some(run)) => run.error_message.unwrap_or_default(),
            _ => String::new(),
        };

        nflow_claude::context::build_task_context(&task, &project.name, &completed, &previous_error)
    };

    let vars_ref: std::collections::HashMap<&str, &str> = vars_map
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let rendered_prompt = match nflow_claude::prompt::render_template(&template, &vars_ref) {
        Ok(p) => p,
        Err(e) => return Response::error(id, &format!("failed to render prompt: {}", e)),
    };

    let agent_logs_dir = nflow_home
        .join("projects")
        .join(&project.name)
        .join("agent-logs");
    if let Err(e) = std::fs::create_dir_all(&agent_logs_dir) {
        return Response::error(id, &format!("failed to create agent-logs dir: {}", e));
    }

    let prompt_file_path = agent_logs_dir.join(format!("{}.prompt.md", task.short_id));
    if let Err(e) = std::fs::write(&prompt_file_path, &rendered_prompt) {
        return Response::error(id, &format!("failed to write prompt file: {}", e));
    }

    let task_prompt = format!(
        "Implement the task as described in the system prompt file. Task: [{}] {}",
        task.short_id, task.title
    );

    let mut run_config = if is_verify {
        nflow_claude::runner::RunConfig::for_verify_task(task_prompt)
    } else {
        nflow_claude::runner::RunConfig::for_impl_task(task_prompt)
    };
    run_config.working_dir = Some(worktree_path);
    run_config.system_prompt_file = Some(prompt_file_path);

    let runner = nflow_claude::runner::ClaudeRunner::new();
    let process = match runner.spawn(&run_config) {
        Ok(p) => p,
        Err(e) => {
            // Revert task to failed
            let _ =
                db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::Failed);
            return Response::error(id, &format!("failed to spawn Claude agent: {}", e));
        }
    };

    let pid = process.pid;
    let pid_start_time = crate::recovery::read_process_start_time(pid);

    let log_path = agent_logs_dir.join(format!("{}.log", task.short_id));
    let log_path_str = log_path.to_string_lossy().to_string();

    let agent_run = db::agent_runs::AgentRun {
        id: uuid::Uuid::new_v4(),
        work_item_id: task.id,
        pid: Some(pid),
        session_id: None,
        pid_start_time,
        status: db::agent_runs::AgentRunStatus::Running,
        exit_code: None,
        log_path: Some(log_path_str),
        error_message: None,
        started_at: chrono::Utc::now(),
        finished_at: None,
    };

    if let Err(e) = db::agent_runs::insert_agent_run(&conn, &agent_run) {
        return Response::error(id, &format!("database error: {}", e));
    }

    // Spawn background task to pipe stdout to log file
    let mut stdout = process.stdout;
    let _stderr = process.stderr;
    let _child = process.child;

    tokio::spawn(async move {
        let log_file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut writer = tokio::io::BufWriter::new(log_file);

        while let Ok(Some(line)) = stdout.next_line().await {
            use tokio::io::AsyncWriteExt;
            let _ = writer.write_all(line.as_bytes()).await;
            let _ = writer.write_all(b"\n").await;
            let _ = writer.flush().await;
        }
    });

    // 10. Return response
    let mut response_data = serde_json::json!({
        "task_id": task_id_str,
        "retry_count": retry_count + 1,
    });

    if let Some(ref w) = warning {
        response_data["warning"] = serde_json::json!(w);
    }

    Response::ok(id, response_data)
}

/// Handle "exec.skip" command.
///
/// Skips a failed task by marking it as done with skipped metadata.
/// If impl task: also marks the paired verify task as skipped/cancelled.
/// Resumes story execution from the next pending task, or triggers story completion.
///
/// Params:
///   - task_id (required): wave-prefixed short ID (e.g., "W1-T1")
fn handle_exec_skip(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let task_id_str = match req.params.get("task_id").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return Response::error(id, "missing required parameter: task_id"),
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 1. Find the task by wave-prefixed short ID
    let task = match db::work_items::find_work_item_by_wave_short_id(&conn, &task_id_str) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Response::error(id, &format!("NOT_FOUND: task '{}' not found", task_id_str))
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 2. Validate the task is in Failed state
    if task.status != WorkItemStatus::Failed {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: task '{}' is in '{}' state, expected 'failed'",
                task_id_str, task.status
            ),
        );
    }

    // Validate it's actually a task
    if task.item_type != ItemType::Task {
        return Response::error(
            id,
            &format!("INVALID_PARAMS: '{}' is not a task", task_id_str),
        );
    }

    // 3. Find the parent story
    let story_id = match task.parent_id {
        Some(sid) => sid,
        None => {
            return Response::error(
                id,
                &format!("INVALID_STATE: task '{}' has no parent story", task_id_str),
            )
        }
    };

    let story = match db::work_items::get_work_item_by_id(&conn, &story_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(
                id,
                &format!(
                    "NOT_FOUND: parent story for task '{}' not found",
                    task_id_str
                ),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 4. Mark the task as done (skipped) using nflow-core skip logic
    // For impl tasks, also handle paired verify task
    if let Err(e) = db::work_items::update_work_item_status(&conn, &task.id, WorkItemStatus::Done) {
        return Response::error(id, &format!("database error: {}", e));
    }

    // Track which verify task was skipped (if any)
    let mut skipped_verify_id: Option<String> = None;

    if task.kind == Some(TaskKind::Impl) {
        // Find and skip/cancel the paired verify task
        let sibling_tasks = match db::work_items::list_work_items_by_parent(&conn, &story_id) {
            Ok(t) => t,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let verify = sibling_tasks.iter().find(|t| {
            t.item_type == ItemType::Task
                && t.kind == Some(TaskKind::Verify)
                && t.parent_id == task.parent_id
                && t.sort_order == task.sort_order + 1
        });

        if let Some(verify_task) = verify {
            let new_status = match verify_task.status {
                WorkItemStatus::Pending => WorkItemStatus::Cancelled,
                WorkItemStatus::Failed => WorkItemStatus::Done,
                _ => verify_task.status,
            };
            if new_status != verify_task.status {
                if let Err(e) =
                    db::work_items::update_work_item_status(&conn, &verify_task.id, new_status)
                {
                    return Response::error(id, &format!("database error: {}", e));
                }
                skipped_verify_id = Some(verify_task.short_id.clone());
            }
        }
    }

    // 5. Determine what happens next for the story
    // Re-read tasks after updates
    let tasks = match db::work_items::list_work_items_by_parent(&conn, &story_id) {
        Ok(t) => t,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let next_pending = nflow_core::work_item::get_next_pending_task(story_id, &tasks);

    if let Some(next_task) = next_pending {
        // Mark the next task as InProgress
        if let Err(e) = db::work_items::update_work_item_status(
            &conn,
            &next_task.id,
            WorkItemStatus::InProgress,
        ) {
            return Response::error(id, &format!("database error: {}", e));
        }

        // If story was failed, restore to in_progress
        if story.status == WorkItemStatus::Failed {
            if let Err(e) = db::work_items::update_work_item_status(
                &conn,
                &story_id,
                WorkItemStatus::InProgress,
            ) {
                return Response::error(id, &format!("database error: {}", e));
            }
        }

        // Spawn Claude agent for next task
        let session = match db::decomposition_sessions::get_decomposition_session(
            &conn,
            &task.decomposition_session_id,
        ) {
            Ok(Some(s)) => s,
            Ok(None) => return Response::error(id, "NOT_FOUND: decomposition session not found"),
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let project = match db::projects::get_project_by_id(&conn, &session.project_id) {
            Ok(Some(p)) => p,
            Ok(None) => return Response::error(id, "NOT_FOUND: project not found"),
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let worktree_path = match &story.worktree_path {
            Some(p) => PathBuf::from(p),
            None => {
                // Build response without spawning agent
                let mut data = serde_json::json!({
                    "task_id": task_id_str,
                    "next_task_id": next_task.short_id,
                });
                if let Some(ref vid) = skipped_verify_id {
                    data["skipped_verify"] = serde_json::json!(vid);
                }
                return Response::ok(id, data);
            }
        };

        let nflow_home = match crate::daemon::nflow_home() {
            Ok(h) => h,
            Err(e) => return Response::error(id, &format!("failed to get nflow home: {}", e)),
        };

        let is_verify = next_task.kind == Some(TaskKind::Verify);
        let override_dir = nflow_home.join("prompts");
        let override_path = if override_dir.is_dir() {
            Some(override_dir)
        } else {
            None
        };

        let template_name = if is_verify {
            "verify_task"
        } else {
            "task_execution"
        };
        let template =
            match nflow_claude::prompt::load_template(template_name, override_path.as_deref()) {
                Ok(t) => t,
                Err(e) => {
                    return Response::error(id, &format!("failed to load prompt template: {}", e))
                }
            };

        let all_tasks = match db::work_items::list_work_items_by_parent(&conn, &story_id) {
            Ok(t) => t,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let vars_map = if is_verify {
            let impl_task = all_tasks
                .iter()
                .find(|t| {
                    t.item_type == ItemType::Task
                        && t.kind == Some(TaskKind::Impl)
                        && t.parent_id == next_task.parent_id
                        && t.sort_order == next_task.sort_order - 1
                })
                .unwrap_or(next_task);
            nflow_claude::context::build_verify_context(next_task, impl_task, &project.name)
        } else {
            let completed: Vec<_> = all_tasks
                .iter()
                .filter(|t| {
                    t.item_type == ItemType::Task
                        && t.kind == Some(TaskKind::Impl)
                        && t.status == WorkItemStatus::Done
                        && t.id != next_task.id
                })
                .cloned()
                .collect();

            nflow_claude::context::build_task_context(next_task, &project.name, &completed, "")
        };

        let vars_ref: std::collections::HashMap<&str, &str> = vars_map
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let rendered_prompt = match nflow_claude::prompt::render_template(&template, &vars_ref) {
            Ok(p) => p,
            Err(e) => return Response::error(id, &format!("failed to render prompt: {}", e)),
        };

        let agent_logs_dir = nflow_home
            .join("projects")
            .join(&project.name)
            .join("agent-logs");
        if let Err(e) = std::fs::create_dir_all(&agent_logs_dir) {
            return Response::error(id, &format!("failed to create agent-logs dir: {}", e));
        }

        let prompt_file_path = agent_logs_dir.join(format!("{}.prompt.md", next_task.short_id));
        if let Err(e) = std::fs::write(&prompt_file_path, &rendered_prompt) {
            return Response::error(id, &format!("failed to write prompt file: {}", e));
        }

        let task_prompt = format!(
            "Implement the task as described in the system prompt file. Task: [{}] {}",
            next_task.short_id, next_task.title
        );

        let mut run_config = if is_verify {
            nflow_claude::runner::RunConfig::for_verify_task(task_prompt)
        } else {
            nflow_claude::runner::RunConfig::for_impl_task(task_prompt)
        };
        run_config.working_dir = Some(worktree_path);
        run_config.system_prompt_file = Some(prompt_file_path);

        let runner = nflow_claude::runner::ClaudeRunner::new();
        let process = match runner.spawn(&run_config) {
            Ok(p) => p,
            Err(e) => {
                // Revert next task to pending
                let _ = db::work_items::update_work_item_status(
                    &conn,
                    &next_task.id,
                    WorkItemStatus::Pending,
                );
                return Response::error(id, &format!("failed to spawn Claude agent: {}", e));
            }
        };

        let pid = process.pid;
        let pid_start_time = crate::recovery::read_process_start_time(pid);

        let log_path = agent_logs_dir.join(format!("{}.log", next_task.short_id));
        let log_path_str = log_path.to_string_lossy().to_string();

        let agent_run = db::agent_runs::AgentRun {
            id: uuid::Uuid::new_v4(),
            work_item_id: next_task.id,
            pid: Some(pid),
            session_id: None,
            pid_start_time,
            status: db::agent_runs::AgentRunStatus::Running,
            exit_code: None,
            log_path: Some(log_path_str),
            error_message: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
        };

        if let Err(e) = db::agent_runs::insert_agent_run(&conn, &agent_run) {
            return Response::error(id, &format!("database error: {}", e));
        }

        // Spawn background task to pipe stdout to log file
        let mut stdout = process.stdout;
        let _stderr = process.stderr;
        let _child = process.child;

        tokio::spawn(async move {
            let log_file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .await
            {
                Ok(f) => f,
                Err(_) => return,
            };
            let mut writer = tokio::io::BufWriter::new(log_file);

            while let Ok(Some(line)) = stdout.next_line().await {
                use tokio::io::AsyncWriteExt;
                let _ = writer.write_all(line.as_bytes()).await;
                let _ = writer.write_all(b"\n").await;
                let _ = writer.flush().await;
            }
        });

        let mut data = serde_json::json!({
            "task_id": task_id_str,
            "next_task_id": next_task.short_id,
        });
        if let Some(ref vid) = skipped_verify_id {
            data["skipped_verify"] = serde_json::json!(vid);
        }
        return Response::ok(id, data);
    }

    // No more pending tasks — check if all tasks are done/cancelled
    let all_finished = tasks
        .iter()
        .filter(|t| t.item_type == ItemType::Task)
        .all(|t| t.status == WorkItemStatus::Done || t.status == WorkItemStatus::Cancelled);

    if all_finished {
        // Mark story as Done
        if let Err(e) =
            db::work_items::update_work_item_status(&conn, &story_id, WorkItemStatus::Done)
        {
            return Response::error(id, &format!("database error: {}", e));
        }

        // Trigger story completion (rebase, push, MR)
        // This will be handled by the scheduler loop when it sees the story is Done
        let mut data = serde_json::json!({
            "task_id": task_id_str,
            "story_completed": true,
        });
        if let Some(ref vid) = skipped_verify_id {
            data["skipped_verify"] = serde_json::json!(vid);
        }
        return Response::ok(id, data);
    }

    // Fallback — task skipped but story state is unclear
    let mut data = serde_json::json!({
        "task_id": task_id_str,
    });
    if let Some(ref vid) = skipped_verify_id {
        data["skipped_verify"] = serde_json::json!(vid);
    }
    Response::ok(id, data)
}

/// Handle "exec.continue" command.
///
/// Resumes a failed or cancelled story. For failed stories, skips the current
/// failed task and resumes from the next pending task. For cancelled stories,
/// restores them to the appropriate state based on worktree existence and
/// dependency state.
///
/// Params: story_id (required, wave-prefixed like "W1-S1"), force (optional bool)
fn handle_exec_continue(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let story_id_str = match req.params.get("story_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Response::error(id, "missing required parameter: story_id"),
    };

    let force = req
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 1. Find the story by wave-prefixed short ID
    let story = match db::work_items::find_work_item_by_wave_short_id(&conn, &story_id_str) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: story '{}' not found", story_id_str),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Validate it's a story
    if story.item_type != ItemType::Story {
        return Response::error(
            id,
            &format!("INVALID_PARAMS: '{}' is not a story", story_id_str),
        );
    }

    // 2. Validate story is in failed or cancelled state
    if story.status != WorkItemStatus::Failed && story.status != WorkItemStatus::Cancelled {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: story '{}' is in '{}' state, expected 'failed' or 'cancelled'",
                story_id_str, story.status
            ),
        );
    }

    // 3. For cancelled stories, check dependents and warn if not forced
    if story.status == WorkItemStatus::Cancelled && !force {
        let dependents = match db::work_items::list_dependents_of_story(&conn, &story.id) {
            Ok(d) => d,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };
        if !dependents.is_empty() {
            return Response::error(
                id,
                &format!(
                    "INVALID_STATE: story '{}' has {} dependent(s) that may be affected. Use --force to skip this check",
                    story_id_str,
                    dependents.len()
                ),
            );
        }
    }

    // Handle based on story state
    match story.status {
        WorkItemStatus::Failed => handle_continue_failed_story(&conn, id, &story_id_str, &story),
        WorkItemStatus::Cancelled => {
            handle_continue_cancelled_story(&conn, id, &story_id_str, &story)
        }
        _ => unreachable!(), // Already validated above
    }
}

/// Continue a failed story: skip the failed task, resume from next pending.
fn handle_continue_failed_story(
    conn: &rusqlite::Connection,
    id: String,
    story_id_str: &str,
    story: &WorkItem,
) -> Response {
    // Find the current failed task in this story
    let tasks = match db::work_items::list_work_items_by_parent(conn, &story.id) {
        Ok(t) => t,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let failed_task = tasks
        .iter()
        .find(|t| t.item_type == ItemType::Task && t.status == WorkItemStatus::Failed);

    if let Some(failed_task) = failed_task {
        // Mark the failed task as done (skipped)
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &failed_task.id, WorkItemStatus::Done)
        {
            return Response::error(id, &format!("database error: {}", e));
        }

        // If it's an impl task, handle paired verify
        if failed_task.kind == Some(TaskKind::Impl) {
            let verify = tasks.iter().find(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Verify)
                    && t.parent_id == failed_task.parent_id
                    && t.sort_order == failed_task.sort_order + 1
            });

            if let Some(verify_task) = verify {
                let new_status = match verify_task.status {
                    WorkItemStatus::Pending => WorkItemStatus::Cancelled,
                    WorkItemStatus::Failed => WorkItemStatus::Done,
                    _ => verify_task.status,
                };
                if new_status != verify_task.status {
                    let _ =
                        db::work_items::update_work_item_status(conn, &verify_task.id, new_status);
                }
            }
        }
    }

    // Re-read tasks after updates
    let tasks = match db::work_items::list_work_items_by_parent(conn, &story.id) {
        Ok(t) => t,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let next_pending = nflow_core::work_item::get_next_pending_task(story.id, &tasks);

    if let Some(next_task) = next_pending {
        // Mark next task as InProgress
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &next_task.id, WorkItemStatus::InProgress)
        {
            return Response::error(id, &format!("database error: {}", e));
        }

        // Restore story to InProgress
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &story.id, WorkItemStatus::InProgress)
        {
            return Response::error(id, &format!("database error: {}", e));
        }

        // Spawn Claude agent for next task if worktree exists
        if let Some(ref _worktree_path) = story.worktree_path {
            if let Err(e) = spawn_agent_for_task(conn, next_task, story) {
                // Revert next task to pending on spawn failure
                let _ = db::work_items::update_work_item_status(
                    conn,
                    &next_task.id,
                    WorkItemStatus::Pending,
                );
                return Response::error(id, &format!("failed to spawn agent: {}", e));
            }
        }

        return Response::ok(
            id,
            serde_json::json!({
                "story_id": story_id_str,
                "next_task_id": next_task.short_id,
                "resumed": true,
            }),
        );
    }

    // No pending tasks — check if all tasks are done/cancelled
    let all_finished = tasks
        .iter()
        .filter(|t| t.item_type == ItemType::Task)
        .all(|t| t.status == WorkItemStatus::Done || t.status == WorkItemStatus::Cancelled);

    if all_finished {
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &story.id, WorkItemStatus::Done)
        {
            return Response::error(id, &format!("database error: {}", e));
        }
    }

    Response::ok(
        id,
        serde_json::json!({
            "story_id": story_id_str,
            "resumed": all_finished,
        }),
    )
}

/// Continue a cancelled story: restore to appropriate state based on worktree and dependencies.
fn handle_continue_cancelled_story(
    conn: &rusqlite::Connection,
    id: String,
    story_id_str: &str,
    story: &WorkItem,
) -> Response {
    if story.worktree_path.is_some() {
        // Has worktree: set to in_progress, resume from first pending task
        if let Err(e) =
            db::work_items::update_work_item_status(conn, &story.id, WorkItemStatus::InProgress)
        {
            return Response::error(id, &format!("database error: {}", e));
        }

        // Find and start the first pending task
        let tasks = match db::work_items::list_work_items_by_parent(conn, &story.id) {
            Ok(t) => t,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        // Restore cancelled tasks to pending
        for task in &tasks {
            if task.item_type == ItemType::Task && task.status == WorkItemStatus::Cancelled {
                let _ = db::work_items::update_work_item_status(
                    conn,
                    &task.id,
                    WorkItemStatus::Pending,
                );
            }
        }

        // Re-read tasks after restoring
        let tasks = match db::work_items::list_work_items_by_parent(conn, &story.id) {
            Ok(t) => t,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let next_pending = nflow_core::work_item::get_next_pending_task(story.id, &tasks);

        if let Some(next_task) = next_pending {
            if let Err(e) = db::work_items::update_work_item_status(
                conn,
                &next_task.id,
                WorkItemStatus::InProgress,
            ) {
                return Response::error(id, &format!("database error: {}", e));
            }

            return Response::ok(
                id,
                serde_json::json!({
                    "story_id": story_id_str,
                    "next_task_id": next_task.short_id,
                    "resumed": true,
                }),
            );
        }

        Response::ok(
            id,
            serde_json::json!({
                "story_id": story_id_str,
                "resumed": true,
            }),
        )
    } else {
        // No worktree: set to ready or pending based on dependency state
        let blockers = match db::work_items::list_blockers_for_story(conn, &story.id) {
            Ok(b) => b,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let all_blockers_done = if blockers.is_empty() {
            true
        } else {
            blockers.iter().all(|dep| {
                match db::work_items::get_work_item_by_id(conn, &dep.blocker_id) {
                    Ok(Some(blocker)) => {
                        blocker.status == WorkItemStatus::Done
                            || blocker.status == WorkItemStatus::Cancelled
                    }
                    _ => false,
                }
            })
        };

        // Restore cancelled tasks to pending
        let tasks = match db::work_items::list_work_items_by_parent(conn, &story.id) {
            Ok(t) => t,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        for task in &tasks {
            if task.item_type == ItemType::Task && task.status == WorkItemStatus::Cancelled {
                let _ = db::work_items::update_work_item_status(
                    conn,
                    &task.id,
                    WorkItemStatus::Pending,
                );
            }
        }

        let new_status = if all_blockers_done {
            WorkItemStatus::Ready
        } else {
            WorkItemStatus::Pending
        };

        if let Err(e) = db::work_items::update_work_item_status(conn, &story.id, new_status) {
            return Response::error(id, &format!("database error: {}", e));
        }

        Response::ok(
            id,
            serde_json::json!({
                "story_id": story_id_str,
                "resumed": false,
            }),
        )
    }
}

/// Handle "exec.stop" command.
///
/// Stops running stories by sending SIGTERM to their agent processes.
///
/// Params:
///   - story_id (optional): wave-prefixed short ID like "W1-S1" to stop a single story
///   - wave (optional): wave number to stop all running stories in that wave
///   - project_name (optional): required when no story_id or wave is given; stops all agents in that project
///   - all (optional bool): stop all running agents across all projects
fn handle_exec_stop(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let story_id_str: Option<String> = req
        .params
        .get("story_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let wave_filter: Option<u32> = req
        .params
        .get("wave")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let project_name: Option<String> = req
        .params
        .get("project_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let stop_all = req
        .params
        .get("all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Case 1: --all flag — stop all running agents across all projects
    if stop_all {
        let running = match db::agent_runs::find_running_agent_runs(&conn) {
            Ok(r) => r,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let mut stopped_count = 0u32;
        let mut story_ids_stopped = std::collections::HashSet::new();

        for run in &running {
            // Send SIGTERM to the agent process
            if let Some(pid) = run.pid {
                terminate_agent_process_sigterm(pid);
            }
            // Mark agent run as cancelled in DB
            let _ = db::agent_runs::update_agent_run_status(
                &conn,
                &run.id,
                db::agent_runs::AgentRunStatus::Cancelled,
                None,
                Some("stopped by user"),
                Some(chrono::Utc::now()),
            );
            // Mark the task as failed
            let _ = db::work_items::update_work_item_status(
                &conn,
                &run.work_item_id,
                WorkItemStatus::Failed,
            );
            stopped_count += 1;

            // Find parent story to cancel
            if let Ok(Some(task)) = db::work_items::get_work_item_by_id(&conn, &run.work_item_id) {
                if let Some(parent_id) = task.parent_id {
                    story_ids_stopped.insert(parent_id);
                }
            }
        }

        // Cancel all stories that had running agents
        for story_id in &story_ids_stopped {
            let _ =
                db::work_items::update_work_item_status(&conn, story_id, WorkItemStatus::Cancelled);
            // Cancel pending tasks in those stories
            cancel_pending_tasks_in_story(&conn, story_id);
        }

        return Response::ok(
            id,
            serde_json::json!({
                "agents_stopped": stopped_count,
                "stories_cancelled": story_ids_stopped.len(),
            }),
        );
    }

    // Case 2: Single story by story_id
    if let Some(ref story_id_str) = story_id_str {
        let story = match db::work_items::find_work_item_by_wave_short_id(&conn, story_id_str) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Response::error(
                    id,
                    &format!("NOT_FOUND: story '{}' not found", story_id_str),
                )
            }
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        if story.item_type != ItemType::Story {
            return Response::error(
                id,
                &format!("INVALID_PARAMS: '{}' is not a story", story_id_str),
            );
        }

        if story.status != WorkItemStatus::InProgress && story.status != WorkItemStatus::Ready {
            return Response::error(
                id,
                &format!(
                    "INVALID_STATE: story '{}' is in '{}' state, expected 'in_progress' or 'ready'",
                    story_id_str, story.status
                ),
            );
        }

        let stopped = stop_story_agents(&conn, &story);

        return Response::ok(
            id,
            serde_json::json!({
                "story_id": story_id_str,
                "agents_stopped": stopped,
            }),
        );
    }

    // Case 3: Wave-level stop — requires project_name
    if let Some(wave) = wave_filter {
        let project_name = match project_name {
            Some(n) => n,
            None => {
                return Response::error(
                    id,
                    "missing required parameter: project_name (required with --wave)",
                )
            }
        };

        let project = match db::projects::get_project_by_name(&conn, &project_name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Response::error(
                    id,
                    &format!("NOT_FOUND: project '{}' not found", project_name),
                )
            }
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let session =
            match db::decomposition_sessions::get_session_by_wave(&conn, &project.id, wave) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    return Response::error(id, &format!("NOT_FOUND: wave W{} not found", wave))
                }
                Err(e) => return Response::error(id, &format!("database error: {}", e)),
            };

        let stories = match db::work_items::list_stories_by_session(&conn, &session.id) {
            Ok(s) => s,
            Err(e) => return Response::error(id, &format!("database error: {}", e)),
        };

        let mut total_stopped = 0u32;
        let mut stories_cancelled = 0u32;

        for story in &stories {
            if story.status == WorkItemStatus::InProgress {
                let stopped = stop_story_agents(&conn, story);
                total_stopped += stopped;
                stories_cancelled += 1;
            }
        }

        return Response::ok(
            id,
            serde_json::json!({
                "wave": wave,
                "agents_stopped": total_stopped,
                "stories_cancelled": stories_cancelled,
            }),
        );
    }

    // Case 4: Project-level stop — all running agents in a single project
    let project_name =
        match project_name {
            Some(n) => n,
            None => return Response::error(
                id,
                "missing required parameter: project_name (or provide story_id, --wave, or --all)",
            ),
        };

    let project = match db::projects::get_project_by_name(&conn, &project_name) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: project '{}' not found", project_name),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let running = match db::agent_runs::find_running_agent_runs_by_project(&conn, &project.id) {
        Ok(r) => r,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let mut stopped_count = 0u32;
    let mut story_ids_stopped = std::collections::HashSet::new();

    for run in &running {
        if let Some(pid) = run.pid {
            terminate_agent_process_sigterm(pid);
        }
        let _ = db::agent_runs::update_agent_run_status(
            &conn,
            &run.id,
            db::agent_runs::AgentRunStatus::Cancelled,
            None,
            Some("stopped by user"),
            Some(chrono::Utc::now()),
        );
        let _ = db::work_items::update_work_item_status(
            &conn,
            &run.work_item_id,
            WorkItemStatus::Failed,
        );
        stopped_count += 1;

        if let Ok(Some(task)) = db::work_items::get_work_item_by_id(&conn, &run.work_item_id) {
            if let Some(parent_id) = task.parent_id {
                story_ids_stopped.insert(parent_id);
            }
        }
    }

    for story_id in &story_ids_stopped {
        let _ = db::work_items::update_work_item_status(&conn, story_id, WorkItemStatus::Cancelled);
        cancel_pending_tasks_in_story(&conn, story_id);
    }

    Response::ok(
        id,
        serde_json::json!({
            "agents_stopped": stopped_count,
            "stories_cancelled": story_ids_stopped.len(),
        }),
    )
}

/// Handle "exec.cancel" command.
///
/// Cancels a pending or ready story (no process termination needed).
/// Warns if the cancelled story has dependents that will be blocked.
///
/// Params: story_id (required, wave-prefixed like "W1-S1")
fn handle_exec_cancel(req: Request, state: &HandlerState) -> Response {
    let id = req.id.clone();

    let story_id_str = match req.params.get("story_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Response::error(id, "missing required parameter: story_id"),
    };

    let conn = match db::open_connection(&state.db_path) {
        Ok(c) => c,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // 1. Find the story by wave-prefixed short ID
    let story = match db::work_items::find_work_item_by_wave_short_id(&conn, &story_id_str) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Response::error(
                id,
                &format!("NOT_FOUND: story '{}' not found", story_id_str),
            )
        }
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    // Validate it's a story
    if story.item_type != ItemType::Story {
        return Response::error(
            id,
            &format!("INVALID_PARAMS: '{}' is not a story", story_id_str),
        );
    }

    // 2. Validate story is in pending or ready state
    if story.status != WorkItemStatus::Pending && story.status != WorkItemStatus::Ready {
        return Response::error(
            id,
            &format!(
                "INVALID_STATE: story '{}' is in '{}' state, expected 'pending' or 'ready'",
                story_id_str, story.status
            ),
        );
    }

    // 3. Check for dependents and warn
    let dependents = match db::work_items::list_dependents_of_story(&conn, &story.id) {
        Ok(d) => d,
        Err(e) => return Response::error(id, &format!("database error: {}", e)),
    };

    let has_dependents = !dependents.is_empty();

    // 4. Cancel the story
    if let Err(e) =
        db::work_items::update_work_item_status(&conn, &story.id, WorkItemStatus::Cancelled)
    {
        return Response::error(id, &format!("database error: {}", e));
    }

    // Cancel all pending tasks in the story
    cancel_pending_tasks_in_story(&conn, &story.id);

    let mut data = serde_json::json!({
        "story_id": story_id_str,
        "cancelled": true,
    });

    if has_dependents {
        let dependent_ids: Vec<String> = dependents
            .iter()
            .filter_map(|dep| {
                db::work_items::get_work_item_by_id(&conn, &dep.blocked_id)
                    .ok()
                    .flatten()
                    .map(|w| w.short_id)
            })
            .collect();
        data["warning"] = serde_json::json!(format!(
            "story has {} dependent(s) that will be blocked: {}",
            dependents.len(),
            dependent_ids.join(", ")
        ));
        data["blocked_dependents"] = serde_json::json!(dependent_ids);
    }

    Response::ok(id, data)
}

/// Send SIGTERM to an agent process (no wait, no escalation to SIGKILL).
/// Used by exec.stop handler for immediate termination signal.
fn terminate_agent_process_sigterm(pid: u32) {
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    match nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => {} // already dead
        Err(_) => {}
    }
}

/// Stop all running agents for a story, cancel the story, and cancel pending tasks.
/// Returns the number of agents that were stopped.
fn stop_story_agents(conn: &rusqlite::Connection, story: &WorkItem) -> u32 {
    let running = match db::agent_runs::find_running_agent_runs_for_story(conn, &story.id) {
        Ok(r) => r,
        Err(_) => return 0,
    };

    let mut stopped = 0u32;
    for run in &running {
        if let Some(pid) = run.pid {
            terminate_agent_process_sigterm(pid);
        }
        let _ = db::agent_runs::update_agent_run_status(
            conn,
            &run.id,
            db::agent_runs::AgentRunStatus::Cancelled,
            None,
            Some("stopped by user"),
            Some(chrono::Utc::now()),
        );
        let _ = db::work_items::update_work_item_status(
            conn,
            &run.work_item_id,
            WorkItemStatus::Failed,
        );
        stopped += 1;
    }

    // Cancel the story
    let _ = db::work_items::update_work_item_status(conn, &story.id, WorkItemStatus::Cancelled);

    // Cancel pending tasks in the story
    cancel_pending_tasks_in_story(conn, &story.id);

    stopped
}

/// Cancel all pending tasks in a story.
fn cancel_pending_tasks_in_story(conn: &rusqlite::Connection, story_id: &uuid::Uuid) {
    if let Ok(tasks) = db::work_items::list_work_items_by_parent(conn, story_id) {
        for task in &tasks {
            if task.item_type == ItemType::Task && task.status == WorkItemStatus::Pending {
                let _ = db::work_items::update_work_item_status(
                    conn,
                    &task.id,
                    WorkItemStatus::Cancelled,
                );
            }
        }
    }
}

/// Spawn a Claude agent for a task within a story's worktree.
/// Reusable helper for continue/skip/retry handlers.
fn spawn_agent_for_task(
    conn: &rusqlite::Connection,
    task: &WorkItem,
    story: &WorkItem,
) -> std::result::Result<(), String> {
    let session = match db::decomposition_sessions::get_decomposition_session(
        conn,
        &task.decomposition_session_id,
    ) {
        Ok(Some(s)) => s,
        Ok(None) => return Err("decomposition session not found".into()),
        Err(e) => return Err(format!("database error: {}", e)),
    };

    let project = match db::projects::get_project_by_id(conn, &session.project_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Err("project not found".into()),
        Err(e) => return Err(format!("database error: {}", e)),
    };

    let worktree_path = match &story.worktree_path {
        Some(p) => PathBuf::from(p),
        None => return Err("story has no worktree".into()),
    };

    let nflow_home = match crate::daemon::nflow_home() {
        Ok(h) => h,
        Err(e) => return Err(format!("failed to get nflow home: {}", e)),
    };

    let is_verify = task.kind == Some(TaskKind::Verify);
    let override_dir = nflow_home.join("prompts");
    let override_path = if override_dir.is_dir() {
        Some(override_dir)
    } else {
        None
    };

    let template_name = if is_verify {
        "verify_task"
    } else {
        "task_execution"
    };
    let template =
        match nflow_claude::prompt::load_template(template_name, override_path.as_deref()) {
            Ok(t) => t,
            Err(e) => return Err(format!("failed to load prompt template: {}", e)),
        };

    let all_tasks = match db::work_items::list_work_items_by_parent(conn, &story.id) {
        Ok(t) => t,
        Err(e) => return Err(format!("database error: {}", e)),
    };

    let vars_map = if is_verify {
        let impl_task = all_tasks
            .iter()
            .find(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Impl)
                    && t.parent_id == task.parent_id
                    && t.sort_order == task.sort_order - 1
            })
            .unwrap_or(task);
        nflow_claude::context::build_verify_context(task, impl_task, &project.name)
    } else {
        let completed: Vec<_> = all_tasks
            .iter()
            .filter(|t| {
                t.item_type == ItemType::Task
                    && t.kind == Some(TaskKind::Impl)
                    && t.status == WorkItemStatus::Done
                    && t.id != task.id
            })
            .cloned()
            .collect();
        nflow_claude::context::build_task_context(task, &project.name, &completed, "")
    };

    let vars_ref: std::collections::HashMap<&str, &str> = vars_map
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let rendered_prompt = match nflow_claude::prompt::render_template(&template, &vars_ref) {
        Ok(p) => p,
        Err(e) => return Err(format!("failed to render prompt: {}", e)),
    };

    let agent_logs_dir = nflow_home
        .join("projects")
        .join(&project.name)
        .join("agent-logs");
    if let Err(e) = std::fs::create_dir_all(&agent_logs_dir) {
        return Err(format!("failed to create agent-logs dir: {}", e));
    }

    let prompt_file_path = agent_logs_dir.join(format!("{}.prompt.md", task.short_id));
    if let Err(e) = std::fs::write(&prompt_file_path, &rendered_prompt) {
        return Err(format!("failed to write prompt file: {}", e));
    }

    let task_prompt = format!(
        "Implement the task as described in the system prompt file. Task: [{}] {}",
        task.short_id, task.title
    );

    let mut run_config = if is_verify {
        nflow_claude::runner::RunConfig::for_verify_task(task_prompt)
    } else {
        nflow_claude::runner::RunConfig::for_impl_task(task_prompt)
    };
    run_config.working_dir = Some(worktree_path);
    run_config.system_prompt_file = Some(prompt_file_path);

    let runner = nflow_claude::runner::ClaudeRunner::new();
    let process = match runner.spawn(&run_config) {
        Ok(p) => p,
        Err(e) => return Err(format!("failed to spawn Claude agent: {}", e)),
    };

    let pid = process.pid;
    let pid_start_time = crate::recovery::read_process_start_time(pid);

    let log_path = agent_logs_dir.join(format!("{}.log", task.short_id));
    let log_path_str = log_path.to_string_lossy().to_string();

    let agent_run = db::agent_runs::AgentRun {
        id: uuid::Uuid::new_v4(),
        work_item_id: task.id,
        pid: Some(pid),
        session_id: None,
        pid_start_time,
        status: db::agent_runs::AgentRunStatus::Running,
        exit_code: None,
        log_path: Some(log_path_str),
        error_message: None,
        started_at: chrono::Utc::now(),
        finished_at: None,
    };

    if let Err(e) = db::agent_runs::insert_agent_run(conn, &agent_run) {
        return Err(format!("database error: {}", e));
    }

    // Spawn background task to pipe stdout to log file
    let mut stdout = process.stdout;
    let _stderr = process.stderr;
    let _child = process.child;

    tokio::spawn(async move {
        let log_file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut writer = tokio::io::BufWriter::new(log_file);

        while let Ok(Some(line)) = stdout.next_line().await {
            use tokio::io::AsyncWriteExt;
            let _ = writer.write_all(line.as_bytes()).await;
            let _ = writer.write_all(b"\n").await;
            let _ = writer.flush().await;
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_parse_git_provider_github() {
        assert_eq!(parse_git_provider("github"), Some(GitProvider::Github));
        assert_eq!(parse_git_provider("Github"), Some(GitProvider::Github));
        assert_eq!(parse_git_provider("GITHUB"), Some(GitProvider::Github));
    }

    #[test]
    fn test_parse_git_provider_gitlab() {
        assert_eq!(parse_git_provider("gitlab"), Some(GitProvider::Gitlab));
        assert_eq!(parse_git_provider("GitLab"), Some(GitProvider::Gitlab));
    }

    #[test]
    fn test_parse_git_provider_unknown() {
        assert_eq!(parse_git_provider("bitbucket"), None);
        assert_eq!(parse_git_provider(""), None);
    }

    #[test]
    fn test_is_git_repo_nonexistent_path() {
        assert!(!is_git_repo(Path::new("/nonexistent/path/foo/bar")));
    }

    #[test]
    fn test_is_git_repo_on_workspace_root() {
        // The nflow workspace root is a git repo
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(is_git_repo(&workspace_root));
    }

    #[test]
    fn test_is_git_repo_temp_dir_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("nflow_test_not_git_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_git_repo(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_project_init_missing_name() {
        let state = make_test_state();
        let req = Request {
            id: "1".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({ "path": "/tmp" }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: name"));
    }

    #[test]
    fn test_handle_project_init_missing_path() {
        let state = make_test_state();
        let req = Request {
            id: "2".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({ "name": "myproject" }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: path"));
    }

    #[test]
    fn test_handle_project_init_not_a_git_repo() {
        let dir = std::env::temp_dir().join(format!("nflow_test_nogit_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let state = make_test_state();
        let req = Request {
            id: "3".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({
                "name": "testproject",
                "path": dir.to_string_lossy(),
            }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_PARAMS"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_project_init_success() {
        let dir = std::env::temp_dir().join(format!("nflow_test_init_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a git repo in the temp dir
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        // Set up a remote so git provider can be detected
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/user/repo.git",
            ])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        // Use in-memory DB for this test by creating a temp DB file
        let db_dir =
            std::env::temp_dir().join(format!("nflow_test_db_init_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("test.db");

        let conn = db::open_connection(&db_path).unwrap();
        db::run_migrations(&conn, &db_path).unwrap();
        drop(conn);

        let state = HandlerState {
            db_path: db_path.clone(),
        };

        let req = Request {
            id: "4".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({
                "name": "test-project",
                "path": dir.to_string_lossy(),
            }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "test-project");
        assert_eq!(result.data["path"], dir.to_string_lossy().as_ref());
        assert_eq!(result.data["base_branch"], "main");
        assert_eq!(result.data["git_provider"], "github");
        assert!(result.data["project_id"].is_string());

        // Verify the project was persisted in DB
        let conn = db::open_connection(&db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, "test-project")
            .unwrap()
            .unwrap();
        assert_eq!(project.name, "test-project");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    #[test]
    fn test_handle_project_init_duplicate_name() {
        let dir = std::env::temp_dir().join(format!("nflow_test_dup_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a git repo
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/user/repo.git",
            ])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let db_dir =
            std::env::temp_dir().join(format!("nflow_test_db_dup_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("test.db");

        let conn = db::open_connection(&db_path).unwrap();
        db::run_migrations(&conn, &db_path).unwrap();
        drop(conn);

        let state = HandlerState {
            db_path: db_path.clone(),
        };

        // Create first project
        let req = Request {
            id: "5".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({
                "name": "dup-project",
                "path": dir.to_string_lossy(),
            }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);

        // Try to create another project with the same name
        let req2 = Request {
            id: "6".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({
                "name": "dup-project",
                "path": dir.to_string_lossy(),
            }),
        };
        let result2 = handle_project_init(req2, &state);
        assert_eq!(result2.status, crate::socket::ResponseStatus::Error);
        assert!(result2.data["message"]
            .as_str()
            .unwrap()
            .contains("ALREADY_EXISTS"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    #[test]
    fn test_handle_project_init_with_explicit_provider() {
        let dir =
            std::env::temp_dir().join(format!("nflow_test_provider_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a git repo with no remote
        std::process::Command::new("git")
            .args(["init", "--initial-branch=develop"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let db_dir =
            std::env::temp_dir().join(format!("nflow_test_db_provider_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("test.db");

        let conn = db::open_connection(&db_path).unwrap();
        db::run_migrations(&conn, &db_path).unwrap();
        drop(conn);

        let state = HandlerState {
            db_path: db_path.clone(),
        };

        let req = Request {
            id: "7".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({
                "name": "gitlab-project",
                "path": dir.to_string_lossy(),
                "git_provider": "gitlab",
            }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["git_provider"], "gitlab");
        assert_eq!(result.data["base_branch"], "develop");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    #[test]
    fn test_handle_project_init_with_explicit_base_branch() {
        let dir = std::env::temp_dir().join(format!("nflow_test_branch_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a git repo
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/user/repo.git",
            ])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        let db_dir =
            std::env::temp_dir().join(format!("nflow_test_db_branch_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("test.db");

        let conn = db::open_connection(&db_path).unwrap();
        db::run_migrations(&conn, &db_path).unwrap();
        drop(conn);

        let state = HandlerState {
            db_path: db_path.clone(),
        };

        let req = Request {
            id: "8".to_string(),
            command: "project.init".to_string(),
            params: serde_json::json!({
                "name": "branch-project",
                "path": dir.to_string_lossy(),
                "base_branch": "release",
            }),
        };
        let result = handle_project_init(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["base_branch"], "release");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    #[test]
    fn test_dispatch_unknown_command() {
        let state = make_test_state();
        let req = Request {
            id: "99".to_string(),
            command: "unknown.command".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("unknown command"));
            }
            _ => panic!("expected Single response"),
        }
    }

    fn make_test_state() -> HandlerState {
        HandlerState {
            db_path: PathBuf::from("/nonexistent/test.db"),
        }
    }

    /// Create a test DB and return (state, db_dir) for cleanup.
    fn make_db_state(suffix: &str) -> (HandlerState, PathBuf) {
        let db_dir =
            std::env::temp_dir().join(format!("nflow_test_db_{}_{}", suffix, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("test.db");

        let conn = db::open_connection(&db_path).unwrap();
        db::run_migrations(&conn, &db_path).unwrap();
        drop(conn);

        let state = HandlerState {
            db_path: db_path.clone(),
        };
        (state, db_dir)
    }

    fn cleanup_db(db_dir: &Path) {
        let db_path = db_dir.join("test.db");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
        let _ = std::fs::remove_dir_all(db_dir);
    }

    /// Insert a project directly into the DB and return its UUID.
    fn insert_test_project(state: &HandlerState, name: &str) -> uuid::Uuid {
        use chrono::Utc;
        use nflow_core::project::Project;
        let conn = db::open_connection(&state.db_path).unwrap();
        let project = Project {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            path: "/tmp/fakepath".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        db::projects::insert_project(&conn, &project).unwrap();
        project.id
    }

    // --- project.list tests ---

    #[test]
    fn test_handle_project_list_empty() {
        let (state, db_dir) = make_db_state("list_empty");
        let req = Request {
            id: "10".to_string(),
            command: "project.list".to_string(),
            params: serde_json::json!({}),
        };
        let result = handle_project_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let projects = result.data["projects"].as_array().unwrap();
        assert!(projects.is_empty());
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_project_list_with_projects() {
        let (state, db_dir) = make_db_state("list_projects");
        insert_test_project(&state, "alpha");
        insert_test_project(&state, "beta");

        let req = Request {
            id: "11".to_string(),
            command: "project.list".to_string(),
            params: serde_json::json!({}),
        };
        let result = handle_project_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let projects = result.data["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 2);
        // Ordered by name
        assert_eq!(projects[0]["name"], "alpha");
        assert_eq!(projects[1]["name"], "beta");
        // Check all fields present
        assert_eq!(projects[0]["base_branch"], "main");
        assert_eq!(projects[0]["git_provider"], "github");
        assert_eq!(projects[0]["execution_enabled"], true);
        assert_eq!(projects[0]["spec_count"], 0);
        assert_eq!(projects[0]["story_count"], 0);
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_project_list_with_spec_count() {
        let (state, db_dir) = make_db_state("list_specs");
        let project_id = insert_test_project(&state, "myproject");

        // Insert a spec directly
        let conn = db::open_connection(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'spec1', '/tmp/spec.md', 'draft', 0, '2024-01-01', '2024-01-01')",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), project_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "12".to_string(),
            command: "project.list".to_string(),
            params: serde_json::json!({}),
        };
        let result = handle_project_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let projects = result.data["projects"].as_array().unwrap();
        assert_eq!(projects[0]["spec_count"], 1);
        cleanup_db(&db_dir);
    }

    // --- project.delete tests ---

    #[test]
    fn test_handle_project_delete_missing_name() {
        let state = make_test_state();
        let req = Request {
            id: "20".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({}),
        };
        let result = handle_project_delete(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: name"));
    }

    #[test]
    fn test_handle_project_delete_not_found() {
        let (state, db_dir) = make_db_state("del_notfound");
        let req = Request {
            id: "21".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({ "name": "nonexistent" }),
        };
        let result = handle_project_delete(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("NOT_FOUND"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_project_delete_success() {
        let (state, db_dir) = make_db_state("del_success");
        insert_test_project(&state, "to-delete");

        let req = Request {
            id: "22".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({ "name": "to-delete" }),
        };
        let result = handle_project_delete(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "to-delete");
        assert_eq!(result.data["deleted"], true);

        // Verify project is actually deleted
        let conn = db::open_connection(&state.db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, "to-delete").unwrap();
        assert!(project.is_none());
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_project_delete_with_running_agents_no_force() {
        let (state, db_dir) = make_db_state("del_running");
        let project_id = insert_test_project(&state, "running-project");

        // Create a decomposition session and work item with a running agent
        let conn = db::open_connection(&state.db_path).unwrap();
        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01', '2024-01-01')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'in_progress', 'E1', 0, '2024-01-01', '2024-01-01')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        let agent_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO agent_runs (id, work_item_id, status, started_at)
             VALUES (?1, ?2, 'running', '2024-01-01')",
            rusqlite::params![agent_id.to_string(), epic_id.to_string()],
        )
        .unwrap();
        drop(conn);

        // Without force: should error
        let req = Request {
            id: "23".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({ "name": "running-project" }),
        };
        let result = handle_project_delete(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("running agent"));

        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_project_delete_with_running_agents_force() {
        let (state, db_dir) = make_db_state("del_force");
        let project_id = insert_test_project(&state, "force-project");

        // Create a decomposition session, work item, and running agent
        let conn = db::open_connection(&state.db_path).unwrap();
        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01', '2024-01-01')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'in_progress', 'E1', 0, '2024-01-01', '2024-01-01')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        let agent_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO agent_runs (id, work_item_id, status, started_at)
             VALUES (?1, ?2, 'running', '2024-01-01')",
            rusqlite::params![agent_id.to_string(), epic_id.to_string()],
        )
        .unwrap();
        drop(conn);

        // With force: should succeed
        let req = Request {
            id: "24".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({ "name": "force-project", "force": true }),
        };
        let result = handle_project_delete(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "force-project");
        assert_eq!(result.data["deleted"], true);

        // Verify project is deleted
        let conn = db::open_connection(&state.db_path).unwrap();
        let project = db::projects::get_project_by_name(&conn, "force-project").unwrap();
        assert!(project.is_none());
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_project_delete_cascades() {
        let (state, db_dir) = make_db_state("del_cascade");
        let project_id = insert_test_project(&state, "cascade-project");

        // Add a spec to the project
        let conn = db::open_connection(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'spec1', '/tmp/spec.md', 'draft', 0, '2024-01-01', '2024-01-01')",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), project_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "25".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({ "name": "cascade-project" }),
        };
        let result = handle_project_delete(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );

        // Verify specs are also deleted (cascaded)
        let conn = db::open_connection(&state.db_path).unwrap();
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM specs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_project_list() {
        let (state, db_dir) = make_db_state("dispatch_list");
        let req = Request {
            id: "30".to_string(),
            command: "project.list".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_project_delete() {
        let (state, db_dir) = make_db_state("dispatch_del");
        let req = Request {
            id: "31".to_string(),
            command: "project.delete".to_string(),
            params: serde_json::json!({ "name": "nope" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                // Should be error (not found) but not "unknown command"
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    // --- spec.new tests ---

    #[test]
    fn test_handle_spec_new_missing_project_name() {
        let state = make_test_state();
        let req = Request {
            id: "50".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({ "spec_name": "my-spec" }),
        };
        match handle_spec_new(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: project_name"));
            }
            _ => panic!("expected Single response for error"),
        }
    }

    #[test]
    fn test_handle_spec_new_missing_spec_name() {
        let state = make_test_state();
        let req = Request {
            id: "51".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({ "project_name": "myproject" }),
        };
        match handle_spec_new(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: spec_name"));
            }
            _ => panic!("expected Single response for error"),
        }
    }

    #[test]
    fn test_handle_spec_new_project_not_found() {
        let (state, db_dir) = make_db_state("spec_new_notfound");
        let req = Request {
            id: "52".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({
                "project_name": "nonexistent",
                "spec_name": "my-spec",
            }),
        };
        match handle_spec_new(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_new_active_session_exists() {
        let (state, db_dir) = make_db_state("spec_new_active");
        let project_id = insert_test_project(&state, "active-project");

        // Create an active spec session
        let conn = db::open_connection(&state.db_path).unwrap();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'existing-spec', '/tmp/spec.md', 'draft', 1, '2024-01-01', '2024-01-01')",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), project_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "53".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({
                "project_name": "active-project",
                "spec_name": "new-spec",
            }),
        };
        match handle_spec_new(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("session is already active"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[tokio::test]
    async fn test_handle_spec_new_creates_spec_record() {
        let (state, db_dir) = make_db_state("spec_new_success");
        let project_id = insert_test_project(&state, "spec-project");

        let req = Request {
            id: "54".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({
                "project_name": "spec-project",
                "spec_name": "my-spec",
            }),
        };

        // This returns Streaming because the DB part succeeds.
        // Claude won't actually be available, but the spec record should be created.
        match handle_spec_new(req, &state) {
            HandlerResult::Streaming(_rx) => {
                // Verify spec was created in DB with session_active=true
                let conn = db::open_connection(&state.db_path).unwrap();
                let spec = db::specs::get_spec_by_name(&conn, &project_id, "my-spec")
                    .unwrap()
                    .expect("spec should exist");
                assert_eq!(spec.name, "my-spec");
                assert_eq!(spec.status, nflow_core::spec::SpecStatus::Draft);
                assert!(spec.session_active);
                assert_eq!(spec.project_id, project_id);
            }
            HandlerResult::Single(resp) => {
                panic!(
                    "expected Streaming response but got Single: {:?}",
                    resp.data
                );
            }
        }
        cleanup_db(&db_dir);
    }

    #[tokio::test]
    async fn test_handle_spec_new_with_codebase_default_true() {
        let (state, db_dir) = make_db_state("spec_new_codebase");
        insert_test_project(&state, "codebase-project");

        // No with_codebase param — should default to true
        let req = Request {
            id: "55".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({
                "project_name": "codebase-project",
                "spec_name": "code-spec",
            }),
        };
        match handle_spec_new(req, &state) {
            HandlerResult::Streaming(_rx) => {
                // Success — defaults to with_codebase=true
            }
            HandlerResult::Single(resp) => {
                panic!(
                    "expected Streaming response but got Single: {:?}",
                    resp.data
                );
            }
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_new() {
        let (state, db_dir) = make_db_state("dispatch_spec_new");
        let req = Request {
            id: "56".to_string(),
            command: "spec.new".to_string(),
            params: serde_json::json!({ "project_name": "nope" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                // Should be error (missing spec_name) but not "unknown command"
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: spec_name"));
            }
            _ => panic!("expected Single response for missing param"),
        }
        cleanup_db(&db_dir);
    }

    // --- spec.answer tests ---

    #[test]
    fn test_handle_spec_answer_missing_spec_id() {
        let state = make_test_state();
        let req = Request {
            id: "60".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({ "answer_text": "Rust" }),
        };
        match handle_spec_answer(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: spec_id"));
            }
            _ => panic!("expected Single response for error"),
        }
    }

    #[test]
    fn test_handle_spec_answer_missing_answer_text() {
        let state = make_test_state();
        let req = Request {
            id: "61".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({ "spec_id": uuid::Uuid::new_v4().to_string() }),
        };
        match handle_spec_answer(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: answer_text"));
            }
            _ => panic!("expected Single response for error"),
        }
    }

    #[test]
    fn test_handle_spec_answer_invalid_spec_id_format() {
        let state = make_test_state();
        let req = Request {
            id: "62".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({
                "spec_id": "not-a-uuid",
                "answer_text": "Rust",
            }),
        };
        match handle_spec_answer(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_PARAMS"));
            }
            _ => panic!("expected Single response for error"),
        }
    }

    #[test]
    fn test_handle_spec_answer_spec_not_found() {
        let (state, db_dir) = make_db_state("spec_answer_notfound");
        let req = Request {
            id: "63".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({
                "spec_id": uuid::Uuid::new_v4().to_string(),
                "answer_text": "Rust",
            }),
        };
        match handle_spec_answer(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_answer_no_active_session() {
        let (state, db_dir) = make_db_state("spec_answer_inactive");
        let project_id = insert_test_project(&state, "answer-project");

        // Create a spec with session_active=false
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'my-spec', '/tmp/spec.md', 'draft', 0, '2024-01-01', '2024-01-01')",
            rusqlite::params![spec_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "64".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({
                "spec_id": spec_id.to_string(),
                "answer_text": "Rust",
            }),
        };
        match handle_spec_answer(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("no active session"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_answer_no_claude_session_id() {
        let (state, db_dir) = make_db_state("spec_answer_nosession");
        let project_id = insert_test_project(&state, "nosession-project");

        // Create a spec with session_active=true but no claude_session_id
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'my-spec', '/tmp/spec.md', 'draft', 1, '2024-01-01', '2024-01-01')",
            rusqlite::params![spec_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "65".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({
                "spec_id": spec_id.to_string(),
                "answer_text": "Rust",
            }),
        };
        match handle_spec_answer(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("no claude session"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[tokio::test]
    async fn test_handle_spec_answer_with_valid_session_returns_streaming() {
        let (state, db_dir) = make_db_state("spec_answer_streaming");
        let project_id = insert_test_project(&state, "streaming-project");

        // Create a spec with session_active=true and a claude_session_id
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at)
             VALUES (?1, ?2, 'my-spec', '/tmp/spec.md', 'draft', 1, 'session-abc', '2024-01-01', '2024-01-01')",
            rusqlite::params![spec_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "66".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({
                "spec_id": spec_id.to_string(),
                "answer_text": "I want to use Rust",
            }),
        };

        // Should return Streaming because the DB part succeeds
        // (Claude won't be available, but the handler returns Streaming before spawn)
        match handle_spec_answer(req, &state) {
            HandlerResult::Streaming(_rx) => {
                // Success — handler accepted the request and started streaming
            }
            HandlerResult::Single(resp) => {
                panic!(
                    "expected Streaming response but got Single: {:?}",
                    resp.data
                );
            }
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_answer() {
        let (state, db_dir) = make_db_state("dispatch_spec_answer");
        let req = Request {
            id: "67".to_string(),
            command: "spec.answer".to_string(),
            params: serde_json::json!({ "answer_text": "Rust" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                // Should be error (missing spec_id) but not "unknown command"
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: spec_id"));
            }
            _ => panic!("expected Single response for missing param"),
        }
        cleanup_db(&db_dir);
    }

    // --- DB: get_spec_by_id test ---

    #[test]
    fn test_get_spec_by_id() {
        let (state, db_dir) = make_db_state("get_spec_by_id");
        let project_id = insert_test_project(&state, "id-project");

        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at)
             VALUES (?1, ?2, 'my-spec', '/tmp/spec.md', 'draft', 1, 'session-xyz', '2024-01-01', '2024-01-01')",
            rusqlite::params![spec_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        // Found
        let spec = db::specs::get_spec_by_id(&conn, &spec_id).unwrap().unwrap();
        assert_eq!(spec.id, spec_id);
        assert_eq!(spec.name, "my-spec");
        assert_eq!(spec.project_id, project_id);
        assert!(spec.session_active);
        assert_eq!(spec.claude_session_id.as_deref(), Some("session-xyz"));

        // Not found
        let missing = db::specs::get_spec_by_id(&conn, &uuid::Uuid::new_v4()).unwrap();
        assert!(missing.is_none());

        drop(conn);
        cleanup_db(&db_dir);
    }

    // --- Helper to insert a spec directly into the DB ---

    fn insert_test_spec(
        state: &HandlerState,
        project_id: uuid::Uuid,
        name: &str,
        status: &str,
        session_active: bool,
    ) -> uuid::Uuid {
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![
                spec_id.to_string(),
                project_id.to_string(),
                name,
                format!("/tmp/specs/{}.md", name),
                status,
                session_active,
            ],
        )
        .unwrap();
        spec_id
    }

    fn insert_test_spec_with_session(
        state: &HandlerState,
        project_id: uuid::Uuid,
        name: &str,
        status: &str,
        session_active: bool,
        claude_session_id: Option<&str>,
    ) -> uuid::Uuid {
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, claude_session_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![
                spec_id.to_string(),
                project_id.to_string(),
                name,
                format!("/tmp/specs/{}.md", name),
                status,
                session_active,
                claude_session_id,
            ],
        )
        .unwrap();
        spec_id
    }

    // --- spec.list tests ---

    #[test]
    fn test_handle_spec_list_missing_project_name() {
        let state = make_test_state();
        let req = Request {
            id: "100".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({}),
        };
        let result = handle_spec_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: project_name"));
    }

    #[test]
    fn test_handle_spec_list_project_not_found() {
        let (state, db_dir) = make_db_state("spec_list_notfound");
        let req = Request {
            id: "101".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({ "project_name": "nonexistent" }),
        };
        let result = handle_spec_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("NOT_FOUND"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_list_empty() {
        let (state, db_dir) = make_db_state("spec_list_empty");
        insert_test_project(&state, "empty-project");

        let req = Request {
            id: "102".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({ "project_name": "empty-project" }),
        };
        let result = handle_spec_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let specs = result.data["specs"].as_array().unwrap();
        assert!(specs.is_empty());
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_list_returns_specs() {
        let (state, db_dir) = make_db_state("spec_list_specs");
        let project_id = insert_test_project(&state, "list-project");
        insert_test_spec(&state, project_id, "alpha", "draft", false);
        insert_test_spec(&state, project_id, "beta", "approved", false);

        let req = Request {
            id: "103".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({ "project_name": "list-project" }),
        };
        let result = handle_spec_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let specs = result.data["specs"].as_array().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0]["name"], "alpha");
        assert_eq!(specs[0]["status"], "draft");
        assert_eq!(specs[1]["name"], "beta");
        assert_eq!(specs[1]["status"], "approved");
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_list_hides_deleted() {
        let (state, db_dir) = make_db_state("spec_list_deleted");
        let project_id = insert_test_project(&state, "delete-list-project");
        insert_test_spec(&state, project_id, "visible", "draft", false);
        insert_test_spec(&state, project_id, "hidden", "deleted", false);

        let req = Request {
            id: "104".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({ "project_name": "delete-list-project" }),
        };
        let result = handle_spec_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let specs = result.data["specs"].as_array().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0]["name"], "visible");
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_list_includes_session_active() {
        let (state, db_dir) = make_db_state("spec_list_session");
        let project_id = insert_test_project(&state, "session-list-project");
        insert_test_spec(&state, project_id, "active-spec", "draft", true);

        let req = Request {
            id: "105".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({ "project_name": "session-list-project" }),
        };
        let result = handle_spec_list(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Ok);
        let specs = result.data["specs"].as_array().unwrap();
        assert_eq!(specs[0]["session_active"], true);
        cleanup_db(&db_dir);
    }

    // --- spec.view tests ---

    #[test]
    fn test_handle_spec_view_missing_project_name() {
        let state = make_test_state();
        let req = Request {
            id: "110".to_string(),
            command: "spec.view".to_string(),
            params: serde_json::json!({ "spec_name": "my-spec" }),
        };
        let result = handle_spec_view(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: project_name"));
    }

    #[test]
    fn test_handle_spec_view_missing_spec_name() {
        let state = make_test_state();
        let req = Request {
            id: "111".to_string(),
            command: "spec.view".to_string(),
            params: serde_json::json!({ "project_name": "myproject" }),
        };
        let result = handle_spec_view(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: spec_name"));
    }

    #[test]
    fn test_handle_spec_view_spec_not_found() {
        let (state, db_dir) = make_db_state("spec_view_notfound");
        insert_test_project(&state, "view-project");

        let req = Request {
            id: "112".to_string(),
            command: "spec.view".to_string(),
            params: serde_json::json!({
                "project_name": "view-project",
                "spec_name": "nonexistent",
            }),
        };
        let result = handle_spec_view(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("NOT_FOUND"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_view_reads_file() {
        let (state, db_dir) = make_db_state("spec_view_read");
        let project_id = insert_test_project(&state, "view-read-project");

        // Create a temp spec file
        let spec_dir =
            std::env::temp_dir().join(format!("nflow_spec_view_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&spec_dir).unwrap();
        let spec_file = spec_dir.join("my-spec.md");
        std::fs::write(&spec_file, "# My Spec\n\nSome content here.").unwrap();

        // Insert spec with correct file_path
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO specs (id, project_id, name, file_path, status, session_active, created_at, updated_at)
             VALUES (?1, ?2, 'my-spec', ?3, 'draft', 0, '2024-01-01', '2024-01-01')",
            rusqlite::params![spec_id.to_string(), project_id.to_string(), spec_file.to_string_lossy().to_string()],
        ).unwrap();
        drop(conn);

        let req = Request {
            id: "113".to_string(),
            command: "spec.view".to_string(),
            params: serde_json::json!({
                "project_name": "view-read-project",
                "spec_name": "my-spec",
            }),
        };
        let result = handle_spec_view(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "my-spec");
        assert_eq!(result.data["status"], "draft");
        assert_eq!(result.data["content"], "# My Spec\n\nSome content here.");

        let _ = std::fs::remove_dir_all(&spec_dir);
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_view_file_not_found() {
        let (state, db_dir) = make_db_state("spec_view_nofile");
        let project_id = insert_test_project(&state, "nofile-project");
        insert_test_spec(&state, project_id, "no-file-spec", "draft", false);

        let req = Request {
            id: "114".to_string(),
            command: "spec.view".to_string(),
            params: serde_json::json!({
                "project_name": "nofile-project",
                "spec_name": "no-file-spec",
            }),
        };
        let result = handle_spec_view(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("failed to read spec file"));
        cleanup_db(&db_dir);
    }

    // --- spec.approve tests ---

    #[test]
    fn test_handle_spec_approve_missing_params() {
        let state = make_test_state();
        let req = Request {
            id: "120".to_string(),
            command: "spec.approve".to_string(),
            params: serde_json::json!({}),
        };
        let result = handle_spec_approve(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("missing required parameter: project_name"));
    }

    #[test]
    fn test_handle_spec_approve_draft_to_approved() {
        let (state, db_dir) = make_db_state("spec_approve_success");
        let project_id = insert_test_project(&state, "approve-project");
        insert_test_spec(&state, project_id, "my-spec", "draft", false);

        let req = Request {
            id: "121".to_string(),
            command: "spec.approve".to_string(),
            params: serde_json::json!({
                "project_name": "approve-project",
                "spec_name": "my-spec",
            }),
        };
        let result = handle_spec_approve(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "my-spec");
        assert_eq!(result.data["status"], "approved");

        // Verify DB
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "my-spec")
            .unwrap()
            .unwrap();
        assert_eq!(spec.status, SpecStatus::Approved);
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_approve_already_approved() {
        let (state, db_dir) = make_db_state("spec_approve_already");
        let project_id = insert_test_project(&state, "already-approved-project");
        insert_test_spec(&state, project_id, "approved-spec", "approved", false);

        let req = Request {
            id: "122".to_string(),
            command: "spec.approve".to_string(),
            params: serde_json::json!({
                "project_name": "already-approved-project",
                "spec_name": "approved-spec",
            }),
        };
        let result = handle_spec_approve(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_approve_stops_active_session() {
        let (state, db_dir) = make_db_state("spec_approve_session");
        let project_id = insert_test_project(&state, "approve-session-project");
        insert_test_spec_with_session(
            &state,
            project_id,
            "active-spec",
            "draft",
            true,
            Some("session-123"),
        );

        let req = Request {
            id: "123".to_string(),
            command: "spec.approve".to_string(),
            params: serde_json::json!({
                "project_name": "approve-session-project",
                "spec_name": "active-spec",
            }),
        };
        let result = handle_spec_approve(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["status"], "approved");

        // Verify session was stopped
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "active-spec")
            .unwrap()
            .unwrap();
        assert!(!spec.session_active);
        assert_eq!(spec.status, SpecStatus::Approved);
        cleanup_db(&db_dir);
    }

    // --- spec.reopen tests ---

    #[test]
    fn test_handle_spec_reopen_approved_to_draft() {
        let (state, db_dir) = make_db_state("spec_reopen_success");
        let project_id = insert_test_project(&state, "reopen-project");
        insert_test_spec(&state, project_id, "my-spec", "approved", false);

        let req = Request {
            id: "130".to_string(),
            command: "spec.reopen".to_string(),
            params: serde_json::json!({
                "project_name": "reopen-project",
                "spec_name": "my-spec",
            }),
        };
        let result = handle_spec_reopen(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "my-spec");
        assert_eq!(result.data["status"], "draft");

        // Verify DB
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "my-spec")
            .unwrap()
            .unwrap();
        assert_eq!(spec.status, SpecStatus::Draft);
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_reopen_from_draft_fails() {
        let (state, db_dir) = make_db_state("spec_reopen_draft");
        let project_id = insert_test_project(&state, "reopen-draft-project");
        insert_test_spec(&state, project_id, "draft-spec", "draft", false);

        let req = Request {
            id: "131".to_string(),
            command: "spec.reopen".to_string(),
            params: serde_json::json!({
                "project_name": "reopen-draft-project",
                "spec_name": "draft-spec",
            }),
        };
        let result = handle_spec_reopen(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_reopen_from_decomposed_fails() {
        let (state, db_dir) = make_db_state("spec_reopen_decomposed");
        let project_id = insert_test_project(&state, "reopen-decomposed-project");
        insert_test_spec(&state, project_id, "decomposed-spec", "decomposed", false);

        let req = Request {
            id: "132".to_string(),
            command: "spec.reopen".to_string(),
            params: serde_json::json!({
                "project_name": "reopen-decomposed-project",
                "spec_name": "decomposed-spec",
            }),
        };
        let result = handle_spec_reopen(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));
        cleanup_db(&db_dir);
    }

    // --- spec.delete tests ---

    #[test]
    fn test_handle_spec_delete_draft() {
        let (state, db_dir) = make_db_state("spec_delete_draft");
        let project_id = insert_test_project(&state, "delete-draft-project");
        insert_test_spec(&state, project_id, "draft-spec", "draft", false);

        let req = Request {
            id: "140".to_string(),
            command: "spec.delete".to_string(),
            params: serde_json::json!({
                "project_name": "delete-draft-project",
                "spec_name": "draft-spec",
            }),
        };
        let result = handle_spec_delete(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["name"], "draft-spec");
        assert_eq!(result.data["status"], "deleted");
        assert_eq!(result.data["deleted"], true);

        // Verify DB (record still exists with status=deleted)
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "draft-spec")
            .unwrap()
            .unwrap();
        assert_eq!(spec.status, SpecStatus::Deleted);
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_delete_approved_without_force() {
        let (state, db_dir) = make_db_state("spec_delete_approved_noforce");
        let project_id = insert_test_project(&state, "approved-noforce-project");
        insert_test_spec(&state, project_id, "approved-spec", "approved", false);

        let req = Request {
            id: "141".to_string(),
            command: "spec.delete".to_string(),
            params: serde_json::json!({
                "project_name": "approved-noforce-project",
                "spec_name": "approved-spec",
            }),
        };
        let result = handle_spec_delete(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("force=true"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_delete_approved_with_force() {
        let (state, db_dir) = make_db_state("spec_delete_approved_force");
        let project_id = insert_test_project(&state, "approved-force-project");
        insert_test_spec(&state, project_id, "approved-spec", "approved", false);

        let req = Request {
            id: "142".to_string(),
            command: "spec.delete".to_string(),
            params: serde_json::json!({
                "project_name": "approved-force-project",
                "spec_name": "approved-spec",
                "force": true,
            }),
        };
        let result = handle_spec_delete(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );
        assert_eq!(result.data["status"], "deleted");
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_delete_decomposed_fails() {
        let (state, db_dir) = make_db_state("spec_delete_decomposed");
        let project_id = insert_test_project(&state, "decomposed-del-project");
        insert_test_spec(&state, project_id, "decomposed-spec", "decomposed", false);

        let req = Request {
            id: "143".to_string(),
            command: "spec.delete".to_string(),
            params: serde_json::json!({
                "project_name": "decomposed-del-project",
                "spec_name": "decomposed-spec",
                "force": true,
            }),
        };
        let result = handle_spec_delete(req, &state);
        assert_eq!(result.status, crate::socket::ResponseStatus::Error);
        assert!(result.data["message"]
            .as_str()
            .unwrap()
            .contains("INVALID_STATE"));
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_delete_stops_active_session() {
        let (state, db_dir) = make_db_state("spec_delete_session");
        let project_id = insert_test_project(&state, "del-session-project");
        insert_test_spec_with_session(
            &state,
            project_id,
            "active-spec",
            "draft",
            true,
            Some("session-abc"),
        );

        let req = Request {
            id: "144".to_string(),
            command: "spec.delete".to_string(),
            params: serde_json::json!({
                "project_name": "del-session-project",
                "spec_name": "active-spec",
            }),
        };
        let result = handle_spec_delete(req, &state);
        assert_eq!(
            result.status,
            crate::socket::ResponseStatus::Ok,
            "Expected Ok but got error: {:?}",
            result.data
        );

        // Verify session was stopped and spec is deleted
        let conn = db::open_connection(&state.db_path).unwrap();
        let spec = db::specs::get_spec_by_name(&conn, &project_id, "active-spec")
            .unwrap()
            .unwrap();
        assert!(!spec.session_active);
        assert_eq!(spec.status, SpecStatus::Deleted);
        cleanup_db(&db_dir);
    }

    // --- spec.resume tests ---

    #[test]
    fn test_handle_spec_resume_missing_project_name() {
        let state = make_test_state();
        let req = Request {
            id: "150".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({}),
        };
        match handle_spec_resume(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter: project_name"));
            }
            _ => panic!("expected Single response for error"),
        }
    }

    #[test]
    fn test_handle_spec_resume_no_draft_spec() {
        let (state, db_dir) = make_db_state("spec_resume_nodraft");
        insert_test_project(&state, "nodraft-project");

        let req = Request {
            id: "151".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({ "project_name": "nodraft-project" }),
        };
        match handle_spec_resume(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("no draft spec"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_resume_session_already_active() {
        let (state, db_dir) = make_db_state("spec_resume_active");
        let project_id = insert_test_project(&state, "resume-active-project");
        insert_test_spec_with_session(
            &state,
            project_id,
            "active-spec",
            "draft",
            true,
            Some("session-123"),
        );

        let req = Request {
            id: "152".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({
                "project_name": "resume-active-project",
                "spec_name": "active-spec",
            }),
        };
        match handle_spec_resume(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("already has an active session"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_resume_no_previous_session() {
        let (state, db_dir) = make_db_state("spec_resume_nosession");
        let project_id = insert_test_project(&state, "nosession-resume-project");
        insert_test_spec(&state, project_id, "no-session-spec", "draft", false);

        let req = Request {
            id: "153".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({
                "project_name": "nosession-resume-project",
                "spec_name": "no-session-spec",
            }),
        };
        match handle_spec_resume(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("no previous session"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_handle_spec_resume_non_draft_fails() {
        let (state, db_dir) = make_db_state("spec_resume_approved");
        let project_id = insert_test_project(&state, "approved-resume-project");
        insert_test_spec_with_session(
            &state,
            project_id,
            "approved-spec",
            "approved",
            false,
            Some("session-123"),
        );

        let req = Request {
            id: "154".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({
                "project_name": "approved-resume-project",
                "spec_name": "approved-spec",
            }),
        };
        match handle_spec_resume(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("must be draft"));
            }
            _ => panic!("expected Single response for error"),
        }
        cleanup_db(&db_dir);
    }

    #[tokio::test]
    async fn test_handle_spec_resume_returns_streaming() {
        let (state, db_dir) = make_db_state("spec_resume_streaming");
        let project_id = insert_test_project(&state, "resume-streaming-project");
        insert_test_spec_with_session(
            &state,
            project_id,
            "resume-spec",
            "draft",
            false,
            Some("session-abc"),
        );

        let req = Request {
            id: "155".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({
                "project_name": "resume-streaming-project",
                "spec_name": "resume-spec",
            }),
        };

        match handle_spec_resume(req, &state) {
            HandlerResult::Streaming(_rx) => {
                // Success — handler accepted and started streaming
                // Verify session was marked active in DB
                let conn = db::open_connection(&state.db_path).unwrap();
                let spec = db::specs::get_spec_by_name(&conn, &project_id, "resume-spec")
                    .unwrap()
                    .unwrap();
                assert!(spec.session_active);
            }
            HandlerResult::Single(resp) => {
                panic!(
                    "expected Streaming response but got Single: {:?}",
                    resp.data
                );
            }
        }
        cleanup_db(&db_dir);
    }

    // --- dispatch tests for new commands ---

    #[test]
    fn test_dispatch_spec_list() {
        let (state, db_dir) = make_db_state("dispatch_spec_list");
        insert_test_project(&state, "dispatch-list-project");
        let req = Request {
            id: "160".to_string(),
            command: "spec.list".to_string(),
            params: serde_json::json!({ "project_name": "dispatch-list-project" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_approve() {
        let (state, db_dir) = make_db_state("dispatch_spec_approve");
        let req = Request {
            id: "161".to_string(),
            command: "spec.approve".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_reopen() {
        let (state, db_dir) = make_db_state("dispatch_spec_reopen");
        let req = Request {
            id: "162".to_string(),
            command: "spec.reopen".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_delete() {
        let (state, db_dir) = make_db_state("dispatch_spec_delete");
        let req = Request {
            id: "163".to_string(),
            command: "spec.delete".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_view() {
        let (state, db_dir) = make_db_state("dispatch_spec_view");
        let req = Request {
            id: "164".to_string(),
            command: "spec.view".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_spec_resume() {
        let (state, db_dir) = make_db_state("dispatch_spec_resume");
        let req = Request {
            id: "165".to_string(),
            command: "spec.resume".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response for missing param"),
        }
        cleanup_db(&db_dir);
    }

    // --- plan.show tests ---

    #[test]
    fn test_plan_show_missing_project_name() {
        let (state, db_dir) = make_db_state("plan_show_missing_param");
        let req = Request {
            id: "200".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_plan_show_project_not_found() {
        let (state, db_dir) = make_db_state("plan_show_not_found");
        let req = Request {
            id: "201".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({ "project_name": "nonexistent" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_plan_show_no_waves() {
        let (state, db_dir) = make_db_state("plan_show_no_waves");
        insert_test_project(&state, "plan-show-nowaves");
        let req = Request {
            id: "202".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({ "project_name": "plan-show-nowaves" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("no decomposition waves"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_plan_show_tree_format() {
        use nflow_core::decomposition::{DecompositionSession, DecompositionStatus};
        use nflow_core::work_item::{Dependency, TaskKind, WorkItem};

        let (state, db_dir) = make_db_state("plan_show_tree");
        let project_id = insert_test_project(&state, "plan-show-tree");
        let conn = db::open_connection(&state.db_path).unwrap();

        // Create a decomposition session (wave 1)
        let session = DecompositionSession {
            id: uuid::Uuid::new_v4(),
            project_id,
            wave_number: 1,
            status: DecompositionStatus::Approved,
            claude_session_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        // Create an epic
        let epic = WorkItem::new_epic(session.id, "Epic 1".into(), "desc".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        // Create two stories under the epic
        let story1 = WorkItem::new_story(
            epic.id,
            session.id,
            "Story 1".into(),
            "desc".into(),
            "ac".into(),
            "S1".into(),
            0,
        );
        db::work_items::insert_work_item(&conn, &story1).unwrap();

        let story2 = WorkItem::new_story(
            epic.id,
            session.id,
            "Story 2".into(),
            "desc".into(),
            "ac".into(),
            "S2".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story2).unwrap();

        // Story 2 depends on Story 1
        let dep = Dependency {
            blocker_id: story1.id,
            blocked_id: story2.id,
        };
        db::work_items::insert_dependency(&conn, &dep).unwrap();

        // Create tasks under story 1
        let task1 = WorkItem::new_task(
            story1.id,
            session.id,
            "Task 1".into(),
            "desc".into(),
            "ac".into(),
            "T1".into(),
            0,
        );
        db::work_items::insert_work_item(&conn, &task1).unwrap();

        let mut task1v = WorkItem::new_task(
            story1.id,
            session.id,
            "Verify Task 1".into(),
            "desc".into(),
            "ac".into(),
            "T1v".into(),
            1,
        );
        task1v.kind = Some(TaskKind::Verify);
        db::work_items::insert_work_item(&conn, &task1v).unwrap();

        drop(conn);

        let req = Request {
            id: "203".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({ "project_name": "plan-show-tree" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                let data = &resp.data;
                assert_eq!(data["wave_number"], 1);
                assert_eq!(data["status"], "approved");

                let epics = data["epics"].as_array().unwrap();
                assert_eq!(epics.len(), 1);
                assert_eq!(epics[0]["short_id"], "W1-E1");
                assert_eq!(epics[0]["title"], "Epic 1");

                let stories = epics[0]["stories"].as_array().unwrap();
                assert_eq!(stories.len(), 2);
                assert_eq!(stories[0]["short_id"], "W1-S1");
                assert_eq!(stories[0]["progress"], "0/2");

                // Story 2 depends on Story 1
                let depends = stories[1]["depends_on"].as_array().unwrap();
                assert_eq!(depends.len(), 1);
                assert_eq!(depends[0], "W1-S1");

                // Tasks under story 1
                let tasks = stories[0]["tasks"].as_array().unwrap();
                assert_eq!(tasks.len(), 2);
                assert_eq!(tasks[0]["kind"], "impl");
                assert_eq!(tasks[1]["kind"], "verify");
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_plan_show_dag_format() {
        use nflow_core::decomposition::{DecompositionSession, DecompositionStatus};
        use nflow_core::work_item::{Dependency, WorkItem};

        let (state, db_dir) = make_db_state("plan_show_dag");
        let project_id = insert_test_project(&state, "plan-show-dag");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session = DecompositionSession {
            id: uuid::Uuid::new_v4(),
            project_id,
            wave_number: 1,
            status: DecompositionStatus::Approved,
            claude_session_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::decomposition_sessions::insert_decomposition_session(&conn, &session).unwrap();

        let epic = WorkItem::new_epic(session.id, "Epic 1".into(), "desc".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic).unwrap();

        let story1 = WorkItem::new_story(
            epic.id,
            session.id,
            "Story A".into(),
            "desc".into(),
            "ac".into(),
            "S1".into(),
            0,
        );
        db::work_items::insert_work_item(&conn, &story1).unwrap();

        let story2 = WorkItem::new_story(
            epic.id,
            session.id,
            "Story B".into(),
            "desc".into(),
            "ac".into(),
            "S2".into(),
            1,
        );
        db::work_items::insert_work_item(&conn, &story2).unwrap();

        let dep = Dependency {
            blocker_id: story1.id,
            blocked_id: story2.id,
        };
        db::work_items::insert_dependency(&conn, &dep).unwrap();

        drop(conn);

        let req = Request {
            id: "204".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({
                "project_name": "plan-show-dag",
                "dag": true,
            }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                let data = &resp.data;
                assert_eq!(data["wave_number"], 1);

                let adjacency = data["adjacency"].as_object().unwrap();
                // S1 blocks S2
                let s1_blocks = adjacency["W1-S1"].as_array().unwrap();
                assert_eq!(s1_blocks.len(), 1);
                assert_eq!(s1_blocks[0], "W1-S2");
                // S2 blocks nobody
                let s2_blocks = adjacency["W1-S2"].as_array().unwrap();
                assert!(s2_blocks.is_empty());
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_plan_show_specific_wave() {
        use nflow_core::decomposition::{DecompositionSession, DecompositionStatus};
        use nflow_core::work_item::WorkItem;

        let (state, db_dir) = make_db_state("plan_show_wave");
        let project_id = insert_test_project(&state, "plan-show-wave");
        let conn = db::open_connection(&state.db_path).unwrap();

        // Create two waves
        let session1 = DecompositionSession {
            id: uuid::Uuid::new_v4(),
            project_id,
            wave_number: 1,
            status: DecompositionStatus::Approved,
            claude_session_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::decomposition_sessions::insert_decomposition_session(&conn, &session1).unwrap();

        let session2 = DecompositionSession {
            id: uuid::Uuid::new_v4(),
            project_id,
            wave_number: 2,
            status: DecompositionStatus::InProgress,
            claude_session_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::decomposition_sessions::insert_decomposition_session(&conn, &session2).unwrap();

        // Add an epic to wave 1
        let epic1 =
            WorkItem::new_epic(session1.id, "W1 Epic".into(), "desc".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic1).unwrap();

        // Add an epic to wave 2
        let epic2 =
            WorkItem::new_epic(session2.id, "W2 Epic".into(), "desc".into(), "E1".into(), 0);
        db::work_items::insert_work_item(&conn, &epic2).unwrap();

        drop(conn);

        // Default shows latest wave (wave 2)
        let req = Request {
            id: "205".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({ "project_name": "plan-show-wave" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["wave_number"], 2);
                let epics = resp.data["epics"].as_array().unwrap();
                assert_eq!(epics[0]["title"], "W2 Epic");
            }
            _ => panic!("expected Single response"),
        }

        // Specific wave=1
        let req = Request {
            id: "206".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({ "project_name": "plan-show-wave", "wave": 1 }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["wave_number"], 1);
                let epics = resp.data["epics"].as_array().unwrap();
                assert_eq!(epics[0]["title"], "W1 Epic");
            }
            _ => panic!("expected Single response"),
        }

        // Nonexistent wave=99
        let req = Request {
            id: "207".to_string(),
            command: "plan.show".to_string(),
            params: serde_json::json!({ "project_name": "plan-show-wave", "wave": 99 }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }

        cleanup_db(&db_dir);
    }

    // --- exec.run tests ---

    #[test]
    fn test_exec_run_missing_project_name() {
        let (state, db_dir) = make_db_state("exec_run_missing");
        let req = Request {
            id: "300".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_run_project_not_found() {
        let (state, db_dir) = make_db_state("exec_run_notfound");
        let req = Request {
            id: "301".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({ "project_name": "nonexistent" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_run_enables_execution() {
        let (state, db_dir) = make_db_state("exec_run_enable");
        // Insert project with execution_enabled = false (via direct DB insert)
        let conn = db::open_connection(&state.db_path).unwrap();
        let project_id = uuid::Uuid::new_v4();
        let project = nflow_core::project::Project {
            id: project_id,
            name: "exec-test".to_string(),
            path: "/tmp/fakepath".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::projects::insert_project(&conn, &project).unwrap();
        drop(conn);

        let req = Request {
            id: "302".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({ "project_name": "exec-test" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], true);
                assert_eq!(resp.data["running_count"], 0);
                assert_eq!(resp.data["pending_count"], 0);
            }
            _ => panic!("expected Single response"),
        }

        // Verify in DB
        let conn = db::open_connection(&state.db_path).unwrap();
        let updated = db::projects::get_project_by_name(&conn, "exec-test")
            .unwrap()
            .unwrap();
        assert!(updated.execution_enabled);
        drop(conn);

        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_run_already_enabled() {
        let (state, db_dir) = make_db_state("exec_run_already");
        let _pid = insert_test_project(&state, "exec-already");

        let req = Request {
            id: "303".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({ "project_name": "exec-already" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], true);
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_run_with_pending_stories() {
        let (state, db_dir) = make_db_state("exec_run_pending");
        let project_id = insert_test_project(&state, "exec-pending");

        // Insert a session and pending story
        let conn = db::open_connection(&state.db_path).unwrap();
        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'pending', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story 1', 'desc', 'pending', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "304".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({ "project_name": "exec-pending" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], true);
                assert_eq!(resp.data["running_count"], 0);
                assert_eq!(resp.data["pending_count"], 1);
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    // --- exec.pause tests ---

    #[test]
    fn test_exec_pause_missing_project_name() {
        let (state, db_dir) = make_db_state("exec_pause_missing");
        let req = Request {
            id: "310".to_string(),
            command: "exec.pause".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_pause_project_not_found() {
        let (state, db_dir) = make_db_state("exec_pause_notfound");
        let req = Request {
            id: "311".to_string(),
            command: "exec.pause".to_string(),
            params: serde_json::json!({ "project_name": "nonexistent" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_pause_disables_execution() {
        let (state, db_dir) = make_db_state("exec_pause_disable");
        let _pid = insert_test_project(&state, "exec-pause-test");

        let req = Request {
            id: "312".to_string(),
            command: "exec.pause".to_string(),
            params: serde_json::json!({ "project_name": "exec-pause-test" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], false);
                assert_eq!(resp.data["running_count"], 0);
                assert_eq!(resp.data["pending_count"], 0);
            }
            _ => panic!("expected Single response"),
        }

        // Verify in DB
        let conn = db::open_connection(&state.db_path).unwrap();
        let updated = db::projects::get_project_by_name(&conn, "exec-pause-test")
            .unwrap()
            .unwrap();
        assert!(!updated.execution_enabled);
        drop(conn);

        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_pause_already_paused() {
        let (state, db_dir) = make_db_state("exec_pause_already");
        let conn = db::open_connection(&state.db_path).unwrap();
        let project_id = uuid::Uuid::new_v4();
        let project = nflow_core::project::Project {
            id: project_id,
            name: "exec-paused".to_string(),
            path: "/tmp/fakepath".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::projects::insert_project(&conn, &project).unwrap();
        drop(conn);

        let req = Request {
            id: "313".to_string(),
            command: "exec.pause".to_string(),
            params: serde_json::json!({ "project_name": "exec-paused" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], false);
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_run_then_pause_round_trip() {
        let (state, db_dir) = make_db_state("exec_roundtrip");
        let conn = db::open_connection(&state.db_path).unwrap();
        let project_id = uuid::Uuid::new_v4();
        let project = nflow_core::project::Project {
            id: project_id,
            name: "exec-roundtrip".to_string(),
            path: "/tmp/fakepath".to_string(),
            base_branch: "main".to_string(),
            git_provider: GitProvider::Github,
            execution_enabled: false,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        db::projects::insert_project(&conn, &project).unwrap();
        drop(conn);

        // exec.run
        let req = Request {
            id: "314".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({ "project_name": "exec-roundtrip" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], true);
            }
            _ => panic!("expected Single response"),
        }

        // exec.pause
        let req = Request {
            id: "315".to_string(),
            command: "exec.pause".to_string(),
            params: serde_json::json!({ "project_name": "exec-roundtrip" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["execution_enabled"], false);
            }
            _ => panic!("expected Single response"),
        }

        // Verify final state in DB
        let conn = db::open_connection(&state.db_path).unwrap();
        let updated = db::projects::get_project_by_name(&conn, "exec-roundtrip")
            .unwrap()
            .unwrap();
        assert!(!updated.execution_enabled);
        drop(conn);

        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_exec_run_route() {
        let (state, db_dir) = make_db_state("dispatch_exec_run");
        let req = Request {
            id: "320".to_string(),
            command: "exec.run".to_string(),
            params: serde_json::json!({}),
        };
        // Should route to exec.run (get error for missing param, not unknown command)
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_exec_pause_route() {
        let (state, db_dir) = make_db_state("dispatch_exec_pause");
        let req = Request {
            id: "321".to_string(),
            command: "exec.pause".to_string(),
            params: serde_json::json!({}),
        };
        // Should route to exec.pause (get error for missing param, not unknown command)
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    // --- exec.status tests ---

    #[test]
    fn test_exec_status_missing_project_name() {
        let (state, db_dir) = make_db_state("exec_status_missing");
        let req = Request {
            id: "400".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_status_project_not_found() {
        let (state, db_dir) = make_db_state("exec_status_notfound");
        let req = Request {
            id: "401".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "nonexistent" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_status_empty_project() {
        let (state, db_dir) = make_db_state("exec_status_empty");
        let _pid = insert_test_project(&state, "exec-status-empty");

        let req = Request {
            id: "402".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-empty" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["project_name"], "exec-status-empty");
                assert_eq!(resp.data["execution_enabled"], true);
                assert_eq!(resp.data["running_count"], 0);
                let waves = resp.data["waves"].as_array().unwrap();
                assert!(waves.is_empty());
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_status_hierarchical_structure() {
        let (state, db_dir) = make_db_state("exec_status_hierarchy");
        let project_id = insert_test_project(&state, "exec-status-hier");
        let conn = db::open_connection(&state.db_path).unwrap();

        // Insert session
        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        // Insert epic
        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Test Epic', 'epic desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Insert story
        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, branch_name, worktree_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Test Story', 'story desc', 'in_progress', 'S1', 0, 'feat/story-1', '/tmp/worktree/s1', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Insert impl task
        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Implement feature', 'task desc', 'done', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Insert verify task
        let verify_task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'verify', 'Verify feature', 'verify desc', 'in_progress', 'T1v', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![verify_task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Insert agent run for impl task
        let run_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO agent_runs (id, work_item_id, status, exit_code, started_at, finished_at)
             VALUES (?1, ?2, 'succeeded', 0, '2024-01-01T00:01:00Z', '2024-01-01T00:05:00Z')",
            rusqlite::params![run_id.to_string(), task_id.to_string()],
        )
        .unwrap();

        drop(conn);

        let req = Request {
            id: "403".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-hier" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["project_name"], "exec-status-hier");

                let waves = resp.data["waves"].as_array().unwrap();
                assert_eq!(waves.len(), 1);

                let wave = &waves[0];
                assert_eq!(wave["wave_number"], 1);
                assert_eq!(wave["status"], "approved");

                // Check summary counts
                let summary = &wave["summary"];
                assert_eq!(summary["total"], 1); // 1 story
                assert_eq!(summary["running"], 1); // in_progress
                assert_eq!(summary["pending"], 0);

                // Check hierarchy
                let epics = wave["epics"].as_array().unwrap();
                assert_eq!(epics.len(), 1);
                assert_eq!(epics[0]["short_id"], "W1-E1");
                assert_eq!(epics[0]["title"], "Test Epic");
                assert_eq!(epics[0]["status"], "in_progress");

                let stories = epics[0]["stories"].as_array().unwrap();
                assert_eq!(stories.len(), 1);
                assert_eq!(stories[0]["short_id"], "W1-S1");
                assert_eq!(stories[0]["title"], "Test Story");
                assert_eq!(stories[0]["branch_name"], "feat/story-1");
                assert_eq!(stories[0]["worktree_path"], "/tmp/worktree/s1");

                let tasks = stories[0]["tasks"].as_array().unwrap();
                assert_eq!(tasks.len(), 2);
                assert_eq!(tasks[0]["short_id"], "W1-T1");
                assert_eq!(tasks[0]["kind"], "impl");
                assert_eq!(tasks[0]["status"], "done");
                assert_eq!(tasks[0]["exit_code"], 0);
                assert_eq!(tasks[0]["retry_count"], 1); // 1 agent run

                assert_eq!(tasks[1]["short_id"], "W1-T1v");
                assert_eq!(tasks[1]["kind"], "verify");
                assert_eq!(tasks[1]["status"], "in_progress");
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_status_wave_filter() {
        let (state, db_dir) = make_db_state("exec_status_wave");
        let project_id = insert_test_project(&state, "exec-status-wave");
        let conn = db::open_connection(&state.db_path).unwrap();

        // Insert two sessions (wave 1 and wave 2)
        let session1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session1_id.to_string(), project_id.to_string()],
        )
        .unwrap();
        let session2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 2, 'in_progress', '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![session2_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        // Insert epic + story in wave 1
        let epic1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Wave 1 Epic', 'desc', 'done', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic1_id.to_string(), session1_id.to_string()],
        )
        .unwrap();
        let story1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Wave 1 Story', 'desc', 'done', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![story1_id.to_string(), epic1_id.to_string(), session1_id.to_string()],
        )
        .unwrap();

        // Insert epic + story in wave 2
        let epic2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Wave 2 Epic', 'desc', 'pending', 'E1', 0, '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![epic2_id.to_string(), session2_id.to_string()],
        )
        .unwrap();
        let story2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Wave 2 Story', 'desc', 'pending', 'S1', 0, '2024-01-02T00:00:00Z', '2024-01-02T00:00:00Z')",
            rusqlite::params![story2_id.to_string(), epic2_id.to_string(), session2_id.to_string()],
        )
        .unwrap();

        drop(conn);

        // Without wave filter: should return both waves
        let req = Request {
            id: "404".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-wave" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                let waves = resp.data["waves"].as_array().unwrap();
                assert_eq!(waves.len(), 2);
            }
            _ => panic!("expected Single response"),
        }

        // With wave filter = 1: should return only wave 1
        let req = Request {
            id: "405".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-wave", "wave": 1 }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                let waves = resp.data["waves"].as_array().unwrap();
                assert_eq!(waves.len(), 1);
                assert_eq!(waves[0]["wave_number"], 1);

                let summary = &waves[0]["summary"];
                assert_eq!(summary["total"], 1);
                assert_eq!(summary["done"], 1);
            }
            _ => panic!("expected Single response"),
        }

        // With non-existent wave: should return error
        let req = Request {
            id: "406".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-wave", "wave": 99 }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
                assert!(resp.data["message"].as_str().unwrap().contains("W99"));
            }
            _ => panic!("expected Single response"),
        }

        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_status_summary_counts() {
        let (state, db_dir) = make_db_state("exec_status_counts");
        let project_id = insert_test_project(&state, "exec-status-counts");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Insert stories with different statuses
        let statuses = [
            "pending",
            "ready",
            "in_progress",
            "done",
            "failed",
            "cancelled",
        ];
        for (i, status) in statuses.iter().enumerate() {
            let sid = uuid::Uuid::new_v4();
            conn.execute(
                "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'story', ?4, 'desc', ?5, ?6, ?7, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
                rusqlite::params![
                    sid.to_string(),
                    epic_id.to_string(),
                    session_id.to_string(),
                    format!("Story {}", i + 1),
                    status,
                    format!("S{}", i + 1),
                    i as i32,
                ],
            )
            .unwrap();
        }
        drop(conn);

        let req = Request {
            id: "407".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-counts" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                let wave = &resp.data["waves"].as_array().unwrap()[0];
                let summary = &wave["summary"];
                assert_eq!(summary["total"], 6);
                assert_eq!(summary["pending"], 1);
                assert_eq!(summary["ready"], 1);
                assert_eq!(summary["running"], 1);
                assert_eq!(summary["done"], 1);
                assert_eq!(summary["failed"], 1);
                assert_eq!(summary["cancelled"], 1);
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_status_task_agent_details() {
        let (state, db_dir) = make_db_state("exec_status_agent");
        let project_id = insert_test_project(&state, "exec-status-agent");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story', 'desc', 'in_progress', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Task with multiple retries
        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Impl Task', 'desc', 'done', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // First attempt (failed)
        let run1_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO agent_runs (id, work_item_id, status, exit_code, started_at, finished_at)
             VALUES (?1, ?2, 'failed', 1, '2024-01-01T00:01:00Z', '2024-01-01T00:03:00Z')",
            rusqlite::params![run1_id.to_string(), task_id.to_string()],
        )
        .unwrap();

        // Second attempt (succeeded)
        let run2_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO agent_runs (id, work_item_id, status, exit_code, started_at, finished_at)
             VALUES (?1, ?2, 'succeeded', 0, '2024-01-01T00:05:00Z', '2024-01-01T00:10:00Z')",
            rusqlite::params![run2_id.to_string(), task_id.to_string()],
        )
        .unwrap();

        drop(conn);

        let req = Request {
            id: "408".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({ "project_name": "exec-status-agent" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                let wave = &resp.data["waves"].as_array().unwrap()[0];
                let task = &wave["epics"][0]["stories"][0]["tasks"][0];
                assert_eq!(task["retry_count"], 2); // 2 agent runs total
                assert_eq!(task["exit_code"], 0); // latest run exit code
                                                  // latest run started_at/finished_at should be present
                assert!(task["started_at"].as_str().is_some());
                assert!(task["finished_at"].as_str().is_some());
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_dispatch_exec_status_route() {
        let (state, db_dir) = make_db_state("dispatch_exec_status");
        let req = Request {
            id: "409".to_string(),
            command: "exec.status".to_string(),
            params: serde_json::json!({}),
        };
        // Should route to exec.status (get error for missing param, not unknown command)
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    // --- exec.retry tests ---

    #[test]
    fn test_exec_retry_missing_task_id() {
        let (state, db_dir) = make_db_state("exec_retry_missing");
        let req = Request {
            id: "400".to_string(),
            command: "exec.retry".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_retry_task_not_found() {
        let (state, db_dir) = make_db_state("exec_retry_notfound");
        let req = Request {
            id: "401".to_string(),
            command: "exec.retry".to_string(),
            params: serde_json::json!({ "task_id": "W1-T99" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_retry_invalid_format() {
        let (state, db_dir) = make_db_state("exec_retry_invalid");
        let req = Request {
            id: "402".to_string(),
            command: "exec.retry".to_string(),
            params: serde_json::json!({ "task_id": "invalid" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_retry_task_not_failed() {
        let (state, db_dir) = make_db_state("exec_retry_notfailed");
        let project_id = insert_test_project(&state, "retry-proj");
        let conn = db::open_connection(&state.db_path).unwrap();

        // Create session (wave 1), epic, story, task
        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story', 'desc', 'in_progress', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task 1', 'desc', 'pending', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "403".to_string(),
            command: "exec.retry".to_string(),
            params: serde_json::json!({ "task_id": "W1-T1" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_retry_not_a_task() {
        let (state, db_dir) = make_db_state("exec_retry_notatask");
        let project_id = insert_test_project(&state, "retry-proj2");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'failed', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        // Trying to retry an epic should fail
        let req = Request {
            id: "404".to_string(),
            command: "exec.retry".to_string(),
            params: serde_json::json!({ "task_id": "W1-E1" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                // Should fail either on INVALID_STATE (not failed) or INVALID_PARAMS (not a task)
                let msg = resp.data["message"].as_str().unwrap();
                assert!(msg.contains("INVALID") || msg.contains("not a task"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    // --- exec.skip tests ---

    #[test]
    fn test_exec_skip_missing_task_id() {
        let (state, db_dir) = make_db_state("exec_skip_missing");
        let req = Request {
            id: "500".to_string(),
            command: "exec.skip".to_string(),
            params: serde_json::json!({}),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_skip_task_not_found() {
        let (state, db_dir) = make_db_state("exec_skip_notfound");
        let req = Request {
            id: "501".to_string(),
            command: "exec.skip".to_string(),
            params: serde_json::json!({ "task_id": "W1-T99" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"].as_str().unwrap().contains("NOT_FOUND"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_skip_task_not_failed() {
        let (state, db_dir) = make_db_state("exec_skip_notfailed");
        let project_id = insert_test_project(&state, "skip-proj");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story', 'desc', 'in_progress', 'S1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task 1', 'desc', 'pending', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "502".to_string(),
            command: "exec.skip".to_string(),
            params: serde_json::json!({ "task_id": "W1-T1" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("INVALID_STATE"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_skip_not_a_task() {
        let (state, db_dir) = make_db_state("exec_skip_notatask");
        let project_id = insert_test_project(&state, "skip-proj2");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'failed', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "503".to_string(),
            command: "exec.skip".to_string(),
            params: serde_json::json!({ "task_id": "W1-E1" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                let msg = resp.data["message"].as_str().unwrap();
                assert!(msg.contains("INVALID") || msg.contains("not a task"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_skip_marks_task_done_and_cancels_verify() {
        let (state, db_dir) = make_db_state("exec_skip_withverify");
        let project_id = insert_test_project(&state, "skip-proj3");
        let conn = db::open_connection(&state.db_path).unwrap();

        let session_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO decomposition_sessions (id, project_id, wave_number, status, created_at, updated_at)
             VALUES (?1, ?2, 1, 'approved', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![session_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        let epic_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, 'epic', 'Epic', 'desc', 'in_progress', 'E1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        let story_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, title, description, status, short_id, sort_order, worktree_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'story', 'Story', 'desc', 'failed', 'S1', 0, '/tmp/fake-wt', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![story_id.to_string(), epic_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Failed impl task at sort_order 0
        let task_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'impl', 'Task 1', 'desc', 'failed', 'T1', 0, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![task_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();

        // Pending verify task at sort_order 1
        let verify_id = uuid::Uuid::new_v4();
        conn.execute(
            "INSERT INTO work_items (id, parent_id, decomposition_session_id, item_type, kind, title, description, status, short_id, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'task', 'verify', 'Verify T1', 'desc', 'pending', 'T1v', 1, '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            rusqlite::params![verify_id.to_string(), story_id.to_string(), session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let req = Request {
            id: "504".to_string(),
            command: "exec.skip".to_string(),
            params: serde_json::json!({ "task_id": "W1-T1" }),
        };
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Ok);
                assert_eq!(resp.data["task_id"].as_str().unwrap(), "W1-T1");
                // Verify task should be reported as skipped
                assert_eq!(resp.data["skipped_verify"].as_str().unwrap(), "T1v");
                // Story should be completed (all tasks done/cancelled)
                assert_eq!(resp.data["story_completed"].as_bool().unwrap(), true);
            }
            _ => panic!("expected Single response"),
        }

        // Verify DB state
        let conn = db::open_connection(&state.db_path).unwrap();
        let impl_status: String = conn
            .query_row(
                "SELECT status FROM work_items WHERE id = ?1",
                rusqlite::params![task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(impl_status, "done");

        let verify_status: String = conn
            .query_row(
                "SELECT status FROM work_items WHERE id = ?1",
                rusqlite::params![verify_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(verify_status, "cancelled");

        let story_status: String = conn
            .query_row(
                "SELECT status FROM work_items WHERE id = ?1",
                rusqlite::params![story_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(story_status, "done");

        cleanup_db(&db_dir);
    }

    #[test]
    fn test_exec_skip_dispatch_route() {
        let (state, db_dir) = make_db_state("dispatch_exec_skip");
        let req = Request {
            id: "510".to_string(),
            command: "exec.skip".to_string(),
            params: serde_json::json!({}),
        };
        // Should route to exec.skip (get error for missing param, not unknown command)
        match dispatch(req, &state) {
            HandlerResult::Single(resp) => {
                assert_eq!(resp.status, crate::socket::ResponseStatus::Error);
                assert!(resp.data["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing required parameter"));
            }
            _ => panic!("expected Single response"),
        }
        cleanup_db(&db_dir);
    }
}
