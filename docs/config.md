# nflow — Configuration

## Config File

Location: `~/.nflow/config.toml`

Created with defaults on first `nflow init` or `nflow config set`.

## Full Reference

```toml
# Maximum number of parallel agent processes (global across all projects)
max_parallel = 3

# Default git provider for new projects: "github" or "gitlab"
git_provider = "github"

# Base branch for new projects (can be overridden per-project)
base_branch = "main"

# Whether to remove worktrees after successful MR creation.
# When true: worktree is removed immediately after MR is created (story status = done).
# When false: worktrees persist until manually removed or `nflow worktree clean` is run.
# Worktrees for failed/cancelled stories are never auto-removed (user may need to inspect).
cleanup_worktrees = false

# Claude model to use (default: whatever `claude` uses)
# model = "claude-sonnet-4-20250514"

# Max agentic turns per task (safety limit)
# Applies to impl tasks. Verify tasks use a hardcoded limit of 30 turns
# (verification should be quick — build, test, check criteria).
max_turns_per_task = 50

# Start execution automatically after plan approval.
# When false (default): after `nflow plan approve`, stories remain `ready` but
# the scheduler does not pick them up until the user runs `nflow run`.
# When true: scheduler picks up ready stories immediately after plan approval.
auto_execute = false

# Max wall-clock time per task in seconds (safety limit)
# Kills the agent process if it exceeds this time. Protects against
# agents stuck on long-running commands (infinite builds, hanging tests).
# Default: 1800 (30 minutes). Set to 0 to disable.
max_time_per_task = 1800

# Log level for daemon: "error", "warn", "info", "debug", "trace"
log_level = "info"

# Branch naming template
# Available variables: {project}, {story_id}, {story_slug}
branch_template = "nflow/{project}/{story_id}-{story_slug}"

# Worktree base directory
worktree_dir = "~/.nflow/worktrees"

# Prompt templates directory (custom overrides)
# prompts_dir = "~/.nflow/prompts"
```

## Per-Project Overrides

Project-specific settings are stored in SQLite (projects table) and override global config:

- `base_branch` — per project
- `git_provider` — per project

Set via:
```bash
nflow config set --project myapp git_provider gitlab
nflow config set --project myapp base_branch develop
```

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `NFLOW_HOME` | Override `~/.nflow` location | `~/.nflow` |
| `NFLOW_SOCKET` | Override socket path | `~/.nflow/nflow.sock` |
| `NFLOW_LOG_LEVEL` | Override log level | from config |
| `NFLOW_MAX_PARALLEL` | Override max parallel | from config |

## Parallelism

`max_parallel` is a **global** limit across all projects. If `max_parallel = 3` and two projects are running, the 3 slots are shared between them. There is no per-project parallelism limit — the scheduler picks the next `ready` story from any project, first-come first-served.

## Log Management

nflow does not perform automatic log rotation. Logs accumulate over time:
- `~/.nflow/logs/daemon.log` — daemon process log
- `~/.nflow/projects/{name}/agent-logs/{wave_short_id}.log` — per-task agent output

Cleanup options:
- `nflow cleanup --logs` — remove agent logs for done stories
- `nflow cleanup --logs --older-than 30d` — remove agent logs older than 30 days
- `nflow worktree clean` — removes worktrees (not logs)
- External tools like `logrotate` can be configured for `daemon.log`

## Config Precedence

```
Environment variable > CLI flag > Per-project config > Global config > Default
```
