# PRD: nflow — CLI/TUI Orchestrator for Claude Code Agents

## Introduction

nflow is a CLI/TUI orchestrator that manages the full software development lifecycle using Claude Code agents. It takes a project from idea to merge requests through three phases: writing specs via interactive Claude dialogue, decomposing specs into a DAG of epics/stories/tasks, and executing those tasks via parallel Claude Code agents in isolated git worktrees. Each story produces a branch, commits, and a merge request.

The system is built as a six-crate Rust workspace: `nflow-core` (pure business logic), `nflow-claude` (Claude CLI wrapper), `nflow-git` (git operations), `nflow-daemon` (background process), `nflow-cli` (thin CLI client), and `nflow-tui` (terminal UI). The daemon is the sole writer to a SQLite database; CLI and TUI are read-only clients communicating via Unix socket with NDJSON protocol.

Target platforms: Linux and macOS from the start.

## Goals

- Deliver a full end-to-end flow from spec creation through decomposition to parallel agent execution
- Implement all six Rust crates as a functional workspace
- Provide both CLI and TUI interfaces for all three phases
- Support parallel agent execution across isolated git worktrees with configurable concurrency
- Support both GitHub (`gh`) and GitLab (`glab`) for MR/PR creation
- Cross-platform support for Linux and macOS
- Full test coverage: unit tests, integration tests, and end-to-end tests with mock Claude

---

## User Stories

---

### SECTION A: nflow-core — Foundation & Data Models

---

### US-001: Rust Workspace Skeleton
**Description:** As a developer, I want the six-crate Rust workspace scaffolded so that all crates compile and depend on each other correctly.

**Acceptance Criteria:**
- [ ] `Cargo.toml` workspace with members: `nflow-core`, `nflow-claude`, `nflow-git`, `nflow-daemon`, `nflow-cli`, `nflow-tui`
- [ ] Each crate has `Cargo.toml` with correct inter-crate dependencies
- [ ] `nflow-core` has zero dependencies on tokio, no async code, no IO (std::fs, std::net, etc.)
- [ ] All other crates depend on `nflow-core`
- [ ] `cargo build --workspace` succeeds
- [ ] `cargo clippy --workspace` passes
- [ ] `cargo fmt --check` passes

### US-002: Project Model and CRUD Logic
**Description:** As a developer, I want a project model in nflow-core so that projects can be created, listed, and deleted with proper validation.

**Acceptance Criteria:**
- [ ] `Project` struct with fields: id (UUID), name, path, base_branch, git_provider (github|gitlab), execution_enabled (bool), created_at, updated_at
- [ ] `ProjectService::create()` validates: name is non-empty, name is unique (given a name-existence checker), path is non-empty
- [ ] `ProjectService::create()` detects git provider from remote URL string (github.com → github, gitlab.com → gitlab) with manual override
- [ ] `ProjectService::create()` detects base branch from provided HEAD ref or defaults to "main"
- [ ] `ProjectService::delete()` returns error if project has running agents (given a running-count checker)
- [ ] All functions are pure — accept trait/closure for external checks, no IO
- [ ] Typecheck and format pass

### US-003: Spec Model and State Machine
**Description:** As a developer, I want a spec model with enforced state transitions so that specs progress through their lifecycle correctly.

**Acceptance Criteria:**
- [ ] `Spec` struct with fields: id (UUID), project_id, name, file_path, status (draft|approved|decomposed|deleted), session_active (bool), claude_session_id (Option), created_at, updated_at
- [ ] State machine enforces: draft → approved, approved → draft (reopen), approved → decomposed, draft|approved → deleted
- [ ] Invalid transitions return typed error (e.g., `Error::InvalidTransition { from, to }`)
- [ ] `reopen()` fails if status is decomposed
- [ ] `delete()` fails if status is decomposed (must discard wave first)
- [ ] `start_session()` fails if `session_active` is already true (one session per project enforced externally)
- [ ] `end_session()` sets `session_active = false`, stores `claude_session_id`
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-004: Work Item Model — Epics and Stories
**Description:** As a developer, I want epic and story models so that work can be organized hierarchically with story-level dependencies.

**Acceptance Criteria:**
- [ ] `WorkItem` struct with fields: id (UUID), parent_id (Option<UUID>), decomposition_session_id, item_type (epic|story|task), kind (Option: impl|verify), title, description, acceptance_criteria, status, short_id, sort_order, branch_name (Option), worktree_path (Option), mr_url (Option), commit_hash (Option), created_at, updated_at
- [ ] Epic status: pending|done|cancelled — derived/materialized from child stories
- [ ] Story state machine: pending → ready → in_progress → done|failed|cancelled
- [ ] Story transitions: ready requires all blockers done, in_progress requires ready|failed(retry), cancelled from pending|ready|in_progress
- [ ] `propagate_epic_status()` sets epic to done when all stories done, cancelled when all cancelled/done
- [ ] Dependency model: `Dependency { blocker_id: UUID, blocked_id: UUID }` — story-level only
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-005: Work Item Model — Tasks (Impl and Verify)
**Description:** As a developer, I want task models with impl/verify pairing so that each implementation step has automatic verification.

**Acceptance Criteria:**
- [ ] Task state machine: pending → in_progress → done|failed|cancelled
- [ ] Task transitions: failed → in_progress (retry), failed → done (skip with metadata), pending → cancelled
- [ ] `auto_generate_verify_tasks()` inserts a verify task after each impl task with `kind=verify`, `short_id="{id}v"`, `sort_order = impl.sort_order + 1`
- [ ] `skip_task()` on impl task also marks paired verify task as skipped
- [ ] `find_paired_verify()` finds verify task by matching `parent_id` and adjacent `sort_order`
- [ ] `find_paired_impl()` finds impl task for a given verify task
- [ ] `get_next_pending_task()` returns next task by `sort_order` within a story
- [ ] `commit_hash` stored on impl task completion
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-006: Short ID System
**Description:** As a developer, I want wave-prefixed short IDs so that users can reference work items concisely in CLI commands.

**Acceptance Criteria:**
- [ ] Generate short IDs: E1, E2 (epics), S1, S2 (stories), T1, T2 (tasks) — scoped per decomposition session
- [ ] Wave-prefixed display: W1-E1, W1-S1, W1-T1, W1-T1v
- [ ] Verify tasks: short_id = "{impl_short_id}v" (e.g., T1 → T1v)
- [ ] `resolve_short_id(wave_prefix, short_id) → Result<UUID>` resolves display IDs to internal UUIDs
- [ ] Error if short ID not found or ambiguous
- [ ] Parse user input: accepts both "W1-S1" and "S1" (when wave is unambiguous or provided by context)
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-007: DAG Construction and Validation
**Description:** As a developer, I want DAG construction from work items so that story dependencies are validated and circular references detected.

**Acceptance Criteria:**
- [ ] `build_dag(stories: &[WorkItem], dependencies: &[Dependency]) → Result<Dag>` constructs adjacency list
- [ ] Topological sort returns stories in valid execution order
- [ ] Circular dependency detection returns `Error::CyclicDependency { cycle: Vec<UUID> }`
- [ ] Validate all `depends_on` references exist within the same decomposition session
- [ ] Validate no cross-wave dependencies (all deps within same session)
- [ ] `find_ready_stories(dag, statuses) → Vec<UUID>` returns stories with all blockers in done|cancelled state
- [ ] `find_blocked_stories(dag, statuses) → Vec<UUID>` returns stories with at least one incomplete blocker
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-008: Scheduler Algorithm
**Description:** As a developer, I want a pure scheduling algorithm so that the daemon can pick the next stories to execute respecting parallelism and dependencies.

**Acceptance Criteria:**
- [ ] `schedule(state: &SchedulerState) → Vec<SchedulerAction>` returns actions to take (start story, start task, mark ready, etc.)
- [ ] `SchedulerState` contains: all work items, dependencies, running_count, max_parallel, execution_enabled
- [ ] Respects `max_parallel`: never returns more start actions than available slots
- [ ] Prioritizes stories by wave_number (lower first), then sort_order
- [ ] Propagates story readiness: pending → ready when all blockers done
- [ ] Propagates epic status from child stories
- [ ] Skips stories in waves that are not approved
- [ ] Skips projects with `execution_enabled = false`
- [ ] Returns no actions when `execution_enabled = false` globally
- [ ] Pure logic, no IO, no async
- [ ] Typecheck and format pass

### US-009: Decomposition Session Model
**Description:** As a developer, I want a decomposition session model so that waves are tracked with their specs and lifecycle.

**Acceptance Criteria:**
- [ ] `DecompositionSession` struct: id (UUID), project_id, wave_number (auto-incremented), status (in_progress|approved|discarded), claude_session_id (Option), created_at, updated_at
- [ ] `decomposition_specs` mapping: session_id ↔ spec_id (many-to-many)
- [ ] State machine: in_progress → approved, in_progress → discarded
- [ ] Only one `in_progress` session per project at a time
- [ ] `approve()` transitions status and makes stories schedulable
- [ ] `discard()` deletes associated work items and frees specs back to `approved` status
- [ ] Auto-increment `wave_number` per project (max existing + 1)
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-010: Config Model and Merging
**Description:** As a developer, I want a config model with layered merging so that defaults, global config, per-project overrides, and env vars are composed correctly.

**Acceptance Criteria:**
- [ ] `Config` struct with all keys: max_parallel (u32), git_provider (String), base_branch (String), cleanup_worktrees (bool), max_turns_per_task (u32), auto_execute (bool), max_time_per_task (u64 secs), log_level (String), branch_template (String), worktree_dir (String)
- [ ] Defaults: max_parallel=3, git_provider="github", base_branch="main", cleanup_worktrees=false, max_turns_per_task=50, auto_execute=false, max_time_per_task=1800, log_level="info"
- [ ] `merge(base: Config, overlay: PartialConfig) → Config` — overlay fields override base when present
- [ ] Merging order: defaults → global toml → per-project toml → env vars
- [ ] Env var mapping: NFLOW_HOME, NFLOW_SOCKET, NFLOW_LOG_LEVEL, NFLOW_MAX_PARALLEL
- [ ] Validation: max_parallel > 0, max_time_per_task > 0, git_provider in ["github", "gitlab"]
- [ ] `branch_template` supports variables: {project}, {story_id}, {story_slug}
- [ ] Pure logic (parsing from TOML/env is in daemon/cli)
- [ ] Typecheck and format pass

### US-011: Database Schema and Migration Runner
**Description:** As a developer, I want a SQLite schema and migration system so that all tables are created and versioned correctly.

**Acceptance Criteria:**
- [ ] `001_init.sql` creates tables: `projects`, `specs`, `work_items`, `dependencies`, `agent_runs`, `decomposition_sessions`, `decomposition_specs`, `schema_version`
- [ ] `work_items` table: unified for epics/stories/tasks with `item_type` discriminator and `kind` (impl|verify) for tasks
- [ ] All primary keys are TEXT (UUID strings)
- [ ] Foreign keys: `work_items.parent_id → work_items.id`, `work_items.decomposition_session_id → decomposition_sessions.id`, etc.
- [ ] ON DELETE CASCADE for project → specs → work_items chain
- [ ] Indexes on: `work_items(parent_id)`, `work_items(decomposition_session_id)`, `specs(project_id)`, `dependencies(blocker_id)`, `dependencies(blocked_id)`
- [ ] Migration runner (~50 LOC): reads numbered files, applies unapplied ones in transaction, tracks in `schema_version`
- [ ] Migration files embedded at compile time via `include_str!`
- [ ] Backup `nflow.db` to `nflow.db.bak-v{version}` before applying new migrations
- [ ] WAL mode enabled on connection open
- [ ] Typecheck and format pass

### US-012: Database CRUD — Projects
**Description:** As a developer, I want database CRUD for projects so that project records are persisted and queryable.

**Acceptance Criteria:**
- [ ] `insert_project(project: &Project) → Result<()>`
- [ ] `get_project_by_name(name: &str) → Result<Option<Project>>`
- [ ] `get_project_by_id(id: &Uuid) → Result<Option<Project>>`
- [ ] `list_projects() → Result<Vec<Project>>`
- [ ] `update_project(project: &Project) → Result<()>`
- [ ] `delete_project(id: &Uuid) → Result<()>` — cascades to specs, work_items, agent_runs
- [ ] `project_name_exists(name: &str) → Result<bool>`
- [ ] All queries use parameterized statements (no SQL injection)
- [ ] Typecheck and format pass

### US-013: Database CRUD — Specs
**Description:** As a developer, I want database CRUD for specs so that spec records and their sessions are persisted.

**Acceptance Criteria:**
- [ ] `insert_spec(spec: &Spec) → Result<()>`
- [ ] `get_spec_by_name(project_id: &Uuid, name: &str) → Result<Option<Spec>>`
- [ ] `list_specs_by_project(project_id: &Uuid) → Result<Vec<Spec>>`
- [ ] `list_specs_by_status(project_id: &Uuid, status: SpecStatus) → Result<Vec<Spec>>`
- [ ] `update_spec_status(id: &Uuid, status: SpecStatus) → Result<()>`
- [ ] `update_spec_session(id: &Uuid, session_active: bool, claude_session_id: Option<&str>) → Result<()>`
- [ ] `find_latest_draft_spec(project_id: &Uuid) → Result<Option<Spec>>` — for `spec resume` without name
- [ ] `find_unassigned_approved_specs(project_id: &Uuid) → Result<Vec<Spec>>` — approved specs not in any decomposition session
- [ ] `reset_active_sessions(project_id: &Uuid) → Result<u64>` — for crash recovery
- [ ] Typecheck and format pass

### US-014: Database CRUD — Work Items and Dependencies
**Description:** As a developer, I want database CRUD for work items and dependencies so that the full DAG is persisted.

**Acceptance Criteria:**
- [ ] `insert_work_item(item: &WorkItem) → Result<()>`
- [ ] `insert_dependency(dep: &Dependency) → Result<()>`
- [ ] `get_work_item_by_id(id: &Uuid) → Result<Option<WorkItem>>`
- [ ] `list_work_items_by_session(session_id: &Uuid) → Result<Vec<WorkItem>>`
- [ ] `list_work_items_by_parent(parent_id: &Uuid) → Result<Vec<WorkItem>>` — ordered by sort_order
- [ ] `list_dependencies_by_session(session_id: &Uuid) → Result<Vec<Dependency>>`
- [ ] `list_blockers_for_story(story_id: &Uuid) → Result<Vec<UUID>>`
- [ ] `update_work_item_status(id: &Uuid, status: WorkItemStatus) → Result<()>`
- [ ] `update_work_item_commit(id: &Uuid, commit_hash: &str) → Result<()>`
- [ ] `update_story_worktree(id: &Uuid, worktree_path: &str, branch_name: &str) → Result<()>`
- [ ] `update_story_mr(id: &Uuid, mr_url: &str) → Result<()>`
- [ ] `delete_work_items_by_session(session_id: &Uuid) → Result<u64>` — for plan feedback regeneration
- [ ] `count_tasks_by_status(story_id: &Uuid) → Result<HashMap<Status, u32>>` — for progress display
- [ ] Typecheck and format pass

### US-015: Database CRUD — Agent Runs and Decomposition Sessions
**Description:** As a developer, I want database CRUD for agent runs and decomposition sessions so that execution history and wave state are tracked.

**Acceptance Criteria:**
- [ ] `insert_agent_run(run: &AgentRun) → Result<()>` with fields: id, work_item_id, pid, session_id, pid_start_time, status, exit_code, log_path, error_message, started_at, finished_at
- [ ] `update_agent_run_status(id: &Uuid, status, exit_code, error_message, finished_at) → Result<()>`
- [ ] `find_running_agent_runs() → Result<Vec<AgentRun>>` — for crash recovery
- [ ] `count_agent_runs_for_task(work_item_id: &Uuid) → Result<u32>` — for retry warning
- [ ] `insert_decomposition_session(session: &DecompositionSession) → Result<()>`
- [ ] `insert_decomposition_spec(session_id: &Uuid, spec_id: &Uuid) → Result<()>`
- [ ] `get_decomposition_session(id: &Uuid) → Result<Option<DecompositionSession>>`
- [ ] `find_draft_session(project_id: &Uuid) → Result<Option<DecompositionSession>>`
- [ ] `list_sessions_by_project(project_id: &Uuid) → Result<Vec<DecompositionSession>>`
- [ ] `update_session_status(id: &Uuid, status: SessionStatus) → Result<()>`
- [ ] `next_wave_number(project_id: &Uuid) → Result<u32>` — max existing + 1
- [ ] Typecheck and format pass

### US-016: NflowError Type System
**Description:** As a developer, I want a typed error system so that all crates return structured errors with context.

**Acceptance Criteria:**
- [ ] `NflowError` enum in nflow-core with variants: `InvalidTransition { from, to }`, `CyclicDependency { cycle }`, `NotFound { entity, id }`, `AlreadyExists { entity, name }`, `InvalidState { message }`, `InvalidParams { message }`, `ValidationError { field, message }`
- [ ] Each variant carries enough context for meaningful error messages
- [ ] `impl Display for NflowError` produces human-readable messages
- [ ] `impl std::error::Error for NflowError`
- [ ] nflow-daemon, nflow-cli add IO/system error wrappers: `Io(std::io::Error)`, `Db(rusqlite::Error)`, `Git(String)`, `Claude(String)`, `Socket(String)`
- [ ] Typecheck and format pass

---

### SECTION B: nflow-claude — Claude Code Integration

---

### US-017: Claude Process Runner — Core Spawning
**Description:** As a developer, I want to spawn Claude CLI processes with correct arguments so that agents run with proper configuration.

**Acceptance Criteria:**
- [ ] `ClaudeRunner::spawn(config: &RunConfig) → Result<ClaudeProcess>` spawns `claude` binary
- [ ] `RunConfig` specifies: prompt (String), system_prompt_file (Option<PathBuf>), allowed_tools (Vec<String>), max_turns (u32), resume_session (Option<String>), working_dir (PathBuf), output_format ("stream-json"), verbose (true), include_partial_messages (true)
- [ ] Returns `ClaudeProcess` with: child process handle, PID, stdout reader, stderr reader
- [ ] Process spawned with stdout and stderr piped
- [ ] Handles `claude` binary not found in PATH (clear error message)
- [ ] Handles permission denied errors
- [ ] Cross-platform (Linux + macOS)
- [ ] Typecheck and format pass

### US-018: Claude Process Runner — Invocation Modes
**Description:** As a developer, I want preset invocation modes for spec/decompose/impl/verify so that each Claude interaction uses the correct tools and limits.

**Acceptance Criteria:**
- [ ] `RunConfig::for_spec(spec_name, system_prompt_path, working_dir, resume_session)` — no allowedTools restriction (Claude default), no max_turns
- [ ] `RunConfig::for_decompose(prompt, system_prompt_path, working_dir)` — no tool restriction, no max_turns
- [ ] `RunConfig::for_impl_task(prompt, system_prompt_path, working_dir)` — allowedTools: "Read,Write,Edit,Bash,Glob,Grep", max_turns: 50
- [ ] `RunConfig::for_verify_task(prompt, system_prompt_path, working_dir)` — allowedTools: "Read,Bash,Glob,Grep" (NO Write/Edit), max_turns: 30
- [ ] `--with-codebase` for spec/decompose changes working_dir to project root (otherwise specs dir)
- [ ] All modes include `--output-format stream-json --verbose --include-partial-messages`
- [ ] Typecheck and format pass

### US-019: Stream-JSON Parser
**Description:** As a developer, I want a stream-json parser so that Claude's output is parsed into structured events in real-time.

**Acceptance Criteria:**
- [ ] `StreamParser` reads lines from stdout, parses each as JSON
- [ ] Emits typed events: `TextDelta(String)`, `ToolUse { name, input }`, `ToolResult { content }`, `Result { text, session_id }`, `Error { message }`
- [ ] Handles partial lines (buffers until newline)
- [ ] Handles malformed JSON lines (emits `ParseError` event, does not crash)
- [ ] Extracts `session_id` from final result event
- [ ] Detects `AskUserQuestion` tool calls (for spec dialogue)
- [ ] Processes line-by-line without buffering entire output in memory
- [ ] Typecheck and format pass

### US-020: Stream-JSON — Success Criteria Evaluation
**Description:** As a developer, I want to evaluate task success from stream output so that impl and verify tasks are correctly marked as done or failed.

**Acceptance Criteria:**
- [ ] `evaluate_impl_result(exit_code, head_before, head_after, commit_message) → TaskResult`
- [ ] Impl success: exit_code == 0 AND head_after != head_before AND commit_message contains `[{short_id}]`
- [ ] Impl failure reasons: non-zero exit, no new commit, wrong commit message format
- [ ] `evaluate_verify_result(exit_code, result_text) → TaskResult`
- [ ] Verify success: exit_code == 0 AND result_text contains "VERIFICATION PASSED"
- [ ] Verify failure reasons: non-zero exit, result doesn't contain marker
- [ ] `TaskResult` enum: `Success`, `Failed { reason: String }`
- [ ] Pure logic, no IO
- [ ] Typecheck and format pass

### US-021: Session Tracker
**Description:** As a developer, I want session tracking so that spec dialogues and plan feedback can be resumed across Claude invocations.

**Acceptance Criteria:**
- [ ] `SessionTracker` stores mapping: entity_id → claude_session_id
- [ ] `start_session(entity_id) → session_id = None` (first invocation, no resume)
- [ ] `update_session(entity_id, session_id)` — called after first Claude result returns session_id
- [ ] `get_resume_id(entity_id) → Option<String>` — returns session_id for `--resume` flag
- [ ] Works for both specs (multi-turn Q&A) and decomposition sessions (feedback loop)
- [ ] Session IDs persisted via database (spec.claude_session_id, decomposition_session.claude_session_id)
- [ ] Typecheck and format pass

### US-022: Prompt Template Loader and Renderer
**Description:** As a developer, I want prompt template loading and rendering so that prompts are embedded at compile time but overridable at runtime.

**Acceptance Criteria:**
- [ ] Templates embedded via `include_str!`: `spec_session.md`, `decompose.md`, `task_execution.md`, `verify_task.md`, `mr_body.md`
- [ ] `load_template(name: &str, override_dir: Option<&Path>) → String` — checks override dir first, falls back to embedded
- [ ] `render_template(template: &str, vars: &HashMap<&str, &str>) → Result<String>` — replaces `{variable}` placeholders
- [ ] Returns error for unresolved `{variable}` placeholders (missing required variable)
- [ ] Ignores `{text}` that doesn't match any variable key (literal braces)
- [ ] Typecheck and format pass

### US-023: Prompt Context Generation
**Description:** As a developer, I want prompt context builders so that each Claude invocation gets the right context variables.

**Acceptance Criteria:**
- [ ] `build_task_context(task, story, completed_tasks, previous_error) → HashMap` for task_execution.md
- [ ] `build_verify_context(verify_task, impl_task, impl_result) → HashMap` for verify_task.md
- [ ] `build_mr_context(story, tasks, project) → HashMap` for mr_body.md
- [ ] `build_spec_context(spec) → HashMap` for spec_session.md
- [ ] `build_decompose_prompt(specs: &[Spec]) → String` — concatenates spec file contents into single prompt string
- [ ] `completed_tasks` formatted as list: "- [{short_id}] {title}: {commit_hash}"
- [ ] `previous_error` included only on retry (empty string on first attempt)
- [ ] `tasks_list` for MR body: "- {commit_hash} {title}" or "- [SKIPPED] {title}"
- [ ] Typecheck and format pass

### US-024: Claude Process Termination
**Description:** As a developer, I want graceful and forced process termination so that agents can be stopped cleanly.

**Acceptance Criteria:**
- [ ] `ClaudeProcess::terminate() → Result<()>` sends SIGTERM, waits 10 seconds, then SIGKILL if still alive
- [ ] `ClaudeProcess::kill() → Result<()>` sends SIGKILL immediately
- [ ] `ClaudeProcess::wait_with_timeout(duration) → Result<Option<ExitStatus>>` — returns None on timeout
- [ ] Cross-platform signal handling (Linux + macOS via `nix` crate or `libc`)
- [ ] Returns exit code and captures any final stderr output
- [ ] Typecheck and format pass

---

### SECTION C: nflow-git — Git Operations

---

### US-025: Worktree Creation
**Description:** As a developer, I want git worktree creation so that each story runs in its own isolated directory.

**Acceptance Criteria:**
- [ ] `create_worktree(repo_path, worktree_path, base_branch) → Result<()>`
- [ ] Runs: `git fetch origin {base_branch}` then `git worktree add {worktree_path} origin/{base_branch}`
- [ ] Validates worktree_path doesn't already exist (returns error if it does)
- [ ] Validates repo_path is a git repository
- [ ] Uses git2 crate with CLI fallback for operations git2 doesn't support well
- [ ] Typecheck and format pass

### US-026: Worktree Removal and Cleanup
**Description:** As a developer, I want worktree removal and bulk cleanup so that stale worktrees don't accumulate.

**Acceptance Criteria:**
- [ ] `remove_worktree(repo_path, worktree_path) → Result<()>` — runs `git worktree remove {path}`
- [ ] `list_worktrees(repo_path) → Result<Vec<WorktreeInfo>>` — parses `git worktree list --porcelain`
- [ ] `prune_worktrees(repo_path) → Result<()>` — runs `git worktree prune`
- [ ] `cleanup_all(repo_path, worktree_dir) → Result<Vec<String>>` — removes all nflow worktrees, returns removed paths
- [ ] Handles already-removed worktrees gracefully (not an error)
- [ ] Typecheck and format pass

### US-027: Branch Management
**Description:** As a developer, I want branch creation and naming so that each story gets a properly named branch.

**Acceptance Criteria:**
- [ ] `create_branch(worktree_path, branch_name) → Result<()>` — runs `git checkout -b {name}` in worktree
- [ ] `format_branch_name(template, vars) → String` — applies template: `nflow/{project}/{story_id}-{story_slug}`
- [ ] Slugify story title: lowercase, replace spaces/special chars with hyphens, truncate to 50 chars
- [ ] `get_current_branch(worktree_path) → Result<String>`
- [ ] `get_head_commit(worktree_path) → Result<String>` — returns HEAD commit hash
- [ ] `get_commit_message(worktree_path) → Result<String>` — returns latest commit message
- [ ] Typecheck and format pass

### US-028: Rebase and Push Operations
**Description:** As a developer, I want rebase and push so that story branches are up-to-date and delivered to the remote.

**Acceptance Criteria:**
- [ ] `fetch_and_rebase(worktree_path, base_branch) → Result<()>` — runs `git fetch origin {base} && git rebase origin/{base}`
- [ ] On rebase conflict: abort rebase (`git rebase --abort`), return `Error::RebaseConflict { details }`
- [ ] `push_branch(worktree_path, branch_name) → Result<()>` — runs `git push -u origin {branch_name}`
- [ ] Handle push failures: auth error, network error, branch protection — each with distinct error
- [ ] Typecheck and format pass

### US-029: Worktree Reset for Retry
**Description:** As a developer, I want worktree reset so that retried tasks start from a clean state.

**Acceptance Criteria:**
- [ ] `reset_to_commit(worktree_path, commit_hash) → Result<()>` — runs `git reset --hard {hash} && git clean -fd`
- [ ] Used when retrying an impl task: reset to the commit_hash of the previous successful task (or base if first task)
- [ ] `get_base_commit(worktree_path) → Result<String>` — returns the commit the worktree was created from
- [ ] Verify the commit_hash exists before reset
- [ ] Typecheck and format pass

### US-030: MR/PR Creation — GitHub
**Description:** As a developer, I want PR creation via `gh` so that completed stories automatically get pull requests on GitHub.

**Acceptance Criteria:**
- [ ] `create_github_pr(worktree_path, title, body, base_branch) → Result<String>` — returns PR URL
- [ ] Runs `gh pr create --title {title} --body {body} --base {base}` in worktree directory
- [ ] Validates `gh` is in PATH (clear error if not found)
- [ ] Handles auth errors (`gh auth status` check)
- [ ] Handles rate limit errors
- [ ] Parses PR URL from stdout
- [ ] Typecheck and format pass

### US-031: MR/PR Creation — GitLab
**Description:** As a developer, I want MR creation via `glab` so that completed stories automatically get merge requests on GitLab.

**Acceptance Criteria:**
- [ ] `create_gitlab_mr(worktree_path, title, body, base_branch) → Result<String>` — returns MR URL
- [ ] Runs `glab mr create --title {title} --description {body} --target-branch {base}` in worktree directory
- [ ] Validates `glab` is in PATH (clear error if not found)
- [ ] Handles auth errors
- [ ] Parses MR URL from stdout
- [ ] Typecheck and format pass

### US-032: Commit Validation
**Description:** As a developer, I want commit validation so that impl task success criteria are checked against git state.

**Acceptance Criteria:**
- [ ] `has_new_commit(worktree_path, previous_head) → Result<bool>` — compares current HEAD to previous
- [ ] `validate_commit_message(worktree_path, expected_prefix) → Result<bool>` — checks latest commit contains `[{prefix}]`
- [ ] `expected_prefix` is the wave-prefixed task short ID (e.g., "W1-T1")
- [ ] Returns false (not error) when validation fails — caller decides how to handle
- [ ] Typecheck and format pass

---

### SECTION D: nflow-daemon — Background Process

---

### US-033: Daemon Process Spawning and Daemonization
**Description:** As a developer, I want the daemon to spawn as a background process so that it runs independently of the terminal.

**Acceptance Criteria:**
- [ ] `nflow daemon start` spawns daemon via `Command::new("nflow-daemon").spawn()`
- [ ] After spawn, daemon calls `setsid()` via `nix` crate to detach from terminal
- [ ] Ignores SIGHUP before setsid, restores after
- [ ] Writes PID to `~/.nflow/daemon.pid`
- [ ] Redirects stdout/stderr to `~/.nflow/logs/daemon.log`
- [ ] Creates `~/.nflow/logs/` directory if it doesn't exist
- [ ] Works on both Linux and macOS
- [ ] Typecheck and format pass

### US-034: Daemon Foreground Mode
**Description:** As a developer, I want foreground mode so that I can debug the daemon with visible log output.

**Acceptance Criteria:**
- [ ] `nflow daemon start --foreground` runs daemon in the current terminal
- [ ] Logs output to stderr (not redirected to file)
- [ ] Still writes PID file and binds socket
- [ ] Ctrl+C triggers graceful shutdown (same as SIGTERM)
- [ ] Typecheck and format pass

### US-035: Daemon Auto-Start with Flock
**Description:** As a developer, I want the daemon to auto-start on first command so that I never manually start it.

**Acceptance Criteria:**
- [ ] CLI checks if daemon is running (PID file exists + process alive) before sending commands
- [ ] If not running, acquires `flock` on `~/.nflow/daemon.lock` (exclusive, non-blocking or with 5s timeout)
- [ ] If lock acquired, spawns daemon process, waits up to 5s for socket to appear
- [ ] If lock not acquired (another client is starting), waits for socket to appear
- [ ] Returns error if daemon doesn't start within timeout
- [ ] Typecheck and format pass

### US-036: Daemon Status and Stop
**Description:** As a developer, I want daemon status checking and clean stopping so that I can monitor and control the background process.

**Acceptance Criteria:**
- [ ] `nflow daemon status` reads PID file, checks if process is alive, reports status + PID + uptime
- [ ] Reports "not running" if PID file missing or process dead (cleans up stale PID file)
- [ ] `nflow daemon stop` sends SIGTERM to daemon PID
- [ ] Daemon handles SIGTERM: sets shutting_down flag, stops accepting new connections
- [ ] Returns confirmation message with PID
- [ ] Typecheck and format pass

### US-037: Crash Recovery — Stale Agent Detection
**Description:** As a developer, I want crash recovery so that stale agents are detected and cleaned up on daemon restart.

**Acceptance Criteria:**
- [ ] On startup, query `agent_runs WHERE status = 'running'`
- [ ] For each stale run: check if PID is alive using `kill(pid, 0)`
- [ ] If PID alive: compare `pid_start_time` with actual process start time
- [ ] Linux: read start time from `/proc/{pid}/stat` field 22
- [ ] macOS: read start time via `libproc::pid_rusage` or `sysctl kern.proc.pid`
- [ ] If PID dead OR start time mismatch: mark agent_run as failed ("daemon crashed"), update work_item status to failed
- [ ] If PID alive AND start time matches: adopt process (resume reading stdout)
- [ ] Typecheck and format pass

### US-038: Crash Recovery — Session and State Reset
**Description:** As a developer, I want session and state cleanup on crash recovery so that no stuck state remains after a crash.

**Acceptance Criteria:**
- [ ] Reset all specs with `session_active = 1` to `session_active = 0`
- [ ] Work items in `in_progress` state with no running agent_run → set to `failed`
- [ ] Stories in `in_progress` with all tasks done → trigger completion flow (rebase/push/MR)
- [ ] Remove stale socket file if it exists from previous run
- [ ] Remove stale PID file if PID doesn't match current process
- [ ] Log all recovery actions at INFO level
- [ ] Typecheck and format pass

### US-039: Graceful Shutdown
**Description:** As a developer, I want graceful shutdown so that running agents finish cleanly when the daemon stops.

**Acceptance Criteria:**
- [ ] On SIGTERM: set `shutting_down` flag
- [ ] Stop accepting new socket connections
- [ ] Stop scheduler (no new tasks started)
- [ ] Wait up to 60 seconds for running agents to finish naturally
- [ ] After 60s: send SIGTERM to remaining agent processes
- [ ] Wait 10 more seconds
- [ ] After 10s: send SIGKILL to any survivors
- [ ] Close database connection
- [ ] Remove socket file (`~/.nflow/nflow.sock`)
- [ ] Remove PID file (`~/.nflow/daemon.pid`)
- [ ] Exit with code 0
- [ ] Typecheck and format pass

### US-040: Unix Socket Server
**Description:** As a developer, I want a Unix socket server so that CLI and TUI clients can connect and send commands.

**Acceptance Criteria:**
- [ ] Bind Unix domain socket at `~/.nflow/nflow.sock`
- [ ] Set socket file mode to 0600 (owner-only)
- [ ] Accept concurrent client connections (tokio task per client)
- [ ] Read NDJSON from each client (split on `\n`)
- [ ] Parse JSON: `{"id": "uuid", "command": "...", "params": {...}}`
- [ ] Route to command handler, await response
- [ ] Send JSON response: `{"id": "uuid", "status": "ok|error", "data": {...}}`
- [ ] Detect and clean up disconnected clients
- [ ] Typecheck and format pass

### US-041: Streaming Responses and Event Broadcast
**Description:** As a developer, I want streaming responses so that log following and real-time TUI updates work.

**Acceptance Criteria:**
- [ ] Streaming response: multiple JSON lines with same `id`, final line has `"done": true`
- [ ] `subscribe_to_events(client_id)` registers a client for broadcast events
- [ ] Events broadcast to all subscribed clients: `StatusChange { item_id, old_status, new_status }`, `AgentOutput { task_id, line }`, `StoryCompleted { story_id, mr_url }`
- [ ] Client disconnect removes subscription
- [ ] `nflow log -f {task_id}` streams agent output in real-time via streaming response
- [ ] Typecheck and format pass

### US-042: NDJSON Protocol Handshake
**Description:** As a developer, I want a protocol handshake so that version mismatches between CLI and daemon are detected early.

**Acceptance Criteria:**
- [ ] Client sends first message: `{"protocol_version": 1}`
- [ ] Daemon responds: `{"protocol_version": 1, "status": "ok"}` or `{"status": "error", "message": "unsupported protocol version"}`
- [ ] Handshake must complete before any commands are accepted
- [ ] Handshake timeout: 5 seconds
- [ ] Typecheck and format pass

### US-043: Command Handler — Project Init
**Description:** As a developer, I want project init handling so that `nflow init` creates and persists a new project.

**Acceptance Criteria:**
- [ ] Receives: `{ name, path, base_branch?, git_provider? }`
- [ ] Validates using `ProjectService::create()` from nflow-core
- [ ] Creates directories: `~/.nflow/projects/{name}/specs/`, `~/.nflow/projects/{name}/agent-logs/`
- [ ] Inserts project record into SQLite
- [ ] Returns: `{ project_id, name, path, base_branch, git_provider }`
- [ ] Error if name already exists: `ALREADY_EXISTS`
- [ ] Error if path is not a git repo: `INVALID_PARAMS`
- [ ] Typecheck and format pass

### US-044: Command Handler — Project List and Delete
**Description:** As a developer, I want project list and delete handling so that projects can be managed.

**Acceptance Criteria:**
- [ ] `project.list` returns all projects with: name, path, base_branch, git_provider, execution_enabled, spec_count, story_count
- [ ] `project.delete` receives: `{ name, force? }`
- [ ] Without `--force`: returns confirmation prompt (client shows it)
- [ ] With `--force` or confirmation: deletes project record (cascades), removes `~/.nflow/projects/{name}/` directory
- [ ] Error if project has running agents: `INVALID_STATE`
- [ ] Typecheck and format pass

### US-045: Command Handler — Spec New (Start Session)
**Description:** As a developer, I want spec creation handling so that `nflow spec new` starts a Claude dialogue session.

**Acceptance Criteria:**
- [ ] Receives: `{ project_name, spec_name, with_codebase? }`
- [ ] Validates no active spec session for this project
- [ ] Creates spec record with status=draft, session_active=true
- [ ] Loads `spec_session.md` template
- [ ] Spawns Claude process with `ClaudeRunner::spawn(RunConfig::for_spec(...))`
- [ ] Sets working_dir to project root (with_codebase=true) or specs dir (false)
- [ ] Begins streaming Claude output to connected clients
- [ ] Returns: `{ spec_id, name, status: "draft" }`
- [ ] Typecheck and format pass

### US-046: Command Handler — Spec Q&A Flow
**Description:** As a developer, I want spec Q&A handling so that the multi-turn Claude dialogue works via resume chains.

**Acceptance Criteria:**
- [ ] Detects `AskUserQuestion` tool call in Claude stream output
- [ ] Sends question to client as streaming event: `{ type: "question", text, options? }`
- [ ] `spec.answer` receives: `{ spec_id, answer_text }`
- [ ] Spawns new Claude process with `--resume {session_id}` and `-p "{answer_text}"`
- [ ] Chains session: each answer starts a new Claude invocation resuming the previous
- [ ] On Claude exit code 0 + spec file exists: marks session as completed
- [ ] Sets `session_active = false`, stores final `claude_session_id`
- [ ] Typecheck and format pass

### US-047: Command Handler — Spec CRUD (list/view/approve/reopen/delete)
**Description:** As a developer, I want spec CRUD handling so that all spec lifecycle commands work.

**Acceptance Criteria:**
- [ ] `spec.list` returns specs for current project with: name, status, session_active, created_at
- [ ] `spec.view` returns spec markdown content (reads file from disk)
- [ ] `spec.approve` transitions draft → approved (validates via state machine)
- [ ] `spec.reopen` transitions approved → draft (validates not decomposed)
- [ ] `spec.delete` validates spec not in decomposed state, deletes record and file
- [ ] `spec.resume` finds latest draft spec (or by name), validates session not active, starts new Claude process with `--resume`
- [ ] All commands return appropriate error codes for invalid states
- [ ] Typecheck and format pass

### US-048: Command Handler — Plan Generate
**Description:** As a developer, I want plan generation handling so that `nflow plan generate` invokes Claude decomposition.

**Acceptance Criteria:**
- [ ] Receives: `{ project_name, spec_names?, with_codebase? }`
- [ ] Validates no draft wave exists for this project
- [ ] Finds unassigned approved specs (or specified ones)
- [ ] Creates `DecompositionSession` with auto-incremented wave_number
- [ ] Concatenates spec contents into single prompt via `build_decompose_prompt()`
- [ ] Spawns Claude with `RunConfig::for_decompose()` and `decompose.md` system prompt
- [ ] Parses Claude JSON output into work items (epics → stories → tasks)
- [ ] Auto-generates verify tasks after each impl task
- [ ] Builds and validates DAG (cycle check)
- [ ] Inserts all work items and dependencies into SQLite
- [ ] Marks specs as decomposed
- [ ] Returns: `{ wave_number, epic_count, story_count, task_count }`
- [ ] Typecheck and format pass

### US-049: Command Handler — Plan Show
**Description:** As a developer, I want plan display so that the current wave's structure is visible.

**Acceptance Criteria:**
- [ ] `plan.show` returns hierarchical tree: wave → epics → stories → tasks
- [ ] Each item includes: short_id, title, status, depends_on (for stories)
- [ ] `--wave N` selects specific wave (default: latest)
- [ ] `--dag` returns adjacency list representation of story dependencies
- [ ] Tasks include kind (impl|verify) for display differentiation
- [ ] Progress counts per story: `{done}/{total}` tasks
- [ ] Typecheck and format pass

### US-050: Command Handler — Plan Feedback
**Description:** As a developer, I want plan feedback handling so that the decomposition can be refined through Claude dialogue.

**Acceptance Criteria:**
- [ ] Receives: `{ message, wave_number? }`
- [ ] Validates wave is in draft (in_progress) state
- [ ] Deletes existing work items for this session (plan feedback = full regeneration)
- [ ] Spawns Claude with `--resume {session_id}` and feedback message as prompt
- [ ] Parses new JSON output, validates DAG, inserts new work items
- [ ] Auto-generates verify tasks for new plan
- [ ] Returns updated plan structure
- [ ] Typecheck and format pass

### US-051: Command Handler — Plan Approve and Discard
**Description:** As a developer, I want plan approval and discard so that waves are locked for execution or removed.

**Acceptance Criteria:**
- [ ] `plan.approve` validates wave is in draft state, transitions to approved
- [ ] Stories in approved wave become schedulable (status stays pending until dependencies resolve)
- [ ] If `auto_execute` is true: sets `execution_enabled = true` for the project
- [ ] `plan.discard` validates wave is in draft state, transitions to discarded
- [ ] Discard deletes all work items for the session
- [ ] Discard transitions associated specs back from decomposed → approved
- [ ] Returns confirmation with wave_number
- [ ] Typecheck and format pass

### US-052: Scheduler Loop — Main Integration
**Description:** As a developer, I want the scheduler running on the main event loop so that it ticks every 2 seconds without race conditions.

**Acceptance Criteria:**
- [ ] Scheduler runs as a `tokio::time::interval(Duration::from_secs(2))` on the main task
- [ ] Each tick: calls `schedule()` from nflow-core, then executes returned actions
- [ ] Serialized with command handling (no concurrent DB writes)
- [ ] Skips tick if `shutting_down` flag is set
- [ ] Skips tick if no projects have `execution_enabled = true`
- [ ] Logs scheduler actions at DEBUG level
- [ ] Typecheck and format pass

### US-053: Scheduler — Reap Finished Processes
**Description:** As a developer, I want process reaping so that finished agents are detected and their results processed.

**Acceptance Criteria:**
- [ ] On each tick: check all running `agent_runs` PIDs via `waitpid(WNOHANG)` or tokio process handle
- [ ] For finished processes: read exit code, update `agent_run` record
- [ ] Parse final stream-json output for session_id and result text
- [ ] Evaluate task result using `evaluate_impl_result()` or `evaluate_verify_result()`
- [ ] Update work_item status based on evaluation (done or failed with reason)
- [ ] Store commit_hash on successful impl task
- [ ] Broadcast status change events to subscribed clients
- [ ] Typecheck and format pass

### US-054: Scheduler — Start Story Execution
**Description:** As a developer, I want story start logic so that ready stories get a worktree and begin their first task.

**Acceptance Criteria:**
- [ ] When scheduler selects a ready story: set status to in_progress
- [ ] Format branch name from `branch_template`
- [ ] Create git worktree via `nflow-git::create_worktree()`
- [ ] Store worktree_path and branch_name on story
- [ ] Start first task (lowest sort_order with status=pending)
- [ ] If worktree creation fails: set story to failed with error details
- [ ] Typecheck and format pass

### US-055: Scheduler — Start Task Execution
**Description:** As a developer, I want task start logic so that Claude agents are spawned for each task.

**Acceptance Criteria:**
- [ ] Set task status to in_progress
- [ ] Record `head_before` commit hash (for impl tasks)
- [ ] Load and render prompt template (task_execution.md or verify_task.md)
- [ ] Write rendered context to temp file for `--append-system-prompt-file`
- [ ] Spawn Claude process via `ClaudeRunner::spawn()`
- [ ] Create `agent_run` record with: PID, pid_start_time, log_path, status=running
- [ ] Pipe agent stdout to log file at `~/.nflow/projects/{name}/agent-logs/{short_id}.log`
- [ ] Stream agent output to subscribed clients
- [ ] Typecheck and format pass

### US-056: Scheduler — Task Completion and Story Progression
**Description:** As a developer, I want task completion logic so that stories progress through their task sequence correctly.

**Acceptance Criteria:**
- [ ] On impl task success: mark done, store commit_hash, start paired verify task
- [ ] On verify task success: mark done, start next impl task (next pending by sort_order)
- [ ] On verify task failure: mark failed, set story to failed (stop immediately)
- [ ] On impl task failure: mark failed, set story to failed
- [ ] When all tasks in story are done: trigger story completion flow
- [ ] When story has mix of done and skipped tasks: still trigger completion (skipped = done)
- [ ] Typecheck and format pass

### US-057: Scheduler — Story Completion (Rebase, Push, MR)
**Description:** As a developer, I want story completion so that finished stories are rebased, pushed, and get MRs created.

**Acceptance Criteria:**
- [ ] When all tasks done: run `fetch_and_rebase()` from nflow-git
- [ ] On rebase success: run `push_branch()`
- [ ] On push success: render MR body from `mr_body.md` template with completed tasks
- [ ] Create MR/PR via `create_github_pr()` or `create_gitlab_mr()` based on project config
- [ ] Store `mr_url` on story, set status to done
- [ ] If `cleanup_worktrees=true`: remove worktree after MR creation
- [ ] On rebase/push/MR failure: set story to failed, all tasks stay done
- [ ] Broadcast `StoryCompleted` event
- [ ] Typecheck and format pass

### US-058: Scheduler — Wall-Clock Timeout
**Description:** As a developer, I want wall-clock timeout so that hung agents are killed after a configurable time.

**Acceptance Criteria:**
- [ ] Track task start time in `agent_runs.started_at`
- [ ] On each scheduler tick: compare `now - started_at` against `max_time_per_task`
- [ ] On timeout: send SIGTERM to agent process
- [ ] Wait 10 seconds, then SIGKILL if still alive
- [ ] Mark task as failed with reason "wall-clock timeout exceeded ({max_time}s)"
- [ ] Mark story as failed
- [ ] Log timeout at WARN level
- [ ] Typecheck and format pass

### US-059: Command Handler — Exec Run and Pause
**Description:** As a developer, I want run/pause commands so that execution can be enabled and disabled.

**Acceptance Criteria:**
- [ ] `exec.run` sets `execution_enabled = true` for current project
- [ ] `--parallel N` temporarily overrides `max_parallel` for this run (stored in memory, not config)
- [ ] `--story {id}` limits execution to a specific story (sets only that story to ready, others stay pending)
- [ ] `exec.pause` sets `execution_enabled = false` — running agents continue, no new ones started
- [ ] Returns current execution state: enabled/disabled, running_count, pending_count
- [ ] Typecheck and format pass

### US-060: Command Handler — Exec Status
**Description:** As a developer, I want status display so that I can see the current state of all work items.

**Acceptance Criteria:**
- [ ] Returns hierarchical status for current project: waves → epics → stories → tasks
- [ ] Each item: short_id, title, status, started_at, finished_at
- [ ] Stories additionally: branch_name, mr_url, worktree_path
- [ ] Tasks additionally: kind (impl|verify), agent exit_code, retry_count
- [ ] `--wave N` filters to specific wave
- [ ] Summary counts: total/pending/ready/running/done/failed/cancelled per wave
- [ ] Typecheck and format pass

### US-061: Command Handler — Exec Retry
**Description:** As a developer, I want retry handling so that failed tasks can be re-executed.

**Acceptance Criteria:**
- [ ] Receives: `{ task_id }` (wave-prefixed short ID)
- [ ] Validates task is in failed state
- [ ] For impl tasks: reset worktree to previous task's commit_hash (or base commit if first task)
- [ ] For verify tasks: no worktree reset needed (re-verify same state)
- [ ] Set task status back to in_progress, spawn new Claude agent
- [ ] Include `previous_error` in prompt context (from last agent_run's error_message)
- [ ] Count retries via `count_agent_runs_for_task()`, warn at 3+: "Task {id} has been retried {n} times"
- [ ] Set story status back to in_progress
- [ ] Returns: `{ task_id, retry_count, warning? }`
- [ ] Typecheck and format pass

### US-062: Command Handler — Exec Skip
**Description:** As a developer, I want skip handling so that failed tasks can be bypassed.

**Acceptance Criteria:**
- [ ] Receives: `{ task_id }`
- [ ] Validates task is in failed state
- [ ] Marks task as done with metadata `skipped = true`
- [ ] If impl task: also marks paired verify task as skipped
- [ ] Resumes story execution from next pending task
- [ ] If no more pending tasks: trigger story completion
- [ ] Returns: `{ task_id, skipped_verify?: task_id }`
- [ ] Typecheck and format pass

### US-063: Command Handler — Exec Continue
**Description:** As a developer, I want continue handling so that failed stories can resume from the next task.

**Acceptance Criteria:**
- [ ] Receives: `{ story_id, force? }`
- [ ] Validates story is in failed or cancelled state
- [ ] Marks current failed task as done (with `skipped = true`)
- [ ] If cancelled story has worktree: set story to in_progress, resume from first pending task
- [ ] If cancelled story has no worktree: set story to ready or pending (based on dependency state)
- [ ] `--force` skips confirmation for cancelled stories with dependents
- [ ] Returns: `{ story_id, next_task_id?, resumed: bool }`
- [ ] Typecheck and format pass

### US-064: Command Handler — Exec Stop and Cancel
**Description:** As a developer, I want stop/cancel so that running and pending stories can be halted.

**Acceptance Criteria:**
- [ ] `exec.stop` receives: `{ story_id?, wave?, all? }`
- [ ] Single story: sends SIGTERM to running agent, sets story status to cancelled
- [ ] `--wave N --all`: stops all running stories in wave
- [ ] Without args: stops all running stories across all waves
- [ ] Cancelled stories: running task gets terminated, pending tasks stay pending
- [ ] `exec.cancel` receives: `{ story_id }`
- [ ] Validates story is pending or ready (not yet started)
- [ ] Sets story to cancelled without any process termination
- [ ] Warns if cancelled story has dependents that will be blocked
- [ ] Typecheck and format pass

### US-065: Command Handler — Log Viewing
**Description:** As a developer, I want log viewing so that agent output can be inspected.

**Acceptance Criteria:**
- [ ] `exec.log` receives: `{ task_id, follow? }`
- [ ] Without follow: reads log file and returns content
- [ ] With follow: streams new log content as it's written (via daemon file watching or agent stream forwarding)
- [ ] Returns parsed output: tool calls with timestamps, text output, errors
- [ ] Error if no log file exists for the task
- [ ] Typecheck and format pass

### US-066: Command Handler — Worktree and Cleanup
**Description:** As a developer, I want worktree and cleanup commands so that disk space can be managed.

**Acceptance Criteria:**
- [ ] `worktree.list` returns all active worktrees: path, branch, story_id, story_status
- [ ] `worktree.clean` removes worktrees for done/cancelled stories
- [ ] `worktree.clean --all` removes all nflow worktrees (including running — with warning)
- [ ] `cleanup.logs` receives: `{ older_than?, all?, dry_run? }`
- [ ] Removes log files older than specified duration (default: all)
- [ ] `--dry-run` lists files that would be removed without removing them
- [ ] Returns: list of removed (or would-remove) files with sizes
- [ ] Typecheck and format pass

### US-067: Command Handler — Config Get and Set
**Description:** As a developer, I want config commands so that configuration can be viewed and modified at runtime.

**Acceptance Criteria:**
- [ ] `config.show` returns effective config: merged defaults + global + per-project + env
- [ ] Shows which layer each value comes from (default, global, project, env)
- [ ] `config.set` receives: `{ key, value, project? }`
- [ ] Without `--project`: writes to global `~/.nflow/config.toml`
- [ ] With `--project`: writes to per-project section in config
- [ ] Validates key is a known config key (warn on unknown)
- [ ] Validates value type matches expected (e.g., max_parallel must be positive integer)
- [ ] Returns updated effective value
- [ ] Typecheck and format pass

### US-068: Filesystem Security Enforcement
**Description:** As a developer, I want filesystem permission enforcement so that nflow data is protected from other users.

**Acceptance Criteria:**
- [ ] On daemon start: ensure `~/.nflow/` exists with mode 0700
- [ ] If directory mode is too open: fix permissions and log warning
- [ ] Socket file created with mode 0600
- [ ] PID file, lock file created with default umask (inherited 0700 from directory)
- [ ] Database file permissions inherited from directory
- [ ] Works on both Linux and macOS
- [ ] Typecheck and format pass

---

### SECTION E: nflow-cli — Command-Line Client

---

### US-069: CLI Skeleton with Clap
**Description:** As a developer, I want the CLI structure defined so that all commands and flags are parseable.

**Acceptance Criteria:**
- [ ] All commands defined via `clap::Parser` derive macro
- [ ] Command groups: `daemon`, `init`, `projects`, `project`, `spec`, `plan`, `run`, `pause`, `status`, `log`, `retry`, `skip`, `continue`, `stop`, `cancel`, `worktree`, `cleanup`, `config`, `tui`
- [ ] Global flags: `--project <name>`, `--verbose`, `--json`, `--no-color`
- [ ] Each subcommand has correct flags: e.g., `spec new <name> [--with-codebase]`, `plan generate [--specs <names>] [--with-codebase]`
- [ ] Help text for all commands and flags
- [ ] `cargo build -p nflow-cli` succeeds
- [ ] Typecheck and format pass

### US-070: CLI Socket Client
**Description:** As a developer, I want a socket client so that CLI commands are sent to the daemon and responses displayed.

**Acceptance Criteria:**
- [ ] Connect to Unix socket at `~/.nflow/nflow.sock` (or `NFLOW_SOCKET` override)
- [ ] Perform protocol handshake (send protocol_version, verify response)
- [ ] Send NDJSON request with generated UUID
- [ ] Wait for NDJSON response with matching UUID
- [ ] Connection timeout: 5 seconds
- [ ] Command timeout: configurable (default: 60 seconds for most, 300 for spec/plan operations)
- [ ] Clear error if socket doesn't exist: "Daemon not running. Starting..."
- [ ] Typecheck and format pass

### US-071: CLI Streaming Client
**Description:** As a developer, I want streaming support so that `log -f` and spec dialogues work in the terminal.

**Acceptance Criteria:**
- [ ] Handle streaming responses: read lines until `"done": true`
- [ ] For `log -f`: print each line as received, handle Ctrl+C to stop following
- [ ] For spec dialogue: display Claude output, detect question events, prompt user for input, send answer back
- [ ] User input via stdin (line-oriented)
- [ ] Graceful exit on Ctrl+C (send cancel to daemon)
- [ ] Typecheck and format pass

### US-072: CLI Output Formatting
**Description:** As a developer, I want formatted output so that CLI results are readable and scriptable.

**Acceptance Criteria:**
- [ ] Default: pretty-printed human-readable output with colors
- [ ] `--json`: raw JSON output (one object per line for lists)
- [ ] `--no-color`: strip ANSI color codes (for piping)
- [ ] Status colors: green (done), yellow (running/ready), red (failed), gray (pending), dim (cancelled)
- [ ] Tree rendering for `plan show`: indented ASCII tree with status icons
- [ ] Progress bars/indicators for `status` command
- [ ] Error messages include suggested actions (e.g., "Run `nflow retry W1-T3` to retry the failed task")
- [ ] Typecheck and format pass

### US-073: CLI Exit Codes
**Description:** As a developer, I want consistent exit codes so that scripts can check command results.

**Acceptance Criteria:**
- [ ] 0: success
- [ ] 1: general error (command failed)
- [ ] 2: daemon not running and auto-start failed
- [ ] 3: entity not found (project, spec, task, etc.)
- [ ] 4: invalid arguments or parameters
- [ ] 5: invalid state transition (e.g., approve already-approved spec)
- [ ] All exit codes documented in `--help`
- [ ] Typecheck and format pass

---

### SECTION F: nflow-tui — Terminal UI

---

### US-074: TUI App Skeleton
**Description:** As a developer, I want the TUI framework set up so that views can be rendered and input handled.

**Acceptance Criteria:**
- [ ] `nflow tui [--project <name>]` launches full-screen TUI via ratatui + crossterm
- [ ] Connects to daemon via Unix socket on startup
- [ ] If daemon not running: shows "Starting daemon..." and auto-starts
- [ ] If `--project` not specified: detects from cwd or shows project selector
- [ ] Main loop: handle input events → update state → render frame
- [ ] 60 FPS render target (or event-driven rendering)
- [ ] Clean terminal restore on exit (raw mode cleanup, cursor visible)
- [ ] Typecheck and format pass

### US-075: TUI Global Navigation
**Description:** As a developer, I want global keyboard navigation so that views are accessible from anywhere.

**Acceptance Criteria:**
- [ ] `1`/`2`/`3`/`4` switches to Specs/Plan/Execute/Logs views
- [ ] `Tab`/`Shift+Tab` cycles through views
- [ ] `q` quits TUI (daemon continues running)
- [ ] `?` toggles help overlay showing all keybindings for current view
- [ ] `p` opens project switcher popup
- [ ] `/` opens filter/search input
- [ ] Status bar at bottom: daemon state (connected/disconnected), project name, current wave, active agent count
- [ ] Header with view tabs, active tab highlighted
- [ ] Typecheck and format pass

### US-076: TUI — Specs List View
**Description:** As a developer, I want a specs list so that I can see and manage all specifications.

**Acceptance Criteria:**
- [ ] Lists all specs for current project with columns: name, status, updated
- [ ] Status shown with colored badge: draft (yellow), approved (green), decomposed (blue)
- [ ] Arrow keys to navigate, highlighted selection
- [ ] `n` opens new spec dialog (prompts for name and --with-codebase checkbox)
- [ ] `a` approves selected spec (if draft)
- [ ] `d` deletes selected spec (with confirmation popup)
- [ ] `v` opens spec content in pager sub-view
- [ ] `r` resumes selected spec session (if draft with session)
- [ ] Typecheck and format pass

### US-077: TUI — Spec Dialogue Sub-View
**Description:** As a developer, I want a dialogue interface so that I can have Claude conversations for spec creation in the TUI.

**Acceptance Criteria:**
- [ ] Shown when a spec session is active (after `n` or `r`)
- [ ] Split layout: scrollable chat history (top 80%) + input field (bottom 20%)
- [ ] Claude messages displayed with "Claude:" prefix, user messages with "You:" prefix
- [ ] When Claude asks a question: input field becomes active, cursor blinks
- [ ] Enter sends answer via `spec.answer` command
- [ ] Ctrl+D signals session end
- [ ] Esc returns to specs list (session continues in background)
- [ ] Auto-scroll to latest message
- [ ] Typecheck and format pass

### US-078: TUI — Spec Content Pager
**Description:** As a developer, I want a pager view so that I can read full spec content in the TUI.

**Acceptance Criteria:**
- [ ] Full-screen markdown content display (plain text, no rendering)
- [ ] Scrollable: `j`/`k` (line), `Ctrl+d`/`Ctrl+u` (page), `G` (bottom), `g` (top)
- [ ] Line wrapping at terminal width
- [ ] `q` or `Esc` returns to specs list
- [ ] Shows spec name in header
- [ ] Typecheck and format pass

### US-079: TUI — Plan Tree View
**Description:** As a developer, I want a tree visualization so that I can see the full decomposition structure.

**Acceptance Criteria:**
- [ ] Collapsible tree: Wave → Epic → Story → Task
- [ ] Each node shows: short_id, title, status icon
- [ ] Status icons: ○ (pending), ◉ (ready), ▶ (running), ✓ (done), ✗ (failed), ⊘ (cancelled)
- [ ] Stories show dependency indicators (e.g., "← W1-S1, W1-S2" for blockers)
- [ ] Progress per story: "(3/5 tasks)"
- [ ] Arrow keys to navigate tree, Enter to expand/collapse
- [ ] `h` toggles verify task visibility
- [ ] Typecheck and format pass

### US-080: TUI — Plan Actions
**Description:** As a developer, I want plan action keybindings so that I can generate, refine, and approve plans from the TUI.

**Acceptance Criteria:**
- [ ] `g` opens generate dialog (select specs, --with-codebase checkbox)
- [ ] `f` opens feedback input (multi-line text input, Enter to submit, Esc to cancel)
- [ ] `a` approves current draft wave (with confirmation popup)
- [ ] `d` discards current draft wave (with confirmation popup)
- [ ] Enter on selected item shows detail popup: full description, acceptance_criteria, dependencies
- [ ] Actions disabled with message when no draft wave exists (for g/f/a/d)
- [ ] Typecheck and format pass

### US-081: TUI — Execute Split View
**Description:** As a developer, I want a split execution view so that I can see the work tree and agent output simultaneously.

**Acceptance Criteria:**
- [ ] Vertical split: task tree (left 40%) + agent output (right 60%)
- [ ] Left pane: wave → story → task tree with status icons (same as plan view but flattened)
- [ ] Right pane: streaming output of currently selected task
- [ ] Selecting a task (arrow keys + Enter in left pane) switches right pane to that task's output
- [ ] Running tasks show live output; done/failed tasks show historical log
- [ ] Auto-select the currently running task on view entry
- [ ] Typecheck and format pass

### US-082: TUI — Execute Real-Time Updates
**Description:** As a developer, I want real-time status updates so that the execution view reflects current state without manual refresh.

**Acceptance Criteria:**
- [ ] Subscribe to daemon events on view entry
- [ ] `StatusChange` events update tree node status icons and colors
- [ ] `AgentOutput` events append to right pane (if matching selected task)
- [ ] `StoryCompleted` events show MR URL inline
- [ ] Running counts updated in view header: "Running: 2/3 | Done: 5 | Failed: 1"
- [ ] Status propagation visible: story pending → ready → running transitions
- [ ] Typecheck and format pass

### US-083: TUI — Execute Actions
**Description:** As a developer, I want execution control actions so that I can manage running work from the TUI.

**Acceptance Criteria:**
- [ ] `r` starts execution (equivalent to `nflow run`)
- [ ] `s` stops selected story (with confirmation)
- [ ] `c` cancels selected story (with confirmation, only if pending/ready)
- [ ] `Enter` opens selected task's full log in Logs view
- [ ] `e` escalates: stops story, displays worktree path for manual intervention
- [ ] Actions only available for items in appropriate states (gray out otherwise)
- [ ] Typecheck and format pass

### US-084: TUI — Logs View
**Description:** As a developer, I want a full-screen log viewer so that I can inspect agent output in detail.

**Acceptance Criteria:**
- [ ] Full-screen scrollable text display
- [ ] Shows content of selected task's log file
- [ ] Navigation: `j`/`k` (line), `G` (bottom), `g` (top), `Ctrl+d`/`Ctrl+u` (page)
- [ ] If task is running: streams new output in real-time, auto-scroll to bottom
- [ ] If task is done/failed: static display of historical log
- [ ] Tool calls parsed and displayed with timestamps: `[14:23:05] Tool: Read file.rs`
- [ ] `Esc` returns to Execute view
- [ ] Typecheck and format pass

### US-085: TUI — Help Overlay
**Description:** As a developer, I want a help overlay so that I can discover keybindings for the current view.

**Acceptance Criteria:**
- [ ] `?` toggles floating help panel
- [ ] Shows keybindings specific to current active view
- [ ] Also shows global keybindings (q, ?, p, 1-4, Tab)
- [ ] Semi-transparent background (dim underlying content)
- [ ] Any key press dismisses the overlay
- [ ] Typecheck and format pass

### US-086: TUI — Project Switcher
**Description:** As a developer, I want a project switcher so that I can change projects without restarting the TUI.

**Acceptance Criteria:**
- [ ] `p` opens project selector popup
- [ ] Lists all projects with: name, path, active agent count
- [ ] Arrow keys to navigate, Enter to select
- [ ] Esc to cancel (stay on current project)
- [ ] On selection: reload all views with new project's data
- [ ] Current project highlighted in list
- [ ] Typecheck and format pass

### US-087: TUI — Filter and Search
**Description:** As a developer, I want filtering so that I can find specific items in large plans/execution views.

**Acceptance Criteria:**
- [ ] `/` opens search input at bottom of screen
- [ ] Filters tree items by title text match (case-insensitive)
- [ ] Non-matching items hidden (or dimmed)
- [ ] Enter confirms filter, Esc clears filter
- [ ] Filter applies to current view only (Specs, Plan, or Execute tree)
- [ ] Shows match count: "Showing 3/15 items"
- [ ] Typecheck and format pass

---

### SECTION G: Cross-Platform Support

---

### US-088: Platform Abstraction — PID Detection
**Description:** As a developer, I want platform-abstracted PID detection so that crash recovery works on both Linux and macOS.

**Acceptance Criteria:**
- [ ] `get_pid_start_time(pid: u32) → Result<u64>` returns process start time
- [ ] Linux implementation: parse `/proc/{pid}/stat` field 22 (starttime in clock ticks)
- [ ] macOS implementation: use `libproc` crate or `sysctl kern.proc.pid.{pid}` to get `p_starttime`
- [ ] `is_process_alive(pid: u32) → bool` checks via `kill(pid, 0)`
- [ ] `verify_process(pid: u32, expected_start_time: u64) → ProcessState` returns: Alive, Dead, PidReused
- [ ] Compile-time platform selection via `#[cfg(target_os = "...")]`
- [ ] Tests on both platforms in CI
- [ ] Typecheck and format pass

### US-089: Platform Abstraction — Signal Handling
**Description:** As a developer, I want cross-platform signal handling so that process management works on both Linux and macOS.

**Acceptance Criteria:**
- [ ] `send_signal(pid: u32, signal: Signal) → Result<()>` wraps `nix::sys::signal::kill`
- [ ] Signal enum: SIGTERM, SIGKILL, SIGHUP
- [ ] `install_signal_handler(signal: Signal, handler: fn())` for daemon shutdown
- [ ] Works identically on Linux and macOS (both are POSIX)
- [ ] Typecheck and format pass

---

### SECTION H: Testing

---

### US-090: Unit Tests — DAG and Topological Sort
**Description:** As a developer, I want DAG tests so that dependency resolution is proven correct.

**Acceptance Criteria:**
- [ ] Test: build DAG from 5 stories with linear dependencies → topological sort returns correct order
- [ ] Test: build DAG from stories with diamond dependency → valid sort order
- [ ] Test: detect cycle in 3-story circular dependency → CyclicDependency error with cycle list
- [ ] Test: DAG with no dependencies → all stories independent, any order valid
- [ ] Test: `find_ready_stories` with mix of done/pending blockers
- [ ] Test: cross-wave dependency rejected
- [ ] All tests pass: `cargo test -p nflow-core dag`

### US-091: Unit Tests — Scheduler Algorithm
**Description:** As a developer, I want scheduler tests so that story selection and parallel limits are verified.

**Acceptance Criteria:**
- [ ] Test: 5 ready stories, max_parallel=3 → returns 3 start actions
- [ ] Test: 2 running + 3 ready, max_parallel=3 → returns 1 start action
- [ ] Test: all stories done → returns no actions
- [ ] Test: story with unmet dependencies → not selected (stays pending)
- [ ] Test: propagation: blocker done → blocked story becomes ready
- [ ] Test: execution_enabled=false → no start actions
- [ ] Test: wave ordering: W1 stories scheduled before W2
- [ ] Test: epic status materialization from child stories
- [ ] All tests pass: `cargo test -p nflow-core scheduler`

### US-092: Unit Tests — Work Item State Machines
**Description:** As a developer, I want state machine tests so that all valid and invalid transitions are covered.

**Acceptance Criteria:**
- [ ] Test all valid story transitions: pending→ready→in_progress→done, in_progress→failed, failed→in_progress, in_progress→cancelled, ready→cancelled, pending→cancelled
- [ ] Test all valid task transitions: pending→in_progress→done, in_progress→failed, failed→in_progress(retry), failed→done(skip), pending→cancelled
- [ ] Test invalid transitions return error: done→in_progress, pending→done, cancelled→done
- [ ] Test skip_task marks paired verify as skipped
- [ ] Test auto_generate_verify_tasks produces correct sort_order and short_ids
- [ ] All tests pass: `cargo test -p nflow-core work_item`

### US-093: Unit Tests — Spec State Machine
**Description:** As a developer, I want spec state machine tests so that lifecycle transitions are verified.

**Acceptance Criteria:**
- [ ] Test valid: draft→approved, approved→draft(reopen), approved→decomposed, draft→deleted, approved→deleted
- [ ] Test invalid: decomposed→approved(reopen), decomposed→deleted, deleted→draft
- [ ] Test session_active: can't start session when already active
- [ ] Test session_active: end_session stores claude_session_id
- [ ] All tests pass: `cargo test -p nflow-core spec`

### US-094: Unit Tests — Short ID System
**Description:** As a developer, I want short ID tests so that generation and resolution work correctly.

**Acceptance Criteria:**
- [ ] Test: generate short IDs for 3 epics, 5 stories, 10 tasks → E1-E3, S1-S5, T1-T10
- [ ] Test: verify tasks get "{id}v" suffix: T1v, T2v, etc.
- [ ] Test: wave prefix: W1-S1, W2-T3v
- [ ] Test: resolve "W1-S1" → correct UUID
- [ ] Test: resolve "S1" with single wave → correct UUID
- [ ] Test: resolve "S1" with multiple waves → ambiguity error
- [ ] Test: resolve non-existent ID → NotFound error
- [ ] All tests pass: `cargo test -p nflow-core short_id`

### US-095: Unit Tests — Config Merging
**Description:** As a developer, I want config merge tests so that layered configuration works correctly.

**Acceptance Criteria:**
- [ ] Test: defaults only → all default values
- [ ] Test: global overrides one key → that key overridden, others default
- [ ] Test: per-project overrides global → project value wins
- [ ] Test: env var overrides all → env value wins
- [ ] Test: validation rejects max_parallel=0, max_time_per_task=-1
- [ ] Test: validation rejects unknown git_provider
- [ ] All tests pass: `cargo test -p nflow-core config`

### US-096: Unit Tests — Stream-JSON Parser
**Description:** As a developer, I want parser tests so that all stream-json message types are handled correctly.

**Acceptance Criteria:**
- [ ] Test: parse text delta event → TextDelta("text")
- [ ] Test: parse tool_use event → ToolUse { name: "Read", input: {...} }
- [ ] Test: parse tool_result event → ToolResult { content: "..." }
- [ ] Test: parse result event → Result { text: "...", session_id: "..." }
- [ ] Test: parse malformed JSON → ParseError (not crash)
- [ ] Test: parse partial line → buffered until newline
- [ ] Test: detect AskUserQuestion tool call
- [ ] Test: evaluate impl success criteria (all 3 conditions)
- [ ] Test: evaluate verify success criteria
- [ ] All tests pass: `cargo test -p nflow-claude`

### US-097: Unit Tests — Prompt Template Rendering
**Description:** As a developer, I want template tests so that variable substitution works correctly.

**Acceptance Criteria:**
- [ ] Test: render template with all variables present → correct output
- [ ] Test: render template with missing required variable → error
- [ ] Test: render template with extra variables → ignored (no error)
- [ ] Test: literal braces in template not matching any var → preserved
- [ ] Test: override template loaded from file instead of embedded
- [ ] All tests pass: `cargo test -p nflow-claude prompt`

### US-098: Integration Tests — Daemon Lifecycle
**Description:** As a developer, I want daemon lifecycle tests so that start/stop/crash recovery are verified end-to-end.

**Acceptance Criteria:**
- [ ] Test fixture: temporary `NFLOW_HOME` directory, isolated daemon instance
- [ ] Test: start daemon → PID file created, socket exists, status reports running
- [ ] Test: stop daemon → PID file removed, socket removed, status reports stopped
- [ ] Test: start foreground → runs in current process, stops on signal
- [ ] Test: crash recovery: start daemon, start mock agent, kill daemon (SIGKILL), restart → stale agents marked failed
- [ ] Test: flock prevents two daemons starting simultaneously
- [ ] Test: auto-start from CLI command
- [ ] Cleanup: all temp dirs and processes removed after tests
- [ ] All tests pass: `cargo test -p nflow-daemon lifecycle`

### US-099: Integration Tests — NDJSON Protocol
**Description:** As a developer, I want protocol tests so that client-server communication is verified.

**Acceptance Criteria:**
- [ ] Test: send valid request → receive valid response with matching UUID
- [ ] Test: send malformed JSON → receive error response
- [ ] Test: send unknown command → receive NOT_FOUND error
- [ ] Test: protocol handshake succeeds with matching version
- [ ] Test: protocol handshake fails with mismatched version
- [ ] Test: streaming response → multiple lines received until done=true
- [ ] Test: client disconnects mid-stream → daemon handles gracefully
- [ ] All tests pass: `cargo test -p nflow-daemon protocol`

### US-100: Integration Tests — Project and Spec Commands
**Description:** As a developer, I want command integration tests so that project and spec CRUD work through the socket.

**Acceptance Criteria:**
- [ ] Test: project.init → project created in DB, directories exist
- [ ] Test: project.init duplicate name → ALREADY_EXISTS error
- [ ] Test: project.list → returns all projects
- [ ] Test: project.delete → project removed, directories removed
- [ ] Test: spec.new → spec created in DB, session started
- [ ] Test: spec.list → returns specs for project
- [ ] Test: spec.approve → status changes to approved
- [ ] Test: spec.approve on approved → INVALID_STATE error
- [ ] Test: spec.delete on decomposed → error
- [ ] All tests pass: `cargo test -p nflow-daemon commands_project_spec`

### US-101: Integration Tests — Plan Commands
**Description:** As a developer, I want plan integration tests so that decomposition and approval work through the socket.

**Acceptance Criteria:**
- [ ] Test: plan.generate with mock Claude → wave created, work items in DB
- [ ] Test: plan.generate with no approved specs → error
- [ ] Test: plan.generate with existing draft wave → error
- [ ] Test: plan.show → returns correct tree structure
- [ ] Test: plan.feedback → old items deleted, new items created
- [ ] Test: plan.approve → session status = approved
- [ ] Test: plan.discard → items deleted, specs back to approved
- [ ] Uses mock Claude binary for deterministic output
- [ ] All tests pass: `cargo test -p nflow-daemon commands_plan`

### US-102: Integration Tests — Execution Commands
**Description:** As a developer, I want execution integration tests so that run/stop/retry/skip work through the socket.

**Acceptance Criteria:**
- [ ] Test: exec.run → execution_enabled set, scheduler picks up stories
- [ ] Test: exec.pause → no new tasks started
- [ ] Test: exec.status → returns correct work item states
- [ ] Test: exec.stop → agent terminated, story cancelled
- [ ] Test: exec.retry → failed task restarted with new agent
- [ ] Test: exec.skip → task marked done, verify skipped, story continues
- [ ] Test: exec.continue → story resumes from next pending task
- [ ] Test: exec.cancel → pending story cancelled
- [ ] Uses mock Claude binary
- [ ] All tests pass: `cargo test -p nflow-daemon commands_exec`

### US-103: E2E Test — Mock Claude Binary
**Description:** As a developer, I want a mock Claude binary so that E2E tests run without real API calls.

**Acceptance Criteria:**
- [ ] Mock binary (Rust binary or shell script) that reads `-p` prompt and returns canned responses
- [ ] Supports `--output-format stream-json`: outputs valid stream-json events
- [ ] Spec mode: outputs questions, reads answers via `--resume`, writes spec file
- [ ] Decompose mode: returns valid JSON DAG with epics/stories/tasks
- [ ] Impl task mode: creates a commit with correct `[W{n}-{id}]` message format
- [ ] Verify task mode: outputs "VERIFICATION PASSED" in result
- [ ] Supports `--resume` flag (tracks session IDs)
- [ ] Configurable: can simulate failures (non-zero exit, no commit, wrong message)
- [ ] Typecheck and format pass

### US-104: E2E Test — Full Happy Path
**Description:** As a developer, I want a full happy-path E2E test so that the entire workflow is verified end-to-end.

**Acceptance Criteria:**
- [ ] Setup: temporary git repo with initial commit, temporary NFLOW_HOME, mock Claude in PATH
- [ ] `nflow init` → project registered
- [ ] Spec creation via mock Claude → spec file written
- [ ] `nflow spec approve` → spec approved
- [ ] `nflow plan generate` → wave created with work items
- [ ] `nflow plan approve` → wave approved
- [ ] `nflow run` → stories execute in worktrees, tasks create commits
- [ ] All stories complete → branches pushed, MRs created (mock gh/glab)
- [ ] `nflow status` → all items show done
- [ ] Teardown: all temp dirs and processes cleaned up
- [ ] Test passes on Linux and macOS

### US-105: E2E Test — Failure and Recovery Scenarios
**Description:** As a developer, I want failure E2E tests so that error recovery paths are verified.

**Acceptance Criteria:**
- [ ] Test: impl task fails (mock Claude exits non-zero) → task and story marked failed
- [ ] Test: `nflow retry` → task re-executed, succeeds → story continues
- [ ] Test: `nflow skip` → task skipped, verify skipped → story continues to next impl
- [ ] Test: verify task fails → story marked failed immediately
- [ ] Test: `nflow stop` → running agent terminated, story cancelled
- [ ] Test: `nflow continue` on cancelled story → story resumes
- [ ] Test: wall-clock timeout → agent killed, task failed
- [ ] All tests pass on Linux and macOS

### US-106: E2E Test — Parallel Execution
**Description:** As a developer, I want parallel execution E2E tests so that concurrent story execution is verified.

**Acceptance Criteria:**
- [ ] Setup: plan with 3 independent stories (no dependencies)
- [ ] `nflow run --parallel 3` → all 3 stories start simultaneously
- [ ] Each story creates its own worktree and branch
- [ ] All 3 complete independently → 3 MRs created
- [ ] No data corruption in SQLite (WAL handles concurrent reads)
- [ ] Test passes on Linux and macOS

### US-107: Cross-Platform CI
**Description:** As a developer, I want CI running on both platforms so that cross-platform support is continuously verified.

**Acceptance Criteria:**
- [ ] GitHub Actions (or equivalent) workflow with matrix: [ubuntu-latest, macos-latest]
- [ ] Steps: cargo fmt --check, cargo clippy, cargo test --workspace, E2E tests
- [ ] Mock Claude binary built and available in PATH for tests
- [ ] All tests pass on both platforms
- [ ] CI runs on every push and PR

---

## Functional Requirements

- FR-1: The system must provide a six-crate Rust workspace (`nflow-core`, `nflow-claude`, `nflow-git`, `nflow-daemon`, `nflow-cli`, `nflow-tui`) buildable with `cargo build --release`
- FR-2: `nflow-core` must contain zero IO and zero async code; all other crates depend on it
- FR-3: The daemon must be the sole writer to SQLite; CLI and TUI are read-only clients
- FR-4: The daemon must run the scheduler on its main event loop, serialized with command handling (no separate task)
- FR-5: All state must live in SQLite at `~/.nflow/nflow.db` with WAL mode; specs are markdown files on disk referenced by path
- FR-6: The system must support the full three-phase flow: SPEC → DECOMPOSE → EXECUTE
- FR-7: The scheduler must enforce `max_parallel` concurrent agents and story-level dependency ordering
- FR-8: Each story must execute in its own git worktree with its own branch
- FR-9: Impl tasks must use `--allowedTools "Read,Write,Edit,Bash,Glob,Grep"` with `--max-turns 50`
- FR-10: Verify tasks must use `--allowedTools "Read,Bash,Glob,Grep"` (no Write/Edit) with `--max-turns 30`
- FR-11: All Claude invocations must use `--output-format stream-json --verbose --include-partial-messages`
- FR-12: The system must create MRs/PRs via `gh` (GitHub) or `glab` (GitLab) CLI after story completion
- FR-13: Prompt templates must be embedded at compile time via `include_str!` with runtime overrides from `~/.nflow/prompts/`
- FR-14: The daemon must perform crash recovery on startup: verify stale agent PIDs, reset spec sessions, restore work item states
- FR-15: The daemon must gracefully shut down on SIGTERM: stop scheduler → wait 60s → SIGTERM agents → 10s → SIGKILL
- FR-16: The TUI must provide four views (Specs, Plan, Execute, Logs) switchable with keys 1-4
- FR-17: The system must work on both Linux and macOS with platform-specific PID detection
- FR-18: Short IDs (W1-S1, W2-T3v) must be used for CLI input and display; UUIDs used internally
- FR-19: Configuration must merge: defaults → `config.toml` → per-project overrides → environment variables (highest priority)
- FR-20: The system must validate DAGs for circular dependencies before accepting a plan
- FR-21: The daemon must enforce one active spec session per project at a time
- FR-22: The daemon must enforce one draft wave per project at a time
- FR-23: Verify tasks must be auto-generated after each impl task with kind='verify' and short_id="{id}v"
- FR-24: Task execution within a story must be sequential in sort_order (impl → verify → impl → verify)
- FR-25: On impl task skip, the paired verify task must also be auto-skipped
- FR-26: Story completion must include: git rebase, git push, MR/PR creation — in that order
- FR-27: Wall-clock timeout must kill agents after `max_time_per_task` seconds (SIGTERM → 10s → SIGKILL)
- FR-28: Retry must include the previous error in the agent prompt context
- FR-29: The system must track `pid_start_time` to detect PID reuse during crash recovery
- FR-30: All crate public APIs must return `Result<T, NflowError>` — no panics on user input

## Non-Goals

- No web UI or REST API — CLI and TUI only
- No container/namespace sandboxing for agents (agents run with user permissions)
- No automatic conflict resolution for parallel worktrees (user responsibility)
- No auto-merge of MRs/PRs — all merge requests require human review
- No secrets management — authentication delegated to `claude`, `gh`, and `glab` CLIs
- No Windows support
- No support for non-Claude AI providers
- No built-in CI/CD integration (relies on existing CI triggered by MRs)
- No multi-user or team features (single-user tool)
- No priority-based scheduling (stories execute in dependency order, not priority)
- No automatic retry on task failure (user must explicitly `nflow retry`)
- No idle timeout for agents (only wall-clock timeout)
- No template engine (only simple `{variable}` substitution)
- No mouse support in TUI (keyboard-only)
- No automatic log rotation
- No database auto-repair on corruption

## Design Considerations

### TUI Layout
- Four-tab layout with status bar at bottom
- Specs view: list with inline status badges
- Plan view: indented tree with collapse/expand
- Execute view: vertical split (40% tree / 60% log output)
- Logs view: full-screen with line wrapping
- Consistent color scheme: green (done), yellow (running), red (failed), blue (ready), gray (pending), dim (cancelled)

### CLI UX
- All commands follow `nflow <noun> <verb>` pattern (e.g., `nflow spec new`, `nflow plan approve`)
- Short aliases where useful (e.g., `nflow run` instead of `nflow execution start`)
- Colored output with `--no-color` flag for piping
- JSON output with `--json` flag for scripting

### Error Messages
- Every error includes: what went wrong, why, and what to do next
- Example: "Cannot approve spec 'auth' — spec is already in 'decomposed' state. Run `nflow plan discard` first to free the spec."

## Technical Considerations

### Dependencies
- Rust 1.75+ (for async trait stabilization)
- `tokio` for async runtime (daemon only)
- `rusqlite` for SQLite access (with `bundled` feature for cross-platform)
- `git2` for git operations (with CLI fallback for worktree operations)
- `clap` with derive for CLI argument parsing
- `ratatui` + `crossterm` for TUI
- `serde` + `serde_json` for serialization
- `uuid` for ID generation (v4)
- `toml` for config parsing
- `tracing` + `tracing-subscriber` for structured logging
- `nix` for Unix signal handling and setsid
- `chrono` for timestamps

### Platform-Specific Code
- PID start time: `/proc/{pid}/stat` on Linux, `libproc` crate on macOS
- Daemonization: `setsid()` via `nix` crate (works on both platforms)
- File permissions: standard Unix chmod via `std::fs::set_permissions` (works on both platforms)

### Performance
- SQLite WAL mode allows concurrent reads from CLI/TUI while daemon writes
- Scheduler tick every 2 seconds is sufficient — no need for sub-second scheduling
- Stream-json parsing should not buffer entire agent output in memory — process line by line
- Worktree creation is the bottleneck — `git fetch` is network-bound, not CPU-bound
- TUI rendering capped at 60 FPS or event-driven to avoid CPU spin

### Error Handling
- All crate public APIs return `Result<T, NflowError>` with typed error variants
- `nflow-core` errors are pure data (no IO errors)
- Daemon wraps IO/system errors with context via `thiserror`
- CLI displays errors with helpful messages and suggested actions
- Never panic on user input — only panic on programmer errors (internal invariants)

## Success Metrics

- Full end-to-end workflow completes: spec → decompose → execute → MRs created
- 3+ stories execute in parallel without conflicts or data corruption
- Daemon recovers cleanly from crash (kill -9) with no orphaned processes or stuck states
- All 107 user stories implemented and passing acceptance criteria
- All tests pass on both Linux and macOS in CI
- TUI renders correctly in standard terminal emulators (iTerm2, Alacritty, GNOME Terminal, kitty)
- Single developer can go from `nflow init` to merged MRs in one session

## Open Questions

- Should nflow support resuming a partially-executed wave after a rebase conflict on push? (Currently: story → failed, user must retry delivery)
- Should there be a `nflow diff` command to preview changes across all worktrees before MR creation?
- What is the maximum reasonable number of parallel agents? Should there be a hard cap beyond `max_parallel`?
- Should nflow track token usage / API costs per task for budget awareness?
- Should there be a `--dry-run` mode for execution that shows what would happen without spawning agents?
- Should the TUI support resizing gracefully or assume a minimum terminal size?
- Should the daemon support log rotation or leave it to the user (logrotate)?
