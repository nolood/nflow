use std::path::Path;
use std::process::Stdio;

use crate::error::{GitError, Result};
use crate::worktree::validate_git_repo;

/// Checks whether a CLI tool (e.g., `gh`, `glab`) is available in PATH.
async fn check_cli_available(tool: &str) -> Result<()> {
    let result = tokio::process::Command::new("which")
        .arg(tool)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => Err(GitError::CliNotFound(format!(
            "'{}' not found in PATH. Install it to create PRs/MRs.",
            tool
        ))),
    }
}

/// Checks whether `gh` is authenticated by running `gh auth status`.
///
/// Returns Ok(()) if authenticated, or AuthError with details if not.
async fn check_gh_auth(worktree_path: &Path) -> Result<()> {
    let output = tokio::process::Command::new("gh")
        .args(["auth", "status"])
        .current_dir(worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| GitError::CommandFailed(format!("Failed to run gh auth status: {}", e)))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(GitError::AuthError(format!(
            "gh is not authenticated: {}",
            stderr.trim()
        )))
    }
}

/// Creates a GitHub pull request via `gh pr create`.
///
/// Runs `gh pr create --title {title} --body {body} --base {base_branch}` in the
/// given worktree directory. Returns the PR URL on success.
///
/// # Errors
///
/// - `CliNotFound` if `gh` is not in PATH
/// - `AuthError` if `gh` is not authenticated
/// - `CommandFailed` if `gh pr create` fails for other reasons
pub async fn create_github_pr(
    worktree_path: &Path,
    title: &str,
    body: &str,
    base_branch: &str,
) -> Result<String> {
    validate_git_repo(worktree_path).await?;
    check_cli_available("gh").await?;
    check_gh_auth(worktree_path).await?;

    let output = tokio::process::Command::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            title,
            "--body",
            body,
            "--base",
            base_branch,
        ])
        .current_dir(worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| GitError::CommandFailed(format!("Failed to run gh pr create: {}", e)))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let pr_url = stdout.trim().to_string();
        if pr_url.is_empty() {
            return Err(GitError::CommandFailed(
                "gh pr create succeeded but returned no URL".to_string(),
            ));
        }
        Ok(pr_url)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim().to_string();
        let lower = msg.to_lowercase();

        if lower.contains("authentication")
            || lower.contains("not logged in")
            || lower.contains("auth login")
        {
            Err(GitError::AuthError(msg))
        } else {
            Err(GitError::CommandFailed(format!(
                "gh pr create failed: {}",
                msg
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_check_cli_available_gh() {
        // gh may or may not be installed — test that the function doesn't panic
        let result = check_cli_available("gh").await;
        // If gh is installed, result is Ok; if not, it's CliNotFound
        match result {
            Ok(()) => {}
            Err(GitError::CliNotFound(msg)) => {
                assert!(msg.contains("gh"), "Error should mention gh: {}", msg);
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_check_cli_available_nonexistent() {
        let result = check_cli_available("definitely-not-a-real-tool-xyz123").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GitError::CliNotFound(_)),
            "Expected CliNotFound, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_check_cli_available_git() {
        // git should always be available in our environment
        let result = check_cli_available("git").await;
        assert!(result.is_ok(), "git should be in PATH");
    }

    #[tokio::test]
    async fn test_create_github_pr_not_a_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let fake = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&fake).unwrap();

        let result = create_github_pr(&fake, "title", "body", "main").await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), GitError::NotAGitRepository(_)),
            "Expected NotAGitRepository for non-repo path"
        );
    }

    #[tokio::test]
    async fn test_create_github_pr_gh_not_installed() {
        // Skip if gh is actually installed — this test only validates behavior when gh is missing
        if check_cli_available("gh").await.is_ok() {
            // gh is installed, so we can't test the "not found" path directly.
            // Instead, verify the function signature and return type work.
            return;
        }

        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        tokio::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();

        let result = create_github_pr(&repo, "title", "body", "main").await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), GitError::CliNotFound(_)),
            "Expected CliNotFound when gh is not installed"
        );
    }

    #[tokio::test]
    async fn test_check_gh_auth_not_a_repo() {
        // check_gh_auth doesn't validate repo, but it runs in a directory.
        // If gh is not installed, this will fail at check_cli_available level in real flow.
        // This test verifies the function can be called without panicking.
        let dir = tempfile::TempDir::new().unwrap();
        let _ = check_gh_auth(dir.path()).await;
        // We don't assert the result since it depends on gh installation and auth state
    }

    #[test]
    fn test_cli_not_found_error_message() {
        let err = GitError::CliNotFound(
            "'gh' not found in PATH. Install it to create PRs/MRs.".to_string(),
        );
        let msg = format!("{}", err);
        assert!(msg.contains("gh"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_cli_not_found_variant_exists() {
        // Verify the CliNotFound variant can be constructed and matched
        let err = GitError::CliNotFound("test".to_string());
        assert!(matches!(err, GitError::CliNotFound(_)));
    }
}
