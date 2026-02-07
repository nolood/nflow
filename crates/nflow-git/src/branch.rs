use std::path::Path;

use crate::error::Result;
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
}
