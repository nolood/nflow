use std::path::Path;

use crate::error::{GitError, Result};
use crate::worktree::{run_git_command, validate_git_repo};

/// Creates a new branch in the given worktree via `git checkout -b`.
pub async fn create_branch(worktree_path: &Path, branch_name: &str) -> Result<()> {
    validate_git_repo(worktree_path).await?;
    run_git_command(worktree_path, &["checkout", "-b", branch_name]).await?;
    Ok(())
}

/// Formats a branch name by substituting variables in a template.
///
/// The `vars` map supports: `{project}`, `{story_id}`, `{story_slug}`.
/// The story slug is auto-slugified from the raw title.
pub fn format_branch_name(
    template: &str,
    project: &str,
    story_id: &str,
    story_title: &str,
) -> String {
    let slug = slugify(story_title);
    template
        .replace("{project}", project)
        .replace("{story_id}", story_id)
        .replace("{story_slug}", &slug)
}

/// Slugifies a string: lowercase, replace non-alphanumeric with hyphens,
/// collapse consecutive hyphens, trim leading/trailing hyphens, truncate to 50 chars.
pub fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());

    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else {
            // Replace any non-alphanumeric char with a hyphen
            slug.push('-');
        }
    }

    // Collapse consecutive hyphens
    let mut collapsed = String::with_capacity(slug.len());
    let mut prev_hyphen = false;
    for ch in slug.chars() {
        if ch == '-' {
            if !prev_hyphen {
                collapsed.push('-');
            }
            prev_hyphen = true;
        } else {
            collapsed.push(ch);
            prev_hyphen = false;
        }
    }

    // Trim leading/trailing hyphens
    let trimmed = collapsed.trim_matches('-');

    // Truncate to 50 chars at a clean boundary (don't cut in the middle of a word segment)
    if trimmed.len() <= 50 {
        return trimmed.to_string();
    }

    let truncated = &trimmed[..50];
    // Trim trailing hyphens from truncation point
    truncated.trim_end_matches('-').to_string()
}

/// Returns the current branch name of the given worktree/repo path.
pub async fn get_current_branch(worktree_path: &Path) -> Result<String> {
    validate_git_repo(worktree_path).await?;
    let output = run_git_command(worktree_path, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    Ok(output.trim().to_string())
}

/// Returns the HEAD commit hash (full SHA) of the given worktree/repo path.
pub async fn get_head_commit(worktree_path: &Path) -> Result<String> {
    validate_git_repo(worktree_path).await?;
    let output = run_git_command(worktree_path, &["rev-parse", "HEAD"]).await?;
    Ok(output.trim().to_string())
}

/// Returns the commit message of the HEAD commit.
pub async fn get_commit_message(worktree_path: &Path) -> Result<String> {
    validate_git_repo(worktree_path).await?;
    let output = run_git_command(worktree_path, &["log", "-1", "--format=%B"]).await?;
    Ok(output.trim().to_string())
}

/// Fetches the latest changes from origin and rebases the current branch onto `origin/{base_branch}`.
///
/// On rebase conflict: aborts the rebase and returns `GitError::RebaseConflict`.
pub async fn fetch_and_rebase(worktree_path: &Path, base_branch: &str) -> Result<()> {
    validate_git_repo(worktree_path).await?;

    // Fetch latest from origin
    run_git_command(worktree_path, &["fetch", "origin", base_branch]).await?;

    // Attempt rebase
    let remote_ref = format!("origin/{base_branch}");
    let result = run_git_command(worktree_path, &["rebase", &remote_ref]).await;

    match result {
        Ok(_) => Ok(()),
        Err(GitError::CommandFailed(msg)) => {
            // Abort the rebase to restore clean state
            let _ = run_git_command(worktree_path, &["rebase", "--abort"]).await;
            Err(GitError::RebaseConflict { details: msg })
        }
        Err(e) => Err(e),
    }
}

/// Pushes a branch to the remote with tracking (`git push -u origin {branch_name}`).
///
/// Classifies push failures into distinct error types:
/// - `AuthError`: authentication/permission failures
/// - `NetworkError`: network connectivity issues
/// - `BranchProtection`: branch protection rule violations
/// - `CommandFailed`: other push failures
pub async fn push_branch(worktree_path: &Path, branch_name: &str) -> Result<()> {
    validate_git_repo(worktree_path).await?;

    let result = run_git_command(worktree_path, &["push", "-u", "origin", branch_name]).await;

    match result {
        Ok(_) => Ok(()),
        Err(GitError::CommandFailed(msg)) => {
            let lower = msg.to_lowercase();
            if lower.contains("authentication")
                || lower.contains("permission denied")
                || lower.contains("could not read from remote")
                || lower.contains("invalid credentials")
                || lower.contains("authorization")
            {
                Err(GitError::AuthError(msg))
            } else if lower.contains("could not resolve host")
                || lower.contains("unable to access")
                || lower.contains("connection refused")
                || lower.contains("network")
                || lower.contains("timed out")
            {
                Err(GitError::NetworkError(msg))
            } else if lower.contains("protected branch")
                || lower.contains("denied to")
                || lower.contains("pre-receive hook declined")
                || lower.contains("required status check")
            {
                Err(GitError::BranchProtection(msg))
            } else {
                Err(GitError::CommandFailed(msg))
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Helper: create a git repo with an initial commit.
    async fn setup_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        tokio::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();

        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();

        let readme = repo.join("README.md");
        std::fs::write(&readme, "# test\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();

        (dir, repo)
    }

    // --- create_branch tests ---

    #[tokio::test]
    async fn test_create_branch_success() {
        let (_dir, repo) = setup_repo().await;

        let result = create_branch(&repo, "feature/my-branch").await;
        assert!(result.is_ok(), "create_branch failed: {:?}", result);

        let branch = get_current_branch(&repo).await.unwrap();
        assert_eq!(branch, "feature/my-branch");
    }

    #[tokio::test]
    async fn test_create_branch_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let fake = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&fake).unwrap();

        let result = create_branch(&fake, "some-branch").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            crate::error::GitError::NotAGitRepository(_)
        ));
    }

    #[tokio::test]
    async fn test_create_branch_already_exists() {
        let (_dir, repo) = setup_repo().await;

        // Create the branch first
        create_branch(&repo, "dup-branch").await.unwrap();
        // Switch back to main
        run_git_command(&repo, &["checkout", "main"]).await.unwrap();

        // Try creating the same branch again — should fail
        let result = create_branch(&repo, "dup-branch").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            crate::error::GitError::CommandFailed(_)
        ));
    }

    // --- slugify tests ---

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("Add User Auth"), "add-user-auth");
    }

    #[test]
    fn test_slugify_special_chars() {
        assert_eq!(slugify("Fix bug #123: crash!"), "fix-bug-123-crash");
    }

    #[test]
    fn test_slugify_collapses_hyphens() {
        assert_eq!(slugify("a   b---c"), "a-b-c");
    }

    #[test]
    fn test_slugify_trims_hyphens() {
        assert_eq!(slugify("--leading and trailing--"), "leading-and-trailing");
    }

    #[test]
    fn test_slugify_truncates_to_50_chars() {
        let long_title =
            "This is a very long story title that exceeds fifty characters by quite a bit";
        let slug = slugify(long_title);
        assert!(slug.len() <= 50, "Slug length {} exceeds 50", slug.len());
        assert!(!slug.ends_with('-'), "Slug should not end with hyphen");
    }

    #[test]
    fn test_slugify_empty_string() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn test_slugify_only_special_chars() {
        assert_eq!(slugify("!!!@@@###"), "");
    }

    #[test]
    fn test_slugify_unicode() {
        // Non-ASCII chars become hyphens
        assert_eq!(slugify("café résumé"), "caf-r-sum");
    }

    #[test]
    fn test_slugify_exactly_50_chars() {
        // 50 chars of alphanumeric
        let title = "a".repeat(50);
        assert_eq!(slugify(&title).len(), 50);
    }

    // --- format_branch_name tests ---

    #[test]
    fn test_format_branch_name_default_template() {
        let result = format_branch_name(
            "nflow/{project}/{story_id}-{story_slug}",
            "my-project",
            "W1-S3",
            "Add User Auth",
        );
        assert_eq!(result, "nflow/my-project/W1-S3-add-user-auth");
    }

    #[test]
    fn test_format_branch_name_custom_template() {
        let result = format_branch_name("feat/{story_id}", "proj", "S5", "Some Feature");
        assert_eq!(result, "feat/S5");
    }

    #[test]
    fn test_format_branch_name_no_variables() {
        let result = format_branch_name("static-branch", "proj", "S1", "title");
        assert_eq!(result, "static-branch");
    }

    #[test]
    fn test_format_branch_name_slugifies_title() {
        let result =
            format_branch_name("{story_slug}", "proj", "S1", "Fix Bug #42: Memory Leak!!!");
        assert_eq!(result, "fix-bug-42-memory-leak");
    }

    // --- get_current_branch tests ---

    #[tokio::test]
    async fn test_get_current_branch() {
        let (_dir, repo) = setup_repo().await;

        let branch = get_current_branch(&repo).await.unwrap();
        assert_eq!(branch, "main");
    }

    #[tokio::test]
    async fn test_get_current_branch_after_checkout() {
        let (_dir, repo) = setup_repo().await;
        create_branch(&repo, "feature/test").await.unwrap();

        let branch = get_current_branch(&repo).await.unwrap();
        assert_eq!(branch, "feature/test");
    }

    #[tokio::test]
    async fn test_get_current_branch_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let result = get_current_branch(dir.path()).await;
        assert!(result.is_err());
    }

    // --- get_head_commit tests ---

    #[tokio::test]
    async fn test_get_head_commit() {
        let (_dir, repo) = setup_repo().await;

        let commit = get_head_commit(&repo).await.unwrap();
        // SHA-1 hex is 40 chars
        assert_eq!(commit.len(), 40, "Expected 40-char SHA, got: {}", commit);
        assert!(
            commit.chars().all(|c| c.is_ascii_hexdigit()),
            "Expected hex SHA, got: {}",
            commit
        );
    }

    #[tokio::test]
    async fn test_get_head_commit_changes_after_commit() {
        let (_dir, repo) = setup_repo().await;

        let before = get_head_commit(&repo).await.unwrap();

        // Make a new commit
        std::fs::write(repo.join("new.txt"), "content").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "second"])
            .await
            .unwrap();

        let after = get_head_commit(&repo).await.unwrap();
        assert_ne!(before, after, "HEAD should change after new commit");
    }

    #[tokio::test]
    async fn test_get_head_commit_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let result = get_head_commit(dir.path()).await;
        assert!(result.is_err());
    }

    // --- get_commit_message tests ---

    #[tokio::test]
    async fn test_get_commit_message() {
        let (_dir, repo) = setup_repo().await;

        let msg = get_commit_message(&repo).await.unwrap();
        assert_eq!(msg, "initial commit");
    }

    #[tokio::test]
    async fn test_get_commit_message_after_new_commit() {
        let (_dir, repo) = setup_repo().await;

        std::fs::write(repo.join("file.txt"), "data").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "[W1-T1] Implement feature"])
            .await
            .unwrap();

        let msg = get_commit_message(&repo).await.unwrap();
        assert_eq!(msg, "[W1-T1] Implement feature");
    }

    #[tokio::test]
    async fn test_get_commit_message_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let result = get_commit_message(dir.path()).await;
        assert!(result.is_err());
    }

    // --- fetch_and_rebase tests ---

    /// Helper: create a repo with remote (bare origin + clone), same as worktree tests.
    async fn setup_repo_with_remote() -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().unwrap();
        let bare_path = dir.path().join("origin.git");
        let clone_path = dir.path().join("repo");

        // Create bare repo
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
    async fn test_fetch_and_rebase_success_no_changes() {
        let (_dir, _bare, repo) = setup_repo_with_remote().await;

        // Create a feature branch
        create_branch(&repo, "feature/rebase-test").await.unwrap();

        // Rebase on main — no divergence, should succeed trivially
        let result = fetch_and_rebase(&repo, "main").await;
        assert!(
            result.is_ok(),
            "fetch_and_rebase failed: {:?}",
            result.unwrap_err()
        );
    }

    #[tokio::test]
    async fn test_fetch_and_rebase_success_with_upstream_changes() {
        let (dir, bare, repo) = setup_repo_with_remote().await;

        // Create a feature branch with a commit
        create_branch(&repo, "feature/rebase-upstream")
            .await
            .unwrap();
        std::fs::write(repo.join("feature.txt"), "feature work").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "feature commit"])
            .await
            .unwrap();

        // Simulate upstream changes: create another clone, push to main
        let other_clone = dir.path().join("other");
        tokio::process::Command::new("git")
            .args([
                "clone",
                bare.to_str().unwrap(),
                other_clone.to_str().unwrap(),
            ])
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "other@test.com"])
            .current_dir(&other_clone)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Other"])
            .current_dir(&other_clone)
            .output()
            .await
            .unwrap();
        std::fs::write(other_clone.join("upstream.txt"), "upstream change").unwrap();
        run_git_command(&other_clone, &["add", "."]).await.unwrap();
        run_git_command(&other_clone, &["commit", "-m", "upstream commit"])
            .await
            .unwrap();
        run_git_command(&other_clone, &["push", "origin", "main"])
            .await
            .unwrap();

        // Now rebase feature branch onto updated main — should succeed (no conflict)
        let result = fetch_and_rebase(&repo, "main").await;
        assert!(
            result.is_ok(),
            "fetch_and_rebase with upstream changes failed: {:?}",
            result.unwrap_err()
        );

        // Verify upstream file is now visible
        assert!(
            repo.join("upstream.txt").exists(),
            "Upstream file should be present after rebase"
        );
    }

    #[tokio::test]
    async fn test_fetch_and_rebase_conflict() {
        let (dir, bare, repo) = setup_repo_with_remote().await;

        // Create a feature branch that modifies README.md
        create_branch(&repo, "feature/conflict").await.unwrap();
        std::fs::write(repo.join("README.md"), "feature changes to readme\n").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "feature: modify readme"])
            .await
            .unwrap();

        // Simulate conflicting upstream change on same file
        let other_clone = dir.path().join("other");
        tokio::process::Command::new("git")
            .args([
                "clone",
                bare.to_str().unwrap(),
                other_clone.to_str().unwrap(),
            ])
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "other@test.com"])
            .current_dir(&other_clone)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Other"])
            .current_dir(&other_clone)
            .output()
            .await
            .unwrap();
        std::fs::write(
            other_clone.join("README.md"),
            "upstream changes to readme\n",
        )
        .unwrap();
        run_git_command(&other_clone, &["add", "."]).await.unwrap();
        run_git_command(&other_clone, &["commit", "-m", "upstream: modify readme"])
            .await
            .unwrap();
        run_git_command(&other_clone, &["push", "origin", "main"])
            .await
            .unwrap();

        // Rebase should detect conflict and abort
        let result = fetch_and_rebase(&repo, "main").await;
        assert!(result.is_err(), "Expected rebase conflict error");
        let err = result.unwrap_err();
        assert!(
            matches!(err, GitError::RebaseConflict { .. }),
            "Expected RebaseConflict, got: {:?}",
            err
        );

        // Verify rebase was aborted (no .git/rebase-apply or rebase-merge dir)
        // The branch should be clean after abort
        let status = run_git_command(&repo, &["status", "--porcelain"])
            .await
            .unwrap();
        assert!(
            status.trim().is_empty(),
            "Working tree should be clean after rebase abort, got: {}",
            status
        );
    }

    #[tokio::test]
    async fn test_fetch_and_rebase_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let fake = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&fake).unwrap();

        let result = fetch_and_rebase(&fake, "main").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GitError::NotAGitRepository(_)
        ));
    }

    // --- push_branch tests ---

    #[tokio::test]
    async fn test_push_branch_success() {
        let (_dir, _bare, repo) = setup_repo_with_remote().await;

        // Create a feature branch with a commit
        create_branch(&repo, "feature/push-test").await.unwrap();
        std::fs::write(repo.join("push.txt"), "push content").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "push test commit"])
            .await
            .unwrap();

        // Push the branch
        let result = push_branch(&repo, "feature/push-test").await;
        assert!(
            result.is_ok(),
            "push_branch failed: {:?}",
            result.unwrap_err()
        );
    }

    #[tokio::test]
    async fn test_push_branch_sets_upstream() {
        let (_dir, _bare, repo) = setup_repo_with_remote().await;

        create_branch(&repo, "feature/upstream-test").await.unwrap();
        std::fs::write(repo.join("upstream.txt"), "content").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "upstream test"])
            .await
            .unwrap();

        push_branch(&repo, "feature/upstream-test").await.unwrap();

        // Verify upstream is set
        let tracking = run_git_command(
            &repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .await
        .unwrap();
        assert_eq!(tracking.trim(), "origin/feature/upstream-test");
    }

    #[tokio::test]
    async fn test_push_branch_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let fake = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&fake).unwrap();

        let result = push_branch(&fake, "some-branch").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            GitError::NotAGitRepository(_)
        ));
    }

    #[tokio::test]
    async fn test_push_branch_nonexistent_remote() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // Init repo without a remote
        tokio::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        run_git_command(&repo, &["add", "."]).await.unwrap();
        run_git_command(&repo, &["commit", "-m", "init"])
            .await
            .unwrap();

        // Push should fail (no remote named "origin")
        let result = push_branch(&repo, "main").await;
        assert!(result.is_err(), "Expected push to fail without remote");
    }
}
