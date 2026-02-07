# nflow — Phases

## Overview

nflow operates in three sequential phases:

1. **SPEC** — Write detailed specifications through interactive dialogue with Claude
2. **DECOMPOSE** — Generate a DAG of epics/stories/tasks from approved specs
3. **EXECUTE** — Run Claude Code agents on tasks, producing commits and MRs

---

## Phase 1: SPEC

### Purpose

Create detailed technical specifications through a structured dialogue between the user and Claude. The output is markdown files that contain enough detail to generate actionable work items.

### Flow

```
User: nflow spec new "auth-login"
                │
                ▼
    Daemon spawns claude process:
    claude -p "{spec_prompt}" \
      --output-format stream-json \
      --verbose \
      --include-partial-messages \
      --append-system-prompt-file prompts/spec_session.md

    Working directory:
      --with-codebase: current_dir = {project_path}  (Claude sees the repo)
      without:         current_dir = {spec_dir}       (Claude only sees specs)
                │
                ▼
    Claude asks questions ◄──── User answers in TUI/CLI
                │
                ▼
    Claude writes spec to file
                │
                ▼
    Spec saved: ~/.nflow/projects/{project}/specs/auth-login.md
    Status: draft
```

### Spec Session Lifecycle

1. **Start**: `nflow spec new "name"` or `nflow spec new "name" --with-codebase`
2. **Dialogue**: Claude asks questions, user answers. This is a single claude session.
3. **Pause**: User can close TUI. Daemon keeps claude session ID. Next `nflow spec resume [name]` continues via `claude --resume {session_id}`. Name is optional — omitting it resumes the most recently updated `draft` spec.
4. **Complete**: Claude process exits with code 0 and the spec file exists on disk → spec status = `draft`. If exit code != 0 → session interrupted, can be resumed. No special markers or content validation — the spec is a draft for user review.
5. **Approve**: User reviews spec and runs `nflow spec approve "name"`. Status = `approved`.

### Parallel Spec Sessions

Only **one spec session can be active per project** at a time. Starting a new spec session while another is active for the same project returns an `INVALID_STATE` error. The user must first pause or complete the active session.

This is intentional: spec sessions are interactive dialogues that require user attention. Parallel sessions within the same project would be confusing. Different projects can have active spec sessions simultaneously — the daemon manages them independently.

In TUI, the dialogue sub-view shows the active session. In CLI, `nflow spec new` / `nflow spec resume` blocks on stdin/stdout for the dialogue.

### Claude Invocation Details

```bash
# With --with-codebase (working dir = project repo):
claude -p "Start a specification session for: {user_description}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file ~/.nflow/prompts/spec_session.md \
  --allowedTools "Read,Glob,Grep,Write,AskUserQuestion"
  # current_dir = project_path

# Without --with-codebase (working dir = specs folder):
claude -p "Start a specification session for: {user_description}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file ~/.nflow/prompts/spec_session.md \
  --allowedTools "Write,AskUserQuestion"
  # current_dir = ~/.nflow/projects/{project}/specs/
```

Key points:
- `AskUserQuestion` — Claude uses this tool to ask the user questions. nflow intercepts these from the stream and displays them in TUI.
- `Write` — Claude writes the final spec file.
- `Read,Glob,Grep` — included only in `--with-codebase` mode. Claude uses `current_dir` to access the project files. Without `--with-codebase`, these tools are not allowed (there's nothing useful to read).
- Working directory controls Claude's file access — no `--add-dir` flag needed.

### CLAUDE.md Integration

During execution (Phase 3), agents work inside git worktrees — full copies of the project repository. Claude Code automatically discovers and loads `CLAUDE.md` files from the working directory. This means:

- Project-level `CLAUDE.md` (coding standards, conventions, project-specific instructions) is automatically available to all task agents
- No special configuration is needed — it works by virtue of the worktree being a copy of the repo
- Users can influence agent behavior by adding instructions to their project's `CLAUDE.md` (e.g., "always use snake_case", "run `make lint` before committing")
- During spec sessions with `--with-codebase`, `CLAUDE.md` is also picked up since the working directory is the project repo

### Intercepting AskUserQuestion

When Claude uses the `AskUserQuestion` tool, nflow sees it in the `stream-json` output as a tool_use event. Each question-answer cycle is a separate Claude process (`--resume` chain):

1. Claude process exits after emitting `AskUserQuestion` tool call
2. nflow captures `session_id` from the final `result` event in the stream
3. nflow parses the question and options, displays them to the user in TUI
4. User provides answer
5. nflow starts a new process: `claude -p "{answer}" --resume {session_id} --output-format stream-json`
6. Claude continues the conversation with the answer as plain text in `-p`

The answer is passed as **plain text** — no structured tool-result imitation. Claude naturally understands the next message after its question is the answer, since the conversation history (restored via `--resume`) provides context.

### Spec Output Format

The spec file is a markdown document. Claude is instructed to include:

```markdown
# {Feature Name}

## Goals
What this feature achieves.

## Non-Functional Requirements
Performance, security, scalability constraints.

## User Stories
High-level user-facing scenarios.

## Technical Design

### Data Model
Database tables, relationships.

### API Contracts
Endpoints, request/response formats.

### Component Architecture
Modules, services, their responsibilities.

## Acceptance Criteria
Measurable criteria for "done".

## Open Questions
Anything unresolved.
```

---

## Phase 2: DECOMPOSE

### Purpose

Transform approved specs into a structured plan: a DAG of epics, stories, and tasks with dependency relationships.

### Flow

```
User: nflow plan generate
            │
            ▼
    Daemon collects all UNASSIGNED approved specs (not yet in any wave)
    Creates a new wave (wave_number = max + 1)
    Reads spec markdown files
            │
            ▼
    Daemon constructs the -p prompt by concatenating spec contents:
      "Decompose the following specifications into a plan.

       --- SPEC: auth-login ---
       {content of auth-login.md}

       --- SPEC: auth-oauth ---
       {content of auth-oauth.md}

       --- END OF SPECS ---
       Generate the plan as JSON."
            │
            ▼
    Daemon calls claude:
    claude -p "{above_prompt}" \
      --output-format stream-json \
      --verbose \
      --include-partial-messages \
      --append-system-prompt-file prompts/decompose.md \
      --allowedTools "Read,Glob,Grep"   ← only with --with-codebase
      # current_dir = project_path      ← only with --with-codebase
            │
            ▼
    Daemon streams events to connected clients (tool calls, progress)
    Claude's final `result` field contains the structured JSON plan
            │
            ▼
    nflow parses JSON, stores in SQLite (linked to this wave)
    Specs marked 'decomposed', linked to wave via decomposition_specs
            │
            ▼
    User reviews plan in TUI
            │
            ├─── Happy? → nflow plan approve → wave status = approved
            │                                   stories become schedulable
            │
            └─── Not happy? → nflow plan feedback "merge stories 2 and 3"
                      │
                      ▼
                Claude regenerates (using --resume with feedback)
                      │
                      ▼
                Repeat until approved

Meanwhile, other approved waves continue executing independently.
```

### JSON Schema for Decomposition

Claude is asked to return JSON matching this schema:

```json
{
  "type": "object",
  "required": ["epics"],
  "properties": {
    "epics": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["id", "title", "description", "acceptance_criteria", "stories"],
        "properties": {
          "id": { "type": "string", "description": "Short ID like E1, E2" },
          "title": { "type": "string" },
          "description": { "type": "string" },
          "acceptance_criteria": {
            "type": "string",
            "description": "High-level acceptance criteria for the entire epic"
          },
          "stories": {
            "type": "array",
            "items": {
              "type": "object",
              "required": ["id", "title", "description", "acceptance_criteria", "tasks"],
              "properties": {
                "id": { "type": "string", "description": "Short ID like S1, S2" },
                "title": { "type": "string" },
                "description": { "type": "string" },
                "acceptance_criteria": {
                  "type": "string",
                  "description": "Acceptance criteria for the story as a whole"
                },
                "depends_on": {
                  "type": "array",
                  "items": { "type": "string" },
                  "description": "IDs of stories that must complete before this one"
                },
                "tasks": {
                  "type": "array",
                  "items": {
                    "type": "object",
                    "required": ["id", "title", "description", "acceptance_criteria"],
                    "properties": {
                      "id": { "type": "string", "description": "Short ID like T1, T2" },
                      "title": { "type": "string" },
                      "description": {
                        "type": "string",
                        "description": "Detailed description sufficient for an AI agent to implement"
                      },
                      "acceptance_criteria": {
                        "type": "string",
                        "description": "Specific, verifiable criteria: what must build, what tests must pass, what behavior must be observable"
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
```

**Note:** Claude generates only `impl` tasks. nflow automatically inserts a `verify` task after each `impl` task during DAG construction (see below).

### Short ID to UUID Mapping

Claude generates short IDs (E1, S1, T1) in the JSON response. These are for human readability and for `depends_on` references within the same decomposition. When nflow processes the JSON:

1. Assign a UUID to each work item
2. Store the short ID in the `short_id` column (for display)
3. Build a map: `{short_id → UUID}` scoped to this decomposition session
4. Resolve `depends_on` references: `"S1"` → UUID of story with `short_id = "S1"`
5. If a `depends_on` references a non-existent ID → error, reject the decomposition

On re-generation (feedback loop), all work items from the session are deleted and recreated with new UUIDs. Short IDs may be reused by Claude across generations.

### DAG Construction

After parsing the JSON:
1. Create work_items for each epic, story, task with proper parent_id
2. **Auto-generate verify tasks:** For each impl task, insert a verify task immediately after it:
   ```
   Input from Claude:   T1(impl), T2(impl), T3(impl)
   After expansion:     T1(impl), T1v(verify), T2(impl), T2v(verify), T3(impl), T3v(verify)
   ```
   Each verify task:
   - `kind = 'verify'`
   - `short_id = "{original_short_id}v"` (e.g., T1v)
   - `title = "Verify: {original_title}"`
   - `acceptance_criteria` = copied from the preceding impl task
   - `description` = auto-generated from `verify_task.md` template
3. Resolve short ID references to UUIDs, create dependency edges (story-level)
4. Run topological sort to validate no cycles (error if cycles detected)
5. Compute initial statuses:
   - Stories with no dependencies → `ready`
   - Stories with unmet dependencies → `pending`
   - All tasks → `pending` (first task in each story will become `in_progress` when story starts)

### Feedback Loop

The decomposition session keeps the claude session ID. When user gives feedback:

```bash
claude -p "{feedback}" --resume {session_id} \
  --output-format stream-json \
  --verbose \
  --include-partial-messages
```

nflow **deletes all work items** from the decomposition session **before** calling Claude with the feedback. Claude returns an updated plan, and nflow inserts the new work items. If Claude fails during regeneration, the database has no work items for this session, but the decomposition session remains in `in_progress` status — the user can retry with another `nflow plan feedback`, or discard the wave with `nflow plan discard` and start fresh with `nflow plan generate`.

**Parsing the plan JSON from stream-json:** The decomposition uses `stream-json` output format (for progress streaming). The decompose prompt instructs Claude to output the plan as a JSON code block in its final response. nflow extracts the JSON from the `result` field of the final `{"type": "result", ...}` event, parses it against the expected schema, and stores the work items. If the JSON is malformed or doesn't match the schema, the decomposition fails and the user can retry.

### Waves

Each `nflow plan generate` creates a new **wave** — an independent decomposition session with its own epics, stories, and tasks. Multiple waves can coexist in a project and execute in parallel.

**Wave lifecycle:**
```
nflow plan generate   →  wave N (in_progress / draft)
nflow plan feedback   →  wave N regenerated (still in_progress)
nflow plan approve    →  wave N (approved) → stories become schedulable
nflow plan discard    →  wave N (discarded) → specs freed for reuse
```

**Iterative workflow:**
```bash
# Wave 1: auth features
nflow spec new "auth" → approve
nflow plan generate                    # creates wave-1
nflow plan approve                     # wave-1 approved
nflow run                              # wave-1 stories start executing

# Wave 2: payments (while wave-1 is still running)
nflow spec new "payments" → approve
nflow plan generate                    # creates wave-2 (wave-1 unaffected)
nflow plan approve                     # wave-2 approved
# scheduler automatically picks up wave-2 stories alongside wave-1
```

Waves are **fully independent** — no cross-wave dependencies. If feature C depends on feature A, they should be in the same wave. The scheduler treats stories from all approved waves equally, sharing the global `max_parallel` limit.

### Guards and Validation

**Plan feedback is only allowed on draft waves:**
- The target wave's status must be `in_progress` (draft)
- Once `nflow plan approve` is called on a wave, feedback is rejected for that wave

**`nflow plan generate` requires a clean draft slot:**
- Fails if there is already an `in_progress` (draft) wave for this project — only one draft wave at a time
- Succeeds even if other waves are `approved` or executing
- Fails if there are no unassigned approved specs (all approved specs already belong to a wave)

**Adding specs to an existing wave is not supported.** Each wave is generated from a fixed set of specs. To include a new spec, create a new wave. This is intentional — partial plan updates within a wave would create complex dependency conflicts. The decompose step is fast (single Claude call), so creating a new wave is the simplest path.

**`nflow plan discard --wave <n>`:**
- Deletes all work_items from the target wave
- Sets the wave's status to `discarded`
- Moves associated specs back from `decomposed` to `approved` (freeing them for a future wave)
- Fails if the wave has stories with status `in_progress` (must stop them first)
- Can target any wave: draft or approved (as long as no in_progress stories)

---

## Phase 3: EXECUTE

### Purpose

Execute tasks by running Claude Code agents in isolated git worktrees. Each story gets a worktree, each task gets a commit, each story produces an MR.

### Scheduler Algorithm

The scheduler runs as a loop in the daemon:

```
loop every 2 seconds:
    1. Check for completed/failed agent processes
       - Update work_item and agent_run statuses
       - If story's all tasks done → create MR → mark story done
       - If task failed → mark story failed → escalate

    2. Propagate status changes
       - For each story with all blockers 'done' → set to 'ready'

    3. Count running agents
       - running = count of agent_runs with status 'running'

    4. If running < max_parallel:
       - Pick next 'ready' story (by sort_order) from any approved wave
         in projects with execution enabled
         (execution is enabled per-project by `nflow run` or automatically after
         `nflow plan approve` if `auto_execute = true` in config)
       - Stories from different waves are treated equally — max_parallel is global
       - Create worktree
       - Start first task of that story
       - Set story to 'in_progress'
```

### Story Execution Flow

```
Story picked by scheduler
        │
        ▼
git fetch origin {base_branch}
git worktree add ~/.nflow/worktrees/{project}/{branch} origin/{base_branch}
        │
        ▼
Task T1 (impl)   → claude agent → commit
        │
        ▼
Task T1v (verify) → claude agent → build + test + check criteria
        │                           │
        │                     pass? ─┤
        │                     yes    no → story FAILED, escalate to user
        ▼
Task T2 (impl)   → claude agent → commit
        │
        ▼
Task T2v (verify) → claude agent → build + test + check criteria
        │
        ▼
  ... repeat for all tasks ...
        │
        ▼
git push origin {branch}
        │
        ▼
gh pr create / glab mr create
        │
        ▼
Story status → done
        │
        ▼
git worktree remove (optional, configurable)
```

The impl → verify alternation ensures that every piece of work is independently tested before the next task starts. If a verify task fails, the story stops immediately — broken code does not accumulate.

### Task Agent Invocation

nflow uses different invocations for impl and verify tasks.

**Impl task:**
```bash
cd {worktree_path}

claude -p "{task_prompt}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file {task_context_file} \
  --allowedTools "Read,Write,Edit,Bash,Glob,Grep" \
  --max-turns 50
```

The `task_context_file` is generated from `task_execution.md` template and includes:
- Project description
- Epic and story context (including acceptance criteria at each level)
- Task description + acceptance criteria
- Summary of completed tasks in this story
- Instructions to commit with `[{wave_short_id}]` prefix (see "Impl Task Success Criteria")

Spec content is intentionally omitted — task descriptions must be self-sufficient.

**Verify task:**
```bash
cd {worktree_path}

claude -p "{verify_prompt}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file {verify_context_file} \
  --allowedTools "Read,Bash,Glob,Grep" \
  --max-turns 30
```

Key differences for verify tasks:
- **No Write or Edit tools** — the verify agent cannot modify code, only read and run commands
- Uses `verify_task.md` prompt template instead of `task_execution.md`
- Lower `max-turns` — verification should be quick
- The agent must run the build, run tests, and check each acceptance criterion
- If any check fails, the agent must exit with a clear error describing what failed
- The agent does NOT create commits

### Streaming Agent Output

nflow reads the `stream-json` output line by line:

```json
{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"Reading"}}}
{"type":"tool_use","name":"Read","input":{"file_path":"src/main.rs"}}
{"type":"tool_result","content":"..."}
{"type":"result","result":"Task completed. Created login endpoint.","session_id":"abc-123"}
```

nflow:
1. Parses each line
2. Writes raw output to log file (`agent-logs/{wave_short_id}.log`, e.g., `W1-T1.log`)
3. Extracts human-readable events (tool calls, text output)
4. Sends events to connected TUI/CLI clients via socket

### Impl Task Success Criteria

nflow checks three conditions after an impl agent exits:

1. **Exit code 0** — the agent process completed without error
2. **New commit exists** — HEAD changed (compare HEAD hash before and after the agent run)
3. **Commit message contains `[{wave_short_id}]`** — e.g., `[W1-T1] Create login API endpoint`

If all conditions are met: task status → `done`, commit hash is stored in the `commit_hash` column. If any condition fails: task status → `failed`.

The `[W{n}-{short_id}]` prefix enables exact mapping of commits to tasks across waves. `nflow continue` also uses these prefixes to match manual commits to tasks when recovering from failures.

### MR Creation

After all tasks in a story are done:

**GitHub:**
```bash
cd {worktree_path}
git push -u origin {branch_name}
gh pr create \
  --title "{story_title}" \
  --body "{generated_description}" \
  --base {base_branch}
```

**GitLab:**
```bash
cd {worktree_path}
git push -u origin {branch_name}
glab mr create \
  --title "{story_title}" \
  --description "{generated_description}" \
  --target-branch {base_branch}
```

The MR body is generated from the `mr_body.md` template:

```markdown
## {story_title}

{story_description}

### Tasks

{tasks_list}

---
Generated by [nflow](https://github.com/nolood/nflow)
```

Variables substituted by nflow:
- `{story_title}` — story title
- `{story_description}` — story description
- `{tasks_list}` — formatted list of tasks, each as `- {commit_hash} {task_title}` or `- [SKIPPED] {task_title}` for skipped tasks

The template can be overridden by placing a custom `mr_body.md` in `~/.nflow/prompts/`, following the same convention as other prompt templates.

### Rebase Before Push

Before pushing, nflow rebases the story branch onto the current `base_branch`:

```bash
cd {worktree_path}
git fetch origin {base_branch}
git rebase origin/{base_branch}
```

If the rebase fails due to conflicts:
1. Story status → `failed`
2. User is notified with the conflict details
3. User resolves conflicts manually in the worktree, then `nflow continue {story_id}`

This ensures MRs are up-to-date with the base branch, even if `main` has moved forward during execution.

### MR Creation Error Handling

If `gh pr create` / `glab mr create` fails (auth error, network, branch protection):
1. Story status → `failed`. All task statuses remain `done` — the work is complete, only delivery failed.
2. Error details recorded in the story's agent log
3. User is notified in TUI

Recovery:
- `nflow continue {story_id}` — sees all tasks are `done`, skips directly to rebase/push/MR creation. Effectively retries only the final delivery step.
- User can also create the MR manually from the worktree

### Error Handling

When a task agent fails (non-zero exit code or explicit failure):
1. Task status → `failed`
2. Story status → `failed`
3. Agent run recorded with error details
4. User notified in TUI (highlighted in red)
5. User can:
   - `nflow retry {task_id}` — re-run the failed task
   - `nflow skip {task_id}` — mark as done manually and continue
   - Fix manually in worktree, commit, then `nflow continue {story_id}`

### Verify Task Failure — Recommended Workflow

When a **verify** task fails, the implementation exists but didn't pass checks. The typical recovery flow:

1. Check the verify agent's output to understand what failed (build error, test failure, acceptance criteria not met)
2. Go to the story's worktree: `cd {worktree_path}`
3. Fix the code manually
4. Commit the fix: `git add ... && git commit -m "fix: ..."`
5. Re-run verification: `nflow retry {verify_task_id}`

The verify agent re-checks the current worktree state from scratch (build → tests → acceptance criteria).

Alternatively:
- `nflow skip {verify_task_id}` — skip verification and continue to the next impl task (use when you're confident the fix is correct)

### `nflow continue` Mechanics

When a user fixes issues manually in the worktree and runs `nflow continue {story_id}`:

1. **Determine completed tasks via stored commit hashes:**
   - Each impl task stores its `commit_hash` in the database on successful completion (see "Impl Task Success Criteria" below)
   - Tasks with a stored `commit_hash` → already `done`, skip them
   - The currently failed task → user called `continue`, so mark as `done` (assumes user fixed it)

2. **Resume execution:**
   - Story status is set back to `in_progress`
   - The next `pending` task starts executing
   - If no pending tasks remain, proceed to rebase/push/MR creation

No commit-counting heuristics. The `commit_hash` provides exact mapping of commits to tasks, even if the user made additional manual commits, amends, or rebases in the worktree.

### `nflow continue` for Cancelled Stories

When a user runs `nflow continue {story_id}` on a cancelled story:

**If worktree exists** (story was cancelled from `in_progress` via `nflow stop`):
1. Story status: `cancelled` → `in_progress`
2. All `cancelled` tasks in the story are reset to `pending`
3. Tasks with a stored `commit_hash` are marked `done` (+ their paired verify tasks)
4. Scheduler resumes from the first `pending` task

**If worktree does not exist** (story was cancelled from `pending` or `ready` via `nflow cancel`):
1. All `cancelled` tasks in the story are reset to `pending`
2. Story status: `cancelled` → `ready` (if all dependency stories are `done`) or `pending` (if dependencies are unmet)
3. No git log inspection — there is no worktree to inspect
4. The scheduler picks it up normally when it becomes `ready`, creates a worktree, and starts from the first task

### Branch Naming Convention

```
nflow/{project-name}/{story-short-id}-{story-slug}
```

Example: `nflow/myapp/s3-user-registration`

### Task Sizing Guidelines

Tasks must be sized with two constraints in balance:

**Small enough** for a single AI agent to complete reliably:
- One logical concern per task (e.g., one endpoint, one component, one migration)
- Roughly 1-3 files changed
- Roughly 1 commit worth of work
- Agent should not need to hold more than ~2000 lines of context

**Large enough** to be independently verifiable:
- After the task, something observable must work (an endpoint responds, a test passes, a component renders)
- The verify task must be able to run a meaningful check — not just "file exists"
- There must be a clear way to confirm the task's acceptance criteria via build + test

**Anti-patterns (too small):**
- "Create empty file src/auth.rs" — nothing to verify
- "Add import statement" — meaningless in isolation
- "Write type definition" — can't test without implementation

**Anti-patterns (too large):**
- "Implement entire authentication system" — too much for one agent
- "Create all API endpoints" — will exceed context window
- "Build the frontend" — too vague, too broad

**Good examples:**
- "Create user registration endpoint with validation" — testable: POST /register returns 201
- "Add password hashing with bcrypt" — testable: unit test for hash/verify
- "Create login form component with error display" — testable: component renders, shows errors

These guidelines are enforced via the decompose prompt (see prompts.md). Claude is instructed to follow them during decomposition.

### Parallelism Rules

- Stories from **different** dependency chains run in parallel (up to `max_parallel`)
- Stories from **different waves** run in parallel (independent batches of work)
- Tasks within a **single story** run sequentially (shared worktree, impl→verify alternation)
- Stories from **different projects** can run in parallel (separate worktrees, separate repos)
- `max_parallel` is a **global** limit shared across all waves and projects

### Parallel Story Conflicts

nflow does **not** attempt to prevent or minimize file-level merge conflicts between parallel stories. The decomposition creates `depends_on` for logical dependencies; file-level conflicts are rare if stories are well-decomposed (touching different areas of the codebase).

When conflicts occur, the existing flow handles it: rebase fails before push → story `failed` → user resolves conflicts in worktree → `nflow continue`.
