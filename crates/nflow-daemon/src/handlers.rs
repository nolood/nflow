use std::path::{Path, PathBuf};
use std::sync::Arc;

use nflow_core::project::{CreateProjectParams, GitProvider, ProjectService};

use crate::db;
use crate::socket::{HandlerResult, Request, Response};

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
    let git_provider_str = match project.git_provider {
        GitProvider::Github => "github",
        GitProvider::Gitlab => "gitlab",
    };

    Response::ok(
        id,
        serde_json::json!({
            "project_id": project.id.to_string(),
            "name": project.name,
            "path": project.path,
            "base_branch": project.base_branch,
            "git_provider": git_provider_str,
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
}
