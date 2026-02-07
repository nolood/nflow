use std::path::{Path, PathBuf};
use std::sync::Arc;

use nflow_core::project::{CreateProjectParams, GitProvider, ProjectService};
use nflow_core::spec::Spec;

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
}
