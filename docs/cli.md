# nflow — CLI Reference

## General

```
nflow [OPTIONS] <COMMAND>
```

Global options:
- `--project <name>` — override project (default: detected from current directory)
- `--verbose` — verbose output
- `--json` — output in JSON format (for scripting/agent use)

## Work Item IDs

All CLI commands that accept `<task-id>` or `<story-id>` use **wave-prefixed short IDs** — the wave number + human-readable identifier generated during decomposition:

Format: `W{wave}-{short_id}`

- Epics: `W1-E1`, `W2-E1`, ...
- Stories: `W1-S1`, `W1-S2`, ...
- Tasks (impl): `W1-T1`, `W1-T2`, ...
- Tasks (verify): `W1-T1v`, `W1-T2v`, ...

The wave prefix ensures uniqueness across waves. nflow resolves them to internal UUIDs automatically.

```bash
nflow retry W1-T3v       # retry verify task T3v in wave 1
nflow skip W1-T3         # skip impl task T3 in wave 1
nflow log W2-T5          # view log for task T5 in wave 2
nflow continue W1-S2     # continue story S2 in wave 1
nflow stop W1-S3         # stop story S3 in wave 1
```

Use `nflow status` or `nflow plan show` to see current IDs.

---

## Daemon

### `nflow daemon start`

Start the background daemon process.

- Creates `~/.nflow/nflow.sock` (Unix socket)
- Creates `~/.nflow/daemon.pid` (PID file)
- Runs scheduler loop
- Logs to `~/.nflow/logs/daemon.log`

Options:
- `--foreground` — run in foreground (for debugging)

### `nflow daemon stop`

Stop the running daemon.

### `nflow daemon status`

Show daemon status (running/stopped, PID, uptime).

---

## Project

### `nflow init`

Initialize a new nflow project in the current directory.

```bash
nflow init --name "my-project"
```

Required:
- `--name <name>` — project name

Optional:
- `--base-branch <branch>` — base branch (default: `main`)
- `--git-provider <provider>` — `github` or `gitlab` (default: `github`)

Validates that the current directory is a git repository (checks for `.git`). Fails with error if not a git repo — worktrees require an existing repository.

Creates entry in SQLite and directory `~/.nflow/projects/{name}/`.

### `nflow projects list`

List all registered projects.

Output columns: name, path, status (specs count, tasks count, running agents).

### `nflow project delete <name>`

Remove a project from nflow.

- Fails if any stories are `in_progress` (must stop them first). Use `--force` to bypass this check (e.g., when the daemon is crashed and `nflow stop` cannot be sent — force-delete marks in-progress items as cancelled and proceeds).
- Deletes: project record, all specs, all work items, all agent runs from SQLite
- Deletes: `~/.nflow/projects/{name}/` directory (specs, agent logs)
- Removes all worktrees for this project (runs `git worktree remove` for each)
- Does NOT delete the original repository
- Requires confirmation (`--force` to skip)

---

## Specs

### `nflow spec new <name>`

Start a new spec session.

```bash
nflow spec new "auth-login"
nflow spec new "payments" --with-codebase
```

Options:
- `--with-codebase` — give Claude access to the project's source code

Starts an interactive dialogue with Claude. In CLI mode, questions/answers flow through stdin/stdout. In TUI, displayed in the spec view.

### `nflow spec list`

List specs for the current project.

Output columns: name, status (draft/approved/decomposed), wave (wave number if decomposed, or "unassigned" if approved but not yet in any wave), created, updated.

### `nflow spec view <name>`

Print spec contents to stdout.

### `nflow spec resume [name]`

Resume a paused spec session. If name is omitted, resumes the most recently updated spec in `draft` status (by `updated_at` descending). Only `draft` specs can be resumed.

### `nflow spec approve <name>`

Mark a spec as approved. Only `draft` specs can be approved. If the spec has an active Claude session (`session_active = 1`), the session is stopped first (SIGTERM to the Claude process), then the spec is approved.

### `nflow spec reopen <name>`

Move an `approved` spec back to `draft` for further editing.

- Only works on `approved` specs
- If spec is `decomposed` — fails with error. Must `nflow plan discard` first to move the spec back to `approved`, then reopen.

### `nflow spec delete <name>`

Soft-delete a spec. Sets status to `deleted`. The spec record remains in the database but is hidden from all listings. The markdown file on disk is preserved (can be recovered manually).

- If spec is `draft` with an active session — stops the session first
- If spec is `approved` — requires confirmation (`--force` to skip)
- If spec is `decomposed` — fails with error (must discard the plan first)

---

## Plan (Waves)

Each `nflow plan generate` creates a new **wave** — an independent batch of work from unassigned approved specs. Multiple waves can coexist and execute in parallel. Commands that target a specific wave accept `--wave <n>`. When omitted, the default target is the latest `in_progress` (draft) wave.

### `nflow plan generate`

Create a new wave from all unassigned approved specs (specs not yet used in any wave).

Calls Claude to decompose specs into structured work items. Stores result in SQLite as a new wave with an auto-incremented number (wave-1, wave-2, ...).

```bash
nflow plan generate                    # new wave from all unassigned approved specs
nflow plan generate --specs "auth,payments"  # only use specific specs
nflow plan generate --with-codebase    # give Claude read access to the codebase
```

Options:
- `--specs <names>` — only use specific specs (comma-separated). Default: all unassigned approved specs.
- `--with-codebase` — give Claude read access to the project's source code during decomposition. Claude can inspect existing file structure, patterns, and conventions to produce more accurate task descriptions. Without this flag, Claude works only from spec text.

Guards:
- Fails if there is already an `in_progress` (draft) wave. One draft wave at a time — approve or discard the current draft first.
- Fails if there are no unassigned approved specs.

### `nflow plan show`

Display the plan as an ASCII tree with statuses.

```bash
nflow plan show              # show all waves
nflow plan show --wave 2     # show only wave 2
```

```
WAVE 1 [executing] (3/5 stories done)
  EPIC-1: Authentication System
    STORY-1: User Login [done] (MR: !42)
      T1  impl    Create login API endpoint [done]
      T1v verify  Verify: Create login API endpoint [done]
      ...
    STORY-2: User Registration [running]
      T4  impl    Create registration endpoint [done]
      T4v verify  Verify: Create registration endpoint [in_progress]
      ...
    STORY-3: OAuth Integration [pending] (blocked by: STORY-1)
      ...

WAVE 2 [draft] — reviewing plan
  EPIC-2: Payment System
    STORY-4: Stripe Integration [pending]
      T8  impl    Create Stripe client [pending]
      T8v verify  Verify: Create Stripe client [pending]
      ...
```

Options:
- `--wave <n>` — show only a specific wave
- `--dag` — show story dependency graph as an ASCII adjacency list:
  ```
  WAVE 1:
    S1: User Login           → (no deps)
    S2: User Registration    → (no deps)
    S3: OAuth Integration    → S1
    S4: Password Reset       → S1, S2
  ```

### `nflow plan feedback <message>`

Provide feedback on the current draft wave. Claude regenerates the plan based on feedback.

```bash
nflow plan feedback "split story 3 into separate stories for each OAuth provider"
```

Options:
- `--wave <n>` — target a specific wave (default: latest `in_progress` wave)

Only works when the target wave's status is `in_progress` (not yet approved). Fails with `INVALID_STATE` if the wave has already been approved.

### `nflow plan approve`

Approve a wave's plan. Locks its work items for execution.

```bash
nflow plan approve            # approve latest draft wave
nflow plan approve --wave 3   # approve specific wave
```

Options:
- `--wave <n>` — target a specific wave (default: latest `in_progress` wave)

### `nflow plan discard`

Discard a wave's plan. Deletes all work items from the wave and resets its specs from `decomposed` to `approved` (making them available for a future wave).

```bash
nflow plan discard            # discard latest draft wave
nflow plan discard --wave 2   # discard specific wave
```

Options:
- `--wave <n>` — target a specific wave (default: latest `in_progress` wave)

Guards:
- Fails if the wave has stories with status `in_progress` (must stop them first)
- After discard, specs become unassigned and can be included in a new `nflow plan generate`

---

## Execution

### `nflow run`

Enable execution for the current project. The scheduler picks up all `ready` stories and begins running agents.

If `auto_execute = true` in config, execution starts automatically after `nflow plan approve` and this command is not required. With the default `auto_execute = false`, this command must be run explicitly after plan approval.

Options:
- `--parallel <n>` — max parallel agents (overrides config)
- `--story <id>` — run only a specific story
- `--dry-run` — show what would run without starting agents

### `nflow pause`

Disable execution for the current project. The scheduler stops picking up new stories, but running agents continue until they complete or fail. No new tasks are started.

Sets `execution_enabled = 0` on the project. Use `nflow run` to re-enable.

### `nflow status`

Show execution status of all work items, grouped by wave.

```
PROJECT: my-project (4 running, 3 pending, 8 done)

  WAVE 1 [executing]
    [done]     STORY-1: User Login           (6/6 tasks, MR: !42)
    [running]  STORY-2: User Registration    (7/10 tasks, T5v verify in progress)
    [pending]  STORY-3: OAuth Integration    (0/4 tasks, blocked by: S1)

  WAVE 2 [executing]
    [running]  STORY-4: Stripe Integration   (3/8 tasks, T8 impl in progress)
    [pending]  STORY-5: Invoice Generation   (0/6 tasks, blocked by: S4)
    [done]     STORY-6: Payment Webhooks     (6/6 tasks, MR: !44)
```

Options:
- `--wave <n>` — show only a specific wave

### `nflow log <task-id>`

Stream or print the agent output log for a task.

Options:
- `--follow`, `-f` — follow the log in real-time (like `tail -f`)

### `nflow retry <task-id>`

Re-run a failed task.

Mechanics:
- **Resets worktree to last good commit** before spawning the agent: `git checkout . && git clean -fd` in the worktree. This removes partial file changes left by the failed agent (uncommitted edits, half-written files). The last successful commit remains intact.
- Creates a **new** agent_run (fresh invocation, not `--resume` — context may be corrupted)
- Task status: `failed` → `in_progress`
- Story status: `failed` → `in_progress`
- Remaining `pending` tasks in the story continue sequentially after retry

**Retry warning:** nflow tracks the number of retry attempts per task (count of `agent_runs` for the work item). After **3 retries**, nflow displays a warning: `"Task {id} has been retried {n} times. Consider fixing manually or skipping."` The warning does not block the retry — it is informational only. No `--force` is needed.

**Retry on impl task:**
- Its paired `verify` task (still `pending`) will run automatically after the retry succeeds

**Retry on verify task:**
- Re-runs verification against the current state of the worktree
- Common flow: verify fails → user fixes code manually in the worktree, commits → `nflow retry {verify_task_id}` → verify agent re-checks
- The preceding impl task stays `done` — only the verify task is re-executed

### `nflow skip <task-id>`

Mark a failed task as done and continue with the next task in the story.

Mechanics:
- Task status: `failed` → `done` (metadata flag `skipped = true`)
- Story status: `failed` → `in_progress`
- Next `pending` task starts executing
- If this was the last task — proceeds to MR creation
- No commit is created for skipped tasks
- MR body marks skipped tasks: "[SKIPPED] task title"

**Skip on impl task:**
- Its paired `verify` task is also skipped automatically (nothing to verify)

**Skip on verify task:**
- Only the verify task is skipped; the preceding impl task's commit is preserved
- Execution continues with the next `impl` task in the story
- Use this when you're confident the implementation is correct despite the verification failure

### `nflow continue <story-id>`

Continue a failed or cancelled story from where it left off. Used after manual fixes in the worktree or to resume a stopped story.

Works on stories with status `failed` or `cancelled`.

Mechanics for `failed` stories:
1. Tasks with a stored `commit_hash` in the database are already `done` — skip them.
2. The currently failed task is shown to the user with its description and error message:
   ```
   Failed task: W1-T3 — Add login form component
   Error: Build failed: cannot find module 'react-hook-form'
   Mark as done and continue? [y/N]
   ```
   The user must confirm. Use `--force` to skip confirmation.
3. The failed task is marked `done`.
4. Story status: `failed` → `in_progress`.
5. Resumes execution from the next `pending` task.
6. If all tasks are `done`, proceeds to rebase/push/MR creation.

Mechanics for `cancelled` stories:

**If worktree exists** (story was cancelled from `in_progress`):
1. Story status: `cancelled` → `in_progress`
2. All `cancelled` tasks in the story are reset to `pending`
3. Tasks with a stored `commit_hash` are marked `done` (+ their paired verify tasks)
4. Scheduler resumes from the first `pending` task

**If worktree does not exist** (story was cancelled from `pending` or `ready`):
1. All `cancelled` tasks in the story are reset to `pending`
2. Story status: `cancelled` → `ready` (if all dependencies are met) or `pending` (if dependencies are unmet)
3. The scheduler picks it up normally when it becomes `ready` — creates worktree and starts execution from the first task

### `nflow stop [story-id]`

Stop a running story's agent.

- If `story-id` is given — stops that specific story
- If omitted — stops all running agents **in the current project** (determined by `--project` flag or cwd)
- `--wave <n>` — stop all running agents in a specific wave
- `--all` — stop all running agents across **all projects**

Mechanics:
- Sends SIGTERM to the Claude process
- Sets the current running task to `cancelled`
- Sets the story to `cancelled`
- Does NOT remove the worktree (user may want to inspect it)

### `nflow cancel <story-id>`

Cancel a story that hasn't started yet. Only works on `pending` or `ready` stories. Sets status to `cancelled`.

Options:
- `--wave <n>` — cancel all `pending`/`ready` stories in a wave

---

## Worktrees

### `nflow worktree list`

List all active worktrees for the current project.

Output columns: story, branch, path, status (in_progress/done/failed/cancelled).

### `nflow cleanup`

Remove old agent logs and other accumulated data.

```bash
nflow cleanup --logs              # delete agent logs for done stories
nflow cleanup --logs --all        # delete all agent logs (including failed/in-progress)
nflow cleanup --logs --older-than 30d  # delete agent logs older than 30 days
```

Options:
- `--logs` — clean agent logs (`agent-logs/{id}.log` files)
- `--all` — include logs for non-done stories (default: only done stories)
- `--older-than <duration>` — only delete logs older than the given duration (e.g., `7d`, `30d`)
- `--dry-run` — show what would be deleted without deleting

### `nflow worktree clean`

Remove worktrees for completed (`done`) and cancelled stories.

```bash
nflow worktree clean              # clean current project
nflow worktree clean --all        # clean all projects
```

- Only removes worktrees for stories with status `done` or `cancelled`
- Runs `git worktree remove` for each
- Updates `worktree_path` to NULL in the database
- Does NOT remove worktrees for `failed` or `in_progress` stories

---

## Pipeline

Pipeline Flow provides a simplified Plan → Implement → Review workflow for rapid development without the overhead of formal specification and decomposition. See `docs/pipeline.md` for detailed documentation.

### `nflow pipeline start <description>`

Start a new pipeline run.

```bash
nflow pipeline start "Add user profile page with avatar upload"
nflow pipeline start "Fix login redirect bug" --max-iterations 3
```

Arguments:
- `<description>` — task description for Claude (required)

Options:
- `--max-iterations <n>` — max Implement→Review loops (default: 5)

Runs three stages sequentially:
1. **Plan** — Claude analyzes task and creates implementation plan
2. **Implement** — Claude executes plan and makes code changes
3. **Review** — Claude verifies implementation, loops back if issues found

The command streams output in real-time. Only one pipeline can be active per project at a time.

### `nflow pipeline list`

List all pipeline runs for the current project.

```bash
nflow pipeline list
```

Output columns: ID (short), description, state (Running/Completed/Failed/Cancelled), iteration (current/max), created, updated.

### `nflow pipeline status <pipeline-id>`

Show detailed status for a specific pipeline run.

```bash
nflow pipeline status abc123
```

Displays:
- Pipeline metadata (description, state, iterations)
- Stage history (Plan, Implement, Review with timestamps)
- Stage outputs (approach, changes, review feedback)
- Current iteration progress

### `nflow pipeline cancel <pipeline-id>`

Cancel a running pipeline.

```bash
nflow pipeline cancel abc123
```

Stops the currently running agent (SIGTERM) and sets pipeline state to Cancelled. Cannot cancel pipelines that are already Completed/Failed/Cancelled.

### `nflow pipeline log <pipeline-id> [--stage-id <stage-id>]`

Stream logs for a pipeline stage.

```bash
nflow pipeline log abc123                    # latest stage
nflow pipeline log abc123 --stage-id def456  # specific stage
```

Shows raw agent output (tool calls, text, results). If stage ID is omitted, streams the most recent stage.

---

## TUI

### `nflow tui`

Launch the terminal user interface.

```bash
nflow tui
```

Connects to the daemon via Unix socket. If the daemon is not running, offers to start it. See `docs/tui.md` for full TUI documentation.

Options:
- `--project <name>` — open TUI for a specific project (default: detected from cwd)

---

## Configuration

### `nflow config show`

Print current configuration.

### `nflow config set <key> <value>`

Set a config value.

```bash
nflow config set max_parallel 4
nflow config set git_provider gitlab
```

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Daemon not running |
| 3 | Project not found |
| 4 | Invalid arguments |
