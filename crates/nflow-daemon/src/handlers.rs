use std::path::{Path, PathBuf};
use std::sync::Arc;

use nflow_core::decomposition::{DecompositionSession, DecompositionSpec};
use nflow_core::project::{CreateProjectParams, GitProvider, ProjectService};
use nflow_core::spec::{Spec, SpecStatus};
use nflow_core::work_item::{auto_generate_verify_tasks, Dependency, WorkItem};

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
        let mut session_update =
            match db::decomposition_sessions::get_decomposition_session(&conn, &session_id) {
                Ok(Some(s)) => s,
                _ => {
                    let _ = tx
                        .send(StreamingResponseLine::error(
                            req_id,
                            "failed to retrieve session for update",
                        ))
                        .await;
                    return;
                }
            };
        session_update.set_claude_session_id(csid.clone());
        let _ = db::decomposition_sessions::insert_decomposition_session(&conn, &session_update);
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
}
