use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{NflowError, Result};

/// Git hosting provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitProvider {
    Github,
    Gitlab,
}

impl GitProvider {
    /// Detect the git provider from a remote URL string.
    /// Returns `None` if the provider cannot be determined.
    pub fn detect_from_remote(remote_url: &str) -> Option<Self> {
        let lower = remote_url.to_lowercase();
        if lower.contains("github.com") {
            Some(GitProvider::Github)
        } else if lower.contains("gitlab.com") {
            Some(GitProvider::Gitlab)
        } else {
            None
        }
    }
}

/// A project managed by nflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub base_branch: String,
    pub git_provider: GitProvider,
    pub execution_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Parameters for creating a new project.
pub struct CreateProjectParams {
    pub name: String,
    pub path: String,
    pub remote_url: Option<String>,
    pub git_provider_override: Option<GitProvider>,
    pub head_ref: Option<String>,
}

pub struct ProjectService;

impl ProjectService {
    /// Create a new project with validation.
    ///
    /// `name_exists` is a pure checker — returns true if a project with the given name already exists.
    pub fn create(
        params: CreateProjectParams,
        name_exists: impl Fn(&str) -> bool,
    ) -> Result<Project> {
        // Validate name is non-empty
        let name = params.name.trim().to_string();
        if name.is_empty() {
            return Err(NflowError::ValidationError(
                "project name must not be empty".into(),
            ));
        }

        // Validate name is unique
        if name_exists(&name) {
            return Err(NflowError::AlreadyExists(format!(
                "project with name '{}' already exists",
                name
            )));
        }

        // Validate path is non-empty
        let path = params.path.trim().to_string();
        if path.is_empty() {
            return Err(NflowError::ValidationError(
                "project path must not be empty".into(),
            ));
        }

        // Detect git provider: manual override > auto-detection from remote URL > default (github)
        let git_provider = if let Some(provider) = params.git_provider_override {
            provider
        } else if let Some(ref url) = params.remote_url {
            GitProvider::detect_from_remote(url).unwrap_or(GitProvider::Github)
        } else {
            GitProvider::Github
        };

        // Detect base branch from HEAD ref or default to "main"
        let base_branch = params
            .head_ref
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "main".to_string());

        let now = Utc::now();

        Ok(Project {
            id: Uuid::new_v4(),
            name,
            path,
            base_branch,
            git_provider,
            execution_enabled: true,
            created_at: now,
            updated_at: now,
        })
    }

    /// Delete a project. Returns an error if the project has running agents.
    ///
    /// `running_agent_count` is a pure checker — returns the number of currently running agents
    /// for the given project id.
    pub fn delete(project_id: Uuid, running_agent_count: impl Fn(Uuid) -> usize) -> Result<()> {
        let count = running_agent_count(project_id);
        if count > 0 {
            return Err(NflowError::InvalidState(format!(
                "cannot delete project: {} agent(s) still running",
                count
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_existing_names(_name: &str) -> bool {
        false
    }

    fn name_already_exists(name: &str) -> bool {
        name == "existing-project"
    }

    fn no_running_agents(_id: Uuid) -> usize {
        0
    }

    fn has_running_agents(_id: Uuid) -> usize {
        3
    }

    #[test]
    fn create_project_success_with_github_detection() {
        let params = CreateProjectParams {
            name: "my-project".into(),
            path: "/home/user/project".into(),
            remote_url: Some("git@github.com:user/repo.git".into()),
            git_provider_override: None,
            head_ref: Some("develop".into()),
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.name, "my-project");
        assert_eq!(project.path, "/home/user/project");
        assert_eq!(project.git_provider, GitProvider::Github);
        assert_eq!(project.base_branch, "develop");
        assert!(project.execution_enabled);
    }

    #[test]
    fn create_project_success_with_gitlab_detection() {
        let params = CreateProjectParams {
            name: "gl-project".into(),
            path: "/home/user/project".into(),
            remote_url: Some("https://gitlab.com/user/repo.git".into()),
            git_provider_override: None,
            head_ref: None,
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.git_provider, GitProvider::Gitlab);
        assert_eq!(project.base_branch, "main");
    }

    #[test]
    fn create_project_with_provider_override() {
        let params = CreateProjectParams {
            name: "override-project".into(),
            path: "/some/path".into(),
            remote_url: Some("https://gitlab.com/user/repo.git".into()),
            git_provider_override: Some(GitProvider::Github),
            head_ref: None,
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.git_provider, GitProvider::Github);
    }

    #[test]
    fn create_project_empty_name_fails() {
        let params = CreateProjectParams {
            name: "   ".into(),
            path: "/some/path".into(),
            remote_url: Some("https://github.com/u/r".into()),
            git_provider_override: None,
            head_ref: None,
        };

        let err = ProjectService::create(params, no_existing_names).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn create_project_duplicate_name_fails() {
        let params = CreateProjectParams {
            name: "existing-project".into(),
            path: "/some/path".into(),
            remote_url: Some("https://github.com/u/r".into()),
            git_provider_override: None,
            head_ref: None,
        };

        let err = ProjectService::create(params, name_already_exists).unwrap_err();
        assert!(matches!(err, NflowError::AlreadyExists(_)));
    }

    #[test]
    fn create_project_empty_path_fails() {
        let params = CreateProjectParams {
            name: "valid-name".into(),
            path: "  ".into(),
            remote_url: Some("https://github.com/u/r".into()),
            git_provider_override: None,
            head_ref: None,
        };

        let err = ProjectService::create(params, no_existing_names).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn create_project_unknown_remote_defaults_to_github() {
        let params = CreateProjectParams {
            name: "my-proj".into(),
            path: "/some/path".into(),
            remote_url: Some("https://bitbucket.org/u/r".into()),
            git_provider_override: None,
            head_ref: None,
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.git_provider, GitProvider::Github);
    }

    #[test]
    fn create_project_no_remote_no_override_defaults_to_github() {
        let params = CreateProjectParams {
            name: "my-proj".into(),
            path: "/some/path".into(),
            remote_url: None,
            git_provider_override: None,
            head_ref: None,
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.git_provider, GitProvider::Github);
    }

    #[test]
    fn create_project_no_remote_with_override_succeeds() {
        let params = CreateProjectParams {
            name: "local-proj".into(),
            path: "/some/path".into(),
            remote_url: None,
            git_provider_override: Some(GitProvider::Github),
            head_ref: None,
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.git_provider, GitProvider::Github);
    }

    #[test]
    fn create_project_empty_head_ref_defaults_to_main() {
        let params = CreateProjectParams {
            name: "proj".into(),
            path: "/p".into(),
            remote_url: Some("https://github.com/u/r".into()),
            git_provider_override: None,
            head_ref: Some("  ".into()),
        };

        let project = ProjectService::create(params, no_existing_names).unwrap();
        assert_eq!(project.base_branch, "main");
    }

    #[test]
    fn delete_project_no_running_agents_succeeds() {
        let id = Uuid::new_v4();
        assert!(ProjectService::delete(id, no_running_agents).is_ok());
    }

    #[test]
    fn delete_project_with_running_agents_fails() {
        let id = Uuid::new_v4();
        let err = ProjectService::delete(id, has_running_agents).unwrap_err();
        assert!(matches!(err, NflowError::InvalidState(_)));
    }

    #[test]
    fn detect_github_from_remote() {
        assert_eq!(
            GitProvider::detect_from_remote("git@github.com:user/repo.git"),
            Some(GitProvider::Github)
        );
        assert_eq!(
            GitProvider::detect_from_remote("https://github.com/user/repo"),
            Some(GitProvider::Github)
        );
    }

    #[test]
    fn detect_gitlab_from_remote() {
        assert_eq!(
            GitProvider::detect_from_remote("git@gitlab.com:user/repo.git"),
            Some(GitProvider::Gitlab)
        );
    }

    #[test]
    fn detect_unknown_remote() {
        assert_eq!(
            GitProvider::detect_from_remote("https://bitbucket.org/user/repo"),
            None
        );
    }
}
