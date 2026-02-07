use serde::{Deserialize, Serialize};

use crate::error::{NflowError, Result};

/// Full configuration with all fields resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub max_parallel: u32,
    pub git_provider: String,
    pub base_branch: String,
    pub cleanup_worktrees: bool,
    pub max_turns_per_task: u32,
    pub auto_execute: bool,
    pub max_time_per_task: u64,
    pub log_level: String,
    pub branch_template: String,
    pub worktree_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_parallel: 3,
            git_provider: "github".into(),
            base_branch: "main".into(),
            cleanup_worktrees: false,
            max_turns_per_task: 50,
            auto_execute: false,
            max_time_per_task: 1800,
            log_level: "info".into(),
            branch_template: "nflow/{project}/{story_id}-{story_slug}".into(),
            worktree_dir: "worktrees".into(),
        }
    }
}

/// Partial configuration for overlaying on top of a base config.
/// All fields are optional — only present fields override the base.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartialConfig {
    pub max_parallel: Option<u32>,
    pub git_provider: Option<String>,
    pub base_branch: Option<String>,
    pub cleanup_worktrees: Option<bool>,
    pub max_turns_per_task: Option<u32>,
    pub auto_execute: Option<bool>,
    pub max_time_per_task: Option<u64>,
    pub log_level: Option<String>,
    pub branch_template: Option<String>,
    pub worktree_dir: Option<String>,
}

/// Environment variable mapping to config fields.
/// Returns a PartialConfig populated from env var values.
///
/// Env var mapping:
/// - NFLOW_MAX_PARALLEL -> max_parallel
/// - NFLOW_LOG_LEVEL -> log_level
/// - NFLOW_HOME -> (not a config field, handled by caller)
/// - NFLOW_SOCKET -> (not a config field, handled by caller)
///
/// This function takes the env values as arguments to keep nflow-core pure (no IO).
pub fn partial_config_from_env(vars: &[(&str, &str)]) -> PartialConfig {
    let mut partial = PartialConfig::default();

    for &(key, value) in vars {
        match key {
            "NFLOW_MAX_PARALLEL" => {
                if let Ok(v) = value.parse::<u32>() {
                    partial.max_parallel = Some(v);
                }
            }
            "NFLOW_LOG_LEVEL" => {
                partial.log_level = Some(value.to_string());
            }
            _ => {}
        }
    }

    partial
}

/// Merge a base config with a partial overlay.
/// Overlay fields override base when present.
pub fn merge(base: Config, overlay: PartialConfig) -> Config {
    Config {
        max_parallel: overlay.max_parallel.unwrap_or(base.max_parallel),
        git_provider: overlay.git_provider.unwrap_or(base.git_provider),
        base_branch: overlay.base_branch.unwrap_or(base.base_branch),
        cleanup_worktrees: overlay.cleanup_worktrees.unwrap_or(base.cleanup_worktrees),
        max_turns_per_task: overlay
            .max_turns_per_task
            .unwrap_or(base.max_turns_per_task),
        auto_execute: overlay.auto_execute.unwrap_or(base.auto_execute),
        max_time_per_task: overlay.max_time_per_task.unwrap_or(base.max_time_per_task),
        log_level: overlay.log_level.unwrap_or(base.log_level),
        branch_template: overlay.branch_template.unwrap_or(base.branch_template),
        worktree_dir: overlay.worktree_dir.unwrap_or(base.worktree_dir),
    }
}

/// Validate a resolved config. Returns an error if any field has an invalid value.
pub fn validate(config: &Config) -> Result<()> {
    if config.max_parallel == 0 {
        return Err(NflowError::ValidationError(
            "max_parallel must be greater than 0".into(),
        ));
    }
    if config.max_time_per_task == 0 {
        return Err(NflowError::ValidationError(
            "max_time_per_task must be greater than 0".into(),
        ));
    }
    if config.git_provider != "github" && config.git_provider != "gitlab" {
        return Err(NflowError::ValidationError(format!(
            "git_provider must be 'github' or 'gitlab', got '{}'",
            config.git_provider
        )));
    }
    Ok(())
}

/// Render a branch template by substituting variables.
///
/// Supported variables: `{project}`, `{story_id}`, `{story_slug}`.
pub fn render_branch_template(
    template: &str,
    project: &str,
    story_id: &str,
    story_slug: &str,
) -> String {
    template
        .replace("{project}", project)
        .replace("{story_id}", story_id)
        .replace("{story_slug}", story_slug)
}

/// Build a fully resolved config by merging layers in order:
/// defaults -> global -> per-project -> env vars.
pub fn resolve(global: PartialConfig, project: PartialConfig, env: PartialConfig) -> Config {
    let config = merge(Config::default(), global);
    let config = merge(config, project);
    merge(config, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_correct_values() {
        let config = Config::default();
        assert_eq!(config.max_parallel, 3);
        assert_eq!(config.git_provider, "github");
        assert_eq!(config.base_branch, "main");
        assert!(!config.cleanup_worktrees);
        assert_eq!(config.max_turns_per_task, 50);
        assert!(!config.auto_execute);
        assert_eq!(config.max_time_per_task, 1800);
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn merge_empty_overlay_returns_base() {
        let base = Config::default();
        let overlay = PartialConfig::default();
        let merged = merge(base.clone(), overlay);
        assert_eq!(merged, base);
    }

    #[test]
    fn merge_full_overlay_overrides_all_fields() {
        let base = Config::default();
        let overlay = PartialConfig {
            max_parallel: Some(8),
            git_provider: Some("gitlab".into()),
            base_branch: Some("develop".into()),
            cleanup_worktrees: Some(true),
            max_turns_per_task: Some(100),
            auto_execute: Some(true),
            max_time_per_task: Some(3600),
            log_level: Some("debug".into()),
            branch_template: Some("feature/{story_id}".into()),
            worktree_dir: Some("/tmp/wt".into()),
        };
        let merged = merge(base, overlay);
        assert_eq!(merged.max_parallel, 8);
        assert_eq!(merged.git_provider, "gitlab");
        assert_eq!(merged.base_branch, "develop");
        assert!(merged.cleanup_worktrees);
        assert_eq!(merged.max_turns_per_task, 100);
        assert!(merged.auto_execute);
        assert_eq!(merged.max_time_per_task, 3600);
        assert_eq!(merged.log_level, "debug");
        assert_eq!(merged.branch_template, "feature/{story_id}");
        assert_eq!(merged.worktree_dir, "/tmp/wt");
    }

    #[test]
    fn merge_partial_overlay_overrides_only_present_fields() {
        let base = Config::default();
        let overlay = PartialConfig {
            max_parallel: Some(10),
            log_level: Some("warn".into()),
            ..Default::default()
        };
        let merged = merge(base, overlay);
        assert_eq!(merged.max_parallel, 10);
        assert_eq!(merged.log_level, "warn");
        // Other fields remain default
        assert_eq!(merged.git_provider, "github");
        assert_eq!(merged.base_branch, "main");
        assert!(!merged.cleanup_worktrees);
        assert_eq!(merged.max_turns_per_task, 50);
        assert!(!merged.auto_execute);
        assert_eq!(merged.max_time_per_task, 1800);
    }

    #[test]
    fn resolve_merges_layers_in_order() {
        let global = PartialConfig {
            max_parallel: Some(5),
            log_level: Some("debug".into()),
            ..Default::default()
        };
        let project = PartialConfig {
            max_parallel: Some(2),
            git_provider: Some("gitlab".into()),
            ..Default::default()
        };
        let env = PartialConfig {
            log_level: Some("error".into()),
            ..Default::default()
        };
        let config = resolve(global, project, env);
        // project overrides global for max_parallel
        assert_eq!(config.max_parallel, 2);
        // project sets git_provider
        assert_eq!(config.git_provider, "gitlab");
        // env overrides global for log_level
        assert_eq!(config.log_level, "error");
        // defaults for everything else
        assert_eq!(config.base_branch, "main");
    }

    #[test]
    fn validate_valid_config() {
        let config = Config::default();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_max_parallel_zero_fails() {
        let mut config = Config::default();
        config.max_parallel = 0;
        let err = validate(&config).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn validate_max_time_per_task_zero_fails() {
        let mut config = Config::default();
        config.max_time_per_task = 0;
        let err = validate(&config).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn validate_invalid_git_provider_fails() {
        let mut config = Config::default();
        config.git_provider = "bitbucket".into();
        let err = validate(&config).unwrap_err();
        assert!(matches!(err, NflowError::ValidationError(_)));
    }

    #[test]
    fn validate_github_provider_passes() {
        let mut config = Config::default();
        config.git_provider = "github".into();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_gitlab_provider_passes() {
        let mut config = Config::default();
        config.git_provider = "gitlab".into();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn render_branch_template_substitutes_all_variables() {
        let result = render_branch_template(
            "nflow/{project}/{story_id}-{story_slug}",
            "my-project",
            "W1-S3",
            "add-auth",
        );
        assert_eq!(result, "nflow/my-project/W1-S3-add-auth");
    }

    #[test]
    fn render_branch_template_no_variables() {
        let result = render_branch_template("static-branch", "proj", "S1", "slug");
        assert_eq!(result, "static-branch");
    }

    #[test]
    fn render_branch_template_repeated_variables() {
        let result = render_branch_template("{project}/{project}", "foo", "S1", "bar");
        assert_eq!(result, "foo/foo");
    }

    #[test]
    fn render_branch_template_partial_variables() {
        let result = render_branch_template("feat/{story_id}", "proj", "S5", "slug");
        assert_eq!(result, "feat/S5");
    }

    #[test]
    fn partial_config_from_env_parses_max_parallel() {
        let vars = vec![("NFLOW_MAX_PARALLEL", "8")];
        let partial = partial_config_from_env(&vars);
        assert_eq!(partial.max_parallel, Some(8));
        assert!(partial.log_level.is_none());
    }

    #[test]
    fn partial_config_from_env_parses_log_level() {
        let vars = vec![("NFLOW_LOG_LEVEL", "debug")];
        let partial = partial_config_from_env(&vars);
        assert_eq!(partial.log_level.as_deref(), Some("debug"));
        assert!(partial.max_parallel.is_none());
    }

    #[test]
    fn partial_config_from_env_ignores_unknown_vars() {
        let vars = vec![
            ("NFLOW_HOME", "/home/user/.nflow"),
            ("NFLOW_SOCKET", "/tmp/s"),
        ];
        let partial = partial_config_from_env(&vars);
        assert!(partial.max_parallel.is_none());
        assert!(partial.log_level.is_none());
    }

    #[test]
    fn partial_config_from_env_invalid_number_ignored() {
        let vars = vec![("NFLOW_MAX_PARALLEL", "not_a_number")];
        let partial = partial_config_from_env(&vars);
        assert!(partial.max_parallel.is_none());
    }

    #[test]
    fn partial_config_from_env_multiple_vars() {
        let vars = vec![("NFLOW_MAX_PARALLEL", "12"), ("NFLOW_LOG_LEVEL", "warn")];
        let partial = partial_config_from_env(&vars);
        assert_eq!(partial.max_parallel, Some(12));
        assert_eq!(partial.log_level.as_deref(), Some("warn"));
    }

    #[test]
    fn full_merge_chain_defaults_global_project_env() {
        // Simulate: defaults -> global toml -> per-project toml -> env vars
        let global = PartialConfig {
            max_parallel: Some(5),
            log_level: Some("debug".into()),
            branch_template: Some("feat/{story_id}/{story_slug}".into()),
            ..Default::default()
        };
        let project = PartialConfig {
            git_provider: Some("gitlab".into()),
            max_parallel: Some(2),
            ..Default::default()
        };
        let env_vars = vec![("NFLOW_LOG_LEVEL", "error")];
        let env = partial_config_from_env(&env_vars);

        let config = resolve(global, project, env);

        // env overrides global log_level
        assert_eq!(config.log_level, "error");
        // project overrides global max_parallel
        assert_eq!(config.max_parallel, 2);
        // project sets git_provider
        assert_eq!(config.git_provider, "gitlab");
        // global sets branch_template
        assert_eq!(config.branch_template, "feat/{story_id}/{story_slug}");
        // defaults for the rest
        assert_eq!(config.base_branch, "main");
        assert!(!config.cleanup_worktrees);
        assert_eq!(config.max_turns_per_task, 50);
        assert!(!config.auto_execute);
        assert_eq!(config.max_time_per_task, 1800);
    }

    #[test]
    fn validate_after_merge_catches_invalid_values() {
        let overlay = PartialConfig {
            max_parallel: Some(0),
            ..Default::default()
        };
        let config = merge(Config::default(), overlay);
        assert!(validate(&config).is_err());
    }
}
