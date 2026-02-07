use std::path::Path;
use std::process::Stdio;

use crate::error::{GitError, Result};

/// Validates that the given path is a git repository via `git rev-parse`.
async fn validate_git_repo(repo_path: &Path) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if output.status.success() {
        Ok(())
    } else {
        Err(GitError::NotAGitRepository(repo_path.display().to_string()))
    }
}

/// Runs a git command in the given directory, returning stdout on success
/// or a GitError::CommandFailed on failure.
async fn run_git_command(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(GitError::CommandFailed(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )))
    }
}

/// Creates a git worktree at `worktree_path` based on `base_branch`.
///
/// Runs `git fetch origin {base_branch}` then
/// `git worktree add {worktree_path} origin/{base_branch}`.
///
/// Returns an error if:
/// - `worktree_path` already exists
/// - `repo_path` is not a git repository
/// - Either git command fails
pub async fn create_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    base_branch: &str,
) -> Result<()> {
    // Validate repo_path is a git repository
    validate_git_repo(repo_path).await?;

    // Validate worktree_path doesn't already exist
    if worktree_path.exists() {
        return Err(GitError::PathAlreadyExists(
            worktree_path.display().to_string(),
        ));
    }

    // Fetch the base branch from origin
    run_git_command(repo_path, &["fetch", "origin", base_branch]).await?;

    // Create the worktree
    let worktree_str = worktree_path
        .to_str()
        .ok_or_else(|| GitError::CommandFailed("Invalid worktree path encoding".to_string()))?;

    let remote_ref = format!("origin/{base_branch}");
    run_git_command(repo_path, &["worktree", "add", worktree_str, &remote_ref]).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Helper: create a bare git repo to act as "origin" and a clone to act as repo_path.
    async fn setup_repo_with_remote() -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().unwrap();
        let bare_path = dir.path().join("origin.git");
        let clone_path = dir.path().join("repo");

        // Create bare repo with "main" as default branch
        std::fs::create_dir_all(&bare_path).unwrap();
        tokio::process::Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .current_dir(&bare_path)
            .output()
            .await
            .unwrap();

        // Clone it
        tokio::process::Command::new("git")
            .args([
                "clone",
                bare_path.to_str().unwrap(),
                clone_path.to_str().unwrap(),
            ])
            .output()
            .await
            .unwrap();

        // Configure user in clone
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();

        // Create initial commit on main and push
        let readme = clone_path.join("README.md");
        std::fs::write(&readme, "# test\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(&clone_path)
            .output()
            .await
            .unwrap();

        (dir, bare_path, clone_path)
    }

    #[tokio::test]
    async fn test_create_worktree_success() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/my-feature");

        let result = create_worktree(&repo, &wt_path, "main").await;
        assert!(result.is_ok(), "create_worktree failed: {:?}", result);
        assert!(wt_path.exists());
        // Verify it's a valid git worktree (has .git file)
        assert!(wt_path.join(".git").exists());
    }

    #[tokio::test]
    async fn test_create_worktree_path_already_exists() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/existing");
        std::fs::create_dir_all(&wt_path).unwrap();

        let result = create_worktree(&repo, &wt_path, "main").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GitError::PathAlreadyExists(_)),
            "Expected PathAlreadyExists, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_create_worktree_not_a_git_repo() {
        let dir = TempDir::new().unwrap();
        let fake_repo = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&fake_repo).unwrap();
        let wt_path = dir.path().join("worktrees/feature");

        let result = create_worktree(&fake_repo, &wt_path, "main").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GitError::NotAGitRepository(_)),
            "Expected NotAGitRepository, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_create_worktree_invalid_branch() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/bad-branch");

        let result = create_worktree(&repo, &wt_path, "nonexistent-branch").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GitError::CommandFailed(_)),
            "Expected CommandFailed, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_validate_git_repo_valid() {
        let dir = TempDir::new().unwrap();
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let result = validate_git_repo(dir.path()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_validate_git_repo_invalid() {
        let dir = TempDir::new().unwrap();
        let result = validate_git_repo(dir.path()).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GitError::NotAGitRepository(_)
        ));
    }

    #[tokio::test]
    async fn test_create_worktree_creates_parent_dirs() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        // Nested path where parent dirs don't exist yet
        let wt_path = dir.path().join("deep/nested/worktree");

        let result = create_worktree(&repo, &wt_path, "main").await;
        assert!(result.is_ok(), "create_worktree failed: {:?}", result);
        assert!(wt_path.exists());
    }

    #[tokio::test]
    async fn test_run_git_command_success() {
        let dir = TempDir::new().unwrap();
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let result = run_git_command(dir.path(), &["status"]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_git_command_failure() {
        let dir = TempDir::new().unwrap();
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        let result = run_git_command(dir.path(), &["checkout", "nonexistent"]).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GitError::CommandFailed(_)));
    }
}
