use std::path::{Path, PathBuf};
use std::sync::Arc;

use nflow_core::project::{CreateProjectParams, GitProvider, ProjectService};

use crate::db;
use crate::socket::{HandlerResult, Request, Response};

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
}
