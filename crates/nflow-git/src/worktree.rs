use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::{GitError, Result};

/// Information about a git worktree parsed from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub is_bare: bool,
}

/// Validates that the given path is a git repository via `git rev-parse`.
pub(crate) async fn validate_git_repo(repo_path: &Path) -> Result<()> {
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
pub(crate) async fn run_git_command(repo_path: &Path, args: &[&str]) -> Result<String> {
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

/// Removes a git worktree at `worktree_path`.
///
/// Runs `git worktree remove {worktree_path} --force`.
/// Handles already-removed worktrees gracefully — if the path doesn't exist,
/// prunes stale entries and returns Ok.
pub async fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    validate_git_repo(repo_path).await?;

    let worktree_str = worktree_path
        .to_str()
        .ok_or_else(|| GitError::CommandFailed("Invalid worktree path encoding".to_string()))?;

    // If the worktree directory doesn't exist anymore, just prune stale entries
    if !worktree_path.exists() {
        let _ = run_git_command(repo_path, &["worktree", "prune"]).await;
        return Ok(());
    }

    let result = run_git_command(repo_path, &["worktree", "remove", worktree_str, "--force"]).await;

    match result {
        Ok(_) => Ok(()),
        Err(GitError::CommandFailed(msg)) if msg.contains("is not a working tree") => {
            // Already removed or stale — prune and succeed
            let _ = run_git_command(repo_path, &["worktree", "prune"]).await;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Lists all worktrees for a repository by parsing `git worktree list --porcelain`.
///
/// Returns a Vec of WorktreeInfo structs, one per worktree (including the main repo).
pub async fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeInfo>> {
    validate_git_repo(repo_path).await?;

    let output = run_git_command(repo_path, &["worktree", "list", "--porcelain"]).await?;

    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head = String::new();
    let mut current_branch: Option<String> = None;
    let mut is_bare = false;

    for line in output.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            // If we were building a previous entry, push it
            if let Some(path) = current_path.take() {
                worktrees.push(WorktreeInfo {
                    path,
                    head: std::mem::take(&mut current_head),
                    branch: current_branch.take(),
                    is_bare,
                });
                is_bare = false;
            }
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(head_str) = line.strip_prefix("HEAD ") {
            current_head = head_str.to_string();
        } else if let Some(branch_str) = line.strip_prefix("branch ") {
            current_branch = Some(branch_str.to_string());
        } else if line == "bare" {
            is_bare = true;
        }
        // Blank lines separate entries, but we handle it via the "worktree " prefix detection
    }

    // Push the last entry
    if let Some(path) = current_path {
        worktrees.push(WorktreeInfo {
            path,
            head: current_head,
            branch: current_branch,
            is_bare,
        });
    }

    Ok(worktrees)
}

/// Prunes stale worktree entries that reference paths that no longer exist.
///
/// Runs `git worktree prune`.
pub async fn prune_worktrees(repo_path: &Path) -> Result<()> {
    validate_git_repo(repo_path).await?;
    run_git_command(repo_path, &["worktree", "prune"]).await?;
    Ok(())
}

/// Removes all nflow worktrees under `worktree_dir`, returning the paths that were removed.
///
/// Lists all worktrees, filters for those whose path is under `worktree_dir`,
/// removes each one, and prunes stale entries.
/// Handles already-removed worktrees gracefully.
pub async fn cleanup_all(repo_path: &Path, worktree_dir: &Path) -> Result<Vec<PathBuf>> {
    let worktrees = list_worktrees(repo_path).await?;

    let mut removed = Vec::new();

    for wt in &worktrees {
        if wt.path.starts_with(worktree_dir) && !wt.is_bare {
            let result = remove_worktree(repo_path, &wt.path).await;
            match result {
                Ok(()) => removed.push(wt.path.clone()),
                Err(_) => {
                    // Best-effort removal — continue with others
                    removed.push(wt.path.clone());
                }
            }
        }
    }

    // Final prune to clean up any stale entries
    let _ = prune_worktrees(repo_path).await;

    Ok(removed)
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

    #[tokio::test]
    async fn test_remove_worktree_success() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/to-remove");

        // Create a worktree first
        create_worktree(&repo, &wt_path, "main").await.unwrap();
        assert!(wt_path.exists());

        // Remove it
        let result = remove_worktree(&repo, &wt_path).await;
        assert!(result.is_ok(), "remove_worktree failed: {:?}", result);
        assert!(!wt_path.exists());
    }

    #[tokio::test]
    async fn test_remove_worktree_already_removed() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/already-gone");

        // Create then manually delete the directory
        create_worktree(&repo, &wt_path, "main").await.unwrap();
        std::fs::remove_dir_all(&wt_path).unwrap();
        assert!(!wt_path.exists());

        // remove_worktree should handle this gracefully
        let result = remove_worktree(&repo, &wt_path).await;
        assert!(
            result.is_ok(),
            "Expected graceful handling of already-removed worktree, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_remove_worktree_nonexistent_path() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/never-existed");

        // Path never existed — should succeed gracefully
        let result = remove_worktree(&repo, &wt_path).await;
        assert!(
            result.is_ok(),
            "Expected Ok for nonexistent path, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_list_worktrees() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt1 = dir.path().join("worktrees/feature-1");
        let wt2 = dir.path().join("worktrees/feature-2");

        create_worktree(&repo, &wt1, "main").await.unwrap();
        create_worktree(&repo, &wt2, "main").await.unwrap();

        let worktrees = list_worktrees(&repo).await.unwrap();

        // Should have at least 3: main repo + 2 worktrees
        assert!(
            worktrees.len() >= 3,
            "Expected at least 3 worktrees, got {}",
            worktrees.len()
        );

        let paths: Vec<&Path> = worktrees.iter().map(|w| w.path.as_path()).collect();
        assert!(paths.contains(&wt1.as_path()), "Missing worktree 1 in list");
        assert!(paths.contains(&wt2.as_path()), "Missing worktree 2 in list");
    }

    #[tokio::test]
    async fn test_list_worktrees_has_head_and_branch() {
        let (_dir, _bare, repo) = setup_repo_with_remote().await;

        let worktrees = list_worktrees(&repo).await.unwrap();
        assert!(!worktrees.is_empty());

        // The main worktree should have a HEAD commit and branch
        let main_wt = &worktrees[0];
        assert!(!main_wt.head.is_empty(), "HEAD should not be empty");
        assert!(
            main_wt.branch.is_some(),
            "Main worktree should have a branch"
        );
    }

    #[tokio::test]
    async fn test_list_worktrees_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let result = list_worktrees(dir.path()).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GitError::NotAGitRepository(_)
        ));
    }

    #[tokio::test]
    async fn test_prune_worktrees() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_path = dir.path().join("worktrees/to-prune");

        // Create a worktree then manually remove its directory
        create_worktree(&repo, &wt_path, "main").await.unwrap();
        std::fs::remove_dir_all(&wt_path).unwrap();

        // Prune should clean up the stale entry
        let result = prune_worktrees(&repo).await;
        assert!(result.is_ok(), "prune_worktrees failed: {:?}", result);

        // After pruning, listing should not include the stale worktree
        let worktrees = list_worktrees(&repo).await.unwrap();
        let paths: Vec<&Path> = worktrees.iter().map(|w| w.path.as_path()).collect();
        assert!(
            !paths.contains(&wt_path.as_path()),
            "Stale worktree should be removed after prune"
        );
    }

    #[tokio::test]
    async fn test_prune_worktrees_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let result = prune_worktrees(dir.path()).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GitError::NotAGitRepository(_)
        ));
    }

    #[tokio::test]
    async fn test_cleanup_all() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_dir = dir.path().join("worktrees");
        let wt1 = wt_dir.join("feature-1");
        let wt2 = wt_dir.join("feature-2");

        create_worktree(&repo, &wt1, "main").await.unwrap();
        create_worktree(&repo, &wt2, "main").await.unwrap();

        let removed = cleanup_all(&repo, &wt_dir).await.unwrap();
        assert_eq!(removed.len(), 2, "Expected 2 removed worktrees");
        assert!(removed.contains(&wt1));
        assert!(removed.contains(&wt2));

        // After cleanup, those directories should not exist
        assert!(!wt1.exists());
        assert!(!wt2.exists());
    }

    #[tokio::test]
    async fn test_cleanup_all_skips_non_nflow_worktrees() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let nflow_dir = dir.path().join("nflow-worktrees");
        let other_dir = dir.path().join("other-worktrees");
        let nflow_wt = nflow_dir.join("feature");
        let other_wt = other_dir.join("external");

        create_worktree(&repo, &nflow_wt, "main").await.unwrap();
        create_worktree(&repo, &other_wt, "main").await.unwrap();

        // Only clean up nflow_dir
        let removed = cleanup_all(&repo, &nflow_dir).await.unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed.contains(&nflow_wt));

        // other_wt should still exist
        assert!(other_wt.exists());
    }

    #[tokio::test]
    async fn test_cleanup_all_handles_already_removed() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_dir = dir.path().join("worktrees");
        let wt1 = wt_dir.join("feature-1");
        let wt2 = wt_dir.join("feature-2");

        create_worktree(&repo, &wt1, "main").await.unwrap();
        create_worktree(&repo, &wt2, "main").await.unwrap();

        // Manually remove one worktree directory
        std::fs::remove_dir_all(&wt1).unwrap();

        // cleanup_all should still succeed
        let removed = cleanup_all(&repo, &wt_dir).await.unwrap();
        // wt1 is stale but still listed by git, wt2 exists — both should be handled
        assert!(!removed.is_empty());
    }

    #[tokio::test]
    async fn test_cleanup_all_empty_dir() {
        let (dir, _bare, repo) = setup_repo_with_remote().await;
        let wt_dir = dir.path().join("empty-worktrees");

        // No worktrees under this dir — should return empty vec
        let removed = cleanup_all(&repo, &wt_dir).await.unwrap();
        assert!(removed.is_empty());
    }
}
