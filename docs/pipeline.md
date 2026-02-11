# nflow — Pipeline Flow

## Overview

Pipeline Flow is a simplified alternative to nflow's full SDD (SPEC → DECOMPOSE → EXECUTE) workflow. It provides a streamlined 3-stage process (Plan → Implement → Review) for rapid development without the overhead of formal specification and decomposition.

**Key characteristics:**
- Runs in-place (no git worktrees or branches)
- Sequential stage execution with loop-back on review failures
- All stage outputs saved to database for debugging
- Only one active pipeline per project
- Self-contained task execution (no scheduler integration)

**When to use Pipeline Flow:**
- Rapid prototyping or exploration
- Small features that don't warrant full SDD workflow
- Bug fixes or refactoring tasks
- When you want quick feedback without formal planning

**When to use SDD Flow:**
- Complex features requiring decomposition into parallel stories
- Work that needs formal specification and stakeholder approval
- Features that benefit from isolated git branches per story
- Projects requiring structured merge request workflow

---

## Architecture

### Components

**Core types:** `crates/nflow-core/src/pipeline.rs`
- `PipelineRun` — pipeline execution metadata and state
- `PipelineStage` — individual stage (Plan/Implement/Review) execution record
- `PipelineState` — state machine (Running/Completed/Failed/Cancelled)
- `StageType` — enum for Plan/Implement/Review
- `StageOutput` — structured JSON output from each stage

**Database:**
- `migrations/002_pipeline.sql` — schema definition
- `pipeline_runs` table — pipeline metadata and current state
- `pipeline_stages` table — stage execution history with outputs
- Reuses `agent_runs` table for PID/session tracking

**DB access layer:** `crates/nflow-daemon/src/db/pipeline.rs`
- `create_pipeline_run()` — initialize new pipeline
- `get_pipeline_run()` — fetch pipeline by ID
- `list_pipeline_runs()` — list all pipelines for project
- `update_pipeline_state()` — state transitions
- `insert_stage()` — record stage execution
- `get_stages()` — retrieve stage history

**Prompts:** Embedded in `prompts/` directory
- `pipeline_plan.md` — Plan stage instructions
- `pipeline_implement.md` — Implementation stage instructions
- `pipeline_review.md` — Review stage instructions

**Daemon integration:**
- `events.rs` — 3 new event types for real-time updates
- `handlers.rs` — 5 command handlers for pipeline operations

**TUI:** `crates/nflow-tui/`
- Pipeline tab (5th tab, key `5`)
- List view, detail view, and new pipeline dialog

### Data Flow

```
User: pipeline.start
        │
        ▼
Daemon spawns tokio task
        │
        ▼
┌─────────────────┐
│  Plan Stage     │ ← Claude: analyze task, create plan
├─────────────────┤
│ Output:         │
│ - approach      │
│ - risks         │
│ - files         │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Implement Stage │ ← Claude: execute plan
├─────────────────┤
│ Output:         │
│ - changes_made  │
│ - issues_found  │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  Review Stage   │ ← Claude: verify implementation
├─────────────────┤
│ Output:         │
│ - issues        │
│ - approved      │
└────────┬────────┘
         │
    ┌────┴────┐
    │approved?│
    └─┬────┬──┘
      │    │
     YES   NO
      │    │
      │    └──► Back to Implement Stage (with feedback)
      │         │
      │         ▼
      │    Iteration N (max: 5, configurable)
      │         │
      │         └──► Eventually fails or succeeds
      │
      ▼
Pipeline Completed
```

---

## State Machine

### PipelineState Transitions

```
Idle
  │
  │ pipeline.start
  ▼
Running ──────────────────┐
  │                       │
  │ stage completes       │ pipeline.cancel
  │ (Plan/Impl/Review)    │
  │                       │
  ├─► Next stage          ▼
  │                   Cancelled
  │
  │ all stages done + approved=true
  ▼
Completed
  │
  │ review found issues + iterations < max
  │
  └─► Loop back to Implement (still Running)
```

### Stage Execution States

Each stage (Plan/Implement/Review) is executed as a separate Claude Code session:
- `in_progress` — agent is running
- `completed` — agent exited successfully, output saved
- `failed` — agent exited with error or invalid output

---

## Commands

### pipeline.start

Start a new pipeline run.

**Request:**
```json
{
  "id": "req-123",
  "command": "pipeline.start",
  "params": {
    "project_id": "uuid",
    "description": "Add user profile page with avatar upload",
    "max_iterations": 5
  }
}
```

**Parameters:**
- `project_id` (string) — project UUID
- `description` (string) — task description for the pipeline
- `max_iterations` (number, optional) — max Implement→Review loops (default: 5)

**Response:**
Streaming response with events:
```json
{"id": "req-123", "done": false, "event": {"type": "PipelineStageChange", "stage": "Plan", "state": "in_progress"}}
{"id": "req-123", "done": false, "event": {"type": "PipelineAgentOutput", "text": "Analyzing requirements..."}}
{"id": "req-123", "done": false, "event": {"type": "PipelineStageChange", "stage": "Plan", "state": "completed"}}
{"id": "req-123", "done": false, "event": {"type": "PipelineStageChange", "stage": "Implement", "state": "in_progress"}}
...
{"id": "req-123", "done": true, "data": {"pipeline_id": "uuid", "state": "Completed"}}
```

**Errors:**
- `INVALID_STATE` — another pipeline is already running for this project

### pipeline.status

Get current pipeline status and metadata.

**Request:**
```json
{
  "id": "req-124",
  "command": "pipeline.status",
  "params": {
    "pipeline_id": "uuid"
  }
}
```

**Response:**
```json
{
  "id": "req-124",
  "status": "ok",
  "data": {
    "id": "uuid",
    "project_id": "uuid",
    "description": "Add user profile page",
    "state": "Running",
    "current_iteration": 2,
    "max_iterations": 5,
    "created_at": "2026-02-10T12:34:56Z",
    "updated_at": "2026-02-10T12:45:20Z"
  }
}
```

### pipeline.list

List all pipeline runs for a project.

**Request:**
```json
{
  "id": "req-125",
  "command": "pipeline.list",
  "params": {
    "project_id": "uuid"
  }
}
```

**Response:**
```json
{
  "id": "req-125",
  "status": "ok",
  "data": {
    "pipelines": [
      {
        "id": "uuid-1",
        "description": "Add user profile page",
        "state": "Completed",
        "created_at": "2026-02-10T12:34:56Z"
      },
      {
        "id": "uuid-2",
        "description": "Fix login redirect",
        "state": "Running",
        "created_at": "2026-02-10T13:00:00Z"
      }
    ]
  }
}
```

### pipeline.cancel

Cancel a running pipeline.

**Request:**
```json
{
  "id": "req-126",
  "command": "pipeline.cancel",
  "params": {
    "pipeline_id": "uuid"
  }
}
```

**Response:**
```json
{
  "id": "req-126",
  "status": "ok",
  "data": {
    "cancelled": true
  }
}
```

**Errors:**
- `NOT_FOUND` — pipeline does not exist
- `INVALID_STATE` — pipeline is not in Running state

### pipeline.log

Stream stage execution logs.

**Request:**
```json
{
  "id": "req-127",
  "command": "pipeline.log",
  "params": {
    "pipeline_id": "uuid",
    "stage_id": "uuid"
  }
}
```

**Response:**
Streaming response with raw agent output:
```json
{"id": "req-127", "done": false, "data": {"line": "Reading src/profile.rs..."}}
{"id": "req-127", "done": false, "data": {"line": "Writing src/profile.rs..."}}
{"id": "req-127", "done": true}
```

---

## Stage Details

### Plan Stage

**Purpose:** Analyze the task and create an implementation plan.

**Claude invocation:**
```bash
claude -p "{description}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file prompts/pipeline_plan.md \
  --allowedTools "Read,Glob,Grep" \
  --max-turns 20
```

**Working directory:** project root (in-place, not a worktree)

**Expected output format:**
```json
{
  "approach": "Create ProfilePage component with avatar upload using FileInput",
  "risks": ["Avatar upload size validation", "S3 integration might need credentials"],
  "files_to_modify": ["src/components/ProfilePage.tsx", "src/api/profile.ts"],
  "files_to_create": ["src/components/AvatarUpload.tsx"],
  "tests_needed": ["ProfilePage renders correctly", "Avatar upload flow"]
}
```

**Success criteria:**
- Exit code 0
- Output contains valid JSON matching expected schema
- JSON extracted from final `result` field or from assistant message

**On failure:**
- Pipeline state → Failed
- User can inspect logs and retry with a new pipeline

### Implement Stage

**Purpose:** Execute the plan and make code changes.

**Claude invocation:**
```bash
claude -p "{implementation_prompt}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file prompts/pipeline_implement.md \
  --allowedTools "Read,Write,Edit,Bash,Glob,Grep" \
  --max-turns 50
```

**Context passed to Claude:**
- Original task description
- Plan output (approach, files, risks)
- Previous review feedback (if iteration > 1)
- All previous iteration outputs (for context accumulation)

**Expected output format:**
```json
{
  "changes_made": [
    "Created AvatarUpload component with drag-and-drop support",
    "Added file size validation (max 5MB)",
    "Integrated with existing API client"
  ],
  "issues_found": ["Needed to add CORS config for S3 endpoint"]
}
```

**Success criteria:**
- Exit code 0
- Output contains valid JSON
- Changes are made to the codebase (verified by checking `git status`)

**On failure:**
- Pipeline state → Failed
- No automatic retry (user must start new pipeline)

### Review Stage

**Purpose:** Verify the implementation meets requirements.

**Claude invocation:**
```bash
claude -p "{review_prompt}" \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --append-system-prompt-file prompts/pipeline_review.md \
  --allowedTools "Read,Bash,Glob,Grep" \
  --max-turns 30
```

**Note:** No Write/Edit tools — review agent is read-only.

**Context passed to Claude:**
- Original task description
- Plan output
- Implementation output (changes made, issues found)
- Instruction to run build, tests, and verify acceptance criteria

**Expected output format:**
```json
{
  "approved": false,
  "issues": [
    "Avatar upload doesn't show preview before upload",
    "Missing error handling for network failures"
  ],
  "build_status": "success",
  "tests_status": "passed"
}
```

**Decision logic:**
- If `approved: true` → Pipeline Completed
- If `approved: false` and `iteration < max_iterations`:
  - Loop back to Implement stage
  - Pass `issues` as feedback to next implementation
  - Increment `current_iteration`
- If `approved: false` and `iteration >= max_iterations`:
  - Pipeline Failed (max iterations exceeded)

**Success criteria:**
- Exit code 0
- Output contains valid JSON with `approved` boolean

---

## Context Accumulation

Each iteration of the Implement→Review loop accumulates context from previous iterations. This enables the implementation agent to learn from past mistakes without losing history.

**Example:** Pipeline with 3 iterations

**Iteration 1 — Implement:**
- Input: Plan + original description
- Output: changes_made, issues_found

**Iteration 1 — Review:**
- Input: Plan + Implement output
- Output: approved=false, issues=[...]

**Iteration 2 — Implement:**
- Input: Plan + original description + **Iteration 1 outputs** + **Review issues as feedback**
- Output: changes_made (addressing review issues), issues_found

**Iteration 2 — Review:**
- Input: Plan + **All Implement outputs** (iteration 1 + 2)
- Output: approved=true

**Result:** Pipeline Completed

---

## Events

Pipeline execution emits events via the daemon's event bus. Clients (TUI/CLI) subscribe to receive real-time updates.

### PipelineStageChange

Emitted when a stage starts or completes.

```json
{
  "type": "PipelineStageChange",
  "pipeline_id": "uuid",
  "stage": "Plan",
  "state": "in_progress"
}
```

**Fields:**
- `stage` — "Plan", "Implement", or "Review"
- `state` — "in_progress", "completed", "failed"

### PipelineCompleted

Emitted when the entire pipeline finishes (success or failure).

```json
{
  "type": "PipelineCompleted",
  "pipeline_id": "uuid",
  "final_state": "Completed",
  "iterations": 3
}
```

**Fields:**
- `final_state` — "Completed", "Failed", or "Cancelled"
- `iterations` — number of Implement→Review loops executed

### PipelineAgentOutput

Emitted for real-time agent output streaming.

```json
{
  "type": "PipelineAgentOutput",
  "pipeline_id": "uuid",
  "text": "Reading src/profile.rs..."
}
```

---

## TUI Integration

### Pipeline Tab (Key: 5)

The TUI adds a 5th tab for pipeline management, accessible via the `5` key.

**Views:**
1. **List view** — shows all pipelines for the current project
2. **Detail view** — shows stage history and outputs for a selected pipeline
3. **New dialog** — form to create a new pipeline

### List View

```
┌─ Pipeline ───────────────────────────────────────────────┐
│                                                          │
│  Description                State        Created         │
│  ──────────────────────────────────────────────          │
│  Add user profile page      Completed    Feb 10 12:34    │
│  Fix login redirect         Running      Feb 10 13:00    │
│  Refactor auth module       Failed       Feb 09 16:20    │
│                                                          │
│  [n]ew  [Enter] detail  [c]ancel  [l]og                 │
└──────────────────────────────────────────────────────────┘
```

**Actions:**
- `n` — open new pipeline dialog
- `Enter` — view pipeline detail
- `c` — cancel running pipeline
- `l` — stream logs for selected stage

### Detail View

```
┌─ Pipeline: Add user profile page ────────────────────────┐
│                                                          │
│  State: Running                 Iteration: 2/5          │
│                                                          │
│  Stages:                                                 │
│  ├─ Plan        [completed]  12:34:56                   │
│  │  Output: Approach: Create ProfilePage component...   │
│  │                                                       │
│  ├─ Implement   [completed]  12:36:10 (iter 1)         │
│  │  Output: Created component, added upload...          │
│  │                                                       │
│  ├─ Review      [completed]  12:38:45 (iter 1)         │
│  │  Output: Issues: Missing error handling...           │
│  │                                                       │
│  ├─ Implement   [completed]  12:40:20 (iter 2)         │
│  │  Output: Added error handling, preview...            │
│  │                                                       │
│  └─ Review      [in_progress]  12:42:30 (iter 2)       │
│     Output: Running tests...                            │
│                                                          │
│  [Esc] back  [l]og  [c]ancel                            │
└──────────────────────────────────────────────────────────┘
```

### New Pipeline Dialog

```
┌─ New Pipeline ───────────────────────────────────────────┐
│                                                          │
│  Description:                                            │
│  > Add user profile page with avatar upload__           │
│                                                          │
│  Max Iterations: [5__]                                  │
│                                                          │
│  [Enter] start  [Esc] cancel  [Tab] next field          │
└──────────────────────────────────────────────────────────┘
```

**Fields:**
- `Description` — text input (required)
- `Max Iterations` — number input (default: 5)

**Navigation:**
- `Tab` — cycle between fields
- `Enter` — submit and start pipeline
- `Esc` — cancel and return to list view

---

## Comparison: Pipeline vs SDD

| Feature | Pipeline Flow | SDD Flow |
|---------|---------------|----------|
| **Stages** | Plan → Implement → Review | SPEC → DECOMPOSE → EXECUTE |
| **Git isolation** | In-place (no branches) | Git worktrees per story |
| **Parallelism** | Sequential only | Parallel stories (up to `max_parallel`) |
| **Formal planning** | No (just Plan stage) | Yes (decomposed DAG) |
| **Merge requests** | Manual (no auto-MR) | Auto-created per story |
| **Verification** | Review stage (holistic) | Verify task (per impl task) |
| **Iteration** | Loop-back on review failure | Manual retry per task |
| **Use case** | Rapid prototyping, small features | Complex features, structured workflow |
| **State persistence** | All outputs saved to DB | Commit hashes, MR links |

---

## Implementation Notes

### No Scheduler Integration

Pipeline execution does NOT use the existing `scheduler_loop` from the SDD flow. It's a self-contained tokio task spawned per pipeline run. This design decision avoids complexity and state conflicts with the scheduler.

**Rationale:**
- Scheduler manages parallel story execution with dependencies
- Pipeline is purely sequential (no dependencies, no parallelism)
- Sharing the scheduler would require special-casing pipeline tasks
- Separate task is simpler and more maintainable

### Session Resumption

Pipeline stages use Claude's `--resume` feature if a stage fails mid-execution. The daemon stores the `session_id` from each stage's final `result` event.

**Recovery flow:**
1. Stage fails (non-zero exit, network error, etc.)
2. Pipeline state → Failed
3. User can inspect logs via `pipeline.log`
4. User starts a new pipeline (no automatic retry within same pipeline)

**Future enhancement:** Add `pipeline.retry` command to resume from failed stage using stored `session_id`.

### Output Parsing

Stage outputs are extracted from Claude's `stream-json` response. nflow looks for JSON in two places:

1. **Final `result` field** — preferred, structured response
2. **Last assistant message** — fallback, parses JSON code block

**Example result field:**
```json
{
  "type": "result",
  "result": "{\"approach\": \"...\", \"risks\": [...]}",
  "session_id": "abc-123"
}
```

**Example assistant message:**
```json
{
  "type": "stream_event",
  "event": {
    "delta": {
      "type": "text_delta",
      "text": "```json\n{\"approach\": \"...\"}\n```"
    }
  }
}
```

If both fail to produce valid JSON, the stage fails and pipeline state → Failed.

---

## Prompt Templates

### pipeline_plan.md

Located at: `prompts/pipeline_plan.md`

**Purpose:** Instruct Claude to analyze the task and produce a structured plan.

**Key instructions:**
- Use Read/Glob/Grep to explore the codebase
- Identify files to modify/create
- List potential risks
- Output JSON with `approach`, `risks`, `files_to_modify`, `files_to_create`, `tests_needed`

**Variables:**
- `{description}` — user-provided task description

### pipeline_implement.md

Located at: `prompts/pipeline_implement.md`

**Purpose:** Instruct Claude to execute the plan and make code changes.

**Key instructions:**
- Follow the plan from Plan stage
- Use Write/Edit/Bash to make changes
- Address previous review feedback (if iteration > 1)
- Output JSON with `changes_made`, `issues_found`

**Variables:**
- `{description}` — original task description
- `{plan}` — JSON output from Plan stage
- `{previous_feedback}` — issues from previous Review stage (if iteration > 1)
- `{previous_iterations}` — all previous Implement/Review outputs

### pipeline_review.md

Located at: `prompts/pipeline_review.md`

**Purpose:** Instruct Claude to verify the implementation.

**Key instructions:**
- Read the changes made
- Run build and tests using Bash
- Check acceptance criteria from original description
- Output JSON with `approved` (boolean), `issues` (array), `build_status`, `tests_status`

**Variables:**
- `{description}` — original task description
- `{plan}` — JSON output from Plan stage
- `{implementation}` — JSON output from Implement stage

---

## Database Schema

### pipeline_runs

```sql
CREATE TABLE pipeline_runs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    description TEXT NOT NULL,
    state TEXT NOT NULL,  -- Running, Completed, Failed, Cancelled
    current_iteration INTEGER NOT NULL DEFAULT 1,
    max_iterations INTEGER NOT NULL DEFAULT 5,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);
```

### pipeline_stages

```sql
CREATE TABLE pipeline_stages (
    id TEXT PRIMARY KEY,
    pipeline_id TEXT NOT NULL,
    stage_type TEXT NOT NULL,  -- Plan, Implement, Review
    iteration INTEGER NOT NULL,
    state TEXT NOT NULL,  -- in_progress, completed, failed
    output_json TEXT,  -- Structured JSON output from the stage
    agent_run_id TEXT,  -- Links to agent_runs for PID tracking
    created_at TEXT NOT NULL,
    completed_at TEXT,
    FOREIGN KEY (pipeline_id) REFERENCES pipeline_runs(id) ON DELETE CASCADE,
    FOREIGN KEY (agent_run_id) REFERENCES agent_runs(id)
);
```

**Indexes:**
```sql
CREATE INDEX idx_pipeline_stages_pipeline_id ON pipeline_stages(pipeline_id);
CREATE INDEX idx_pipeline_runs_project_id ON pipeline_runs(project_id);
CREATE INDEX idx_pipeline_runs_state ON pipeline_runs(state);
```

---

## Future Enhancements

### Planned Features

1. **Resume from failure** — `pipeline.retry` command to resume failed stage using stored `session_id`
2. **Parallel pipelines** — allow multiple pipelines per project (requires resource contention handling)
3. **Custom stage templates** — allow users to define custom stage prompts in `~/.nflow/prompts/`
4. **Pipeline chaining** — trigger new pipeline on completion of previous pipeline
5. **Git integration** — optional branching (hybrid mode: pipeline flow with git isolation)
6. **Metrics tracking** — duration, token usage, iteration count analytics

### Experimental Ideas

- **Interactive mode** — pause between stages for user approval
- **Partial rollback** — undo changes from last iteration
- **Stage dependencies** — custom stage graph (not just Plan→Implement→Review)
- **Multi-agent review** — parallel review from different perspectives

---

## Troubleshooting

### Pipeline stuck in Running state

**Symptoms:** TUI shows pipeline as Running, but no agent output.

**Diagnosis:**
1. Check daemon logs: `tail -f ~/.nflow/logs/daemon.log`
2. Check for zombie claude processes: `ps aux | grep claude`
3. Inspect agent_run record: `pipeline.status` shows last agent PID

**Resolution:**
- Kill zombie claude process: `kill -9 <PID>`
- Daemon will detect process exit and update pipeline state
- Start new pipeline (stuck pipeline cannot be resumed)

### Stage output invalid JSON

**Symptoms:** Pipeline fails with "Failed to parse stage output" error.

**Diagnosis:**
1. View raw stage logs: `pipeline.log --pipeline-id <id> --stage-id <id>`
2. Check if Claude returned JSON (vs plain text)
3. Look for truncated output (token limits)

**Resolution:**
- Adjust prompt to emphasize JSON output format
- Increase `max-turns` in stage invocation
- Simplify task description (reduce complexity)

### Max iterations exceeded

**Symptoms:** Pipeline fails after N Implement→Review loops (default: 5).

**Diagnosis:**
- Review Review stage outputs to see recurring issues
- Check if implementation is actually addressing feedback

**Resolution:**
- Start new pipeline with clearer description
- Increase `max_iterations` parameter
- Break task into smaller sub-tasks (multiple pipelines)

### Pipeline conflicts with SDD execution

**Symptoms:** Pipeline makes changes that conflict with running stories.

**Diagnosis:**
- Check if project has active stories: View Execution tab in TUI
- Pipeline runs in-place, stories run in worktrees — should not conflict

**Resolution:**
- Pipeline and SDD can coexist (different file locations)
- If conflicts occur, pause execution: `nflow stop --all`
- Run pipeline, then resume execution: `nflow run`

---

## See Also

- [Architecture](./architecture.md) — Overall system design
- [Phases](./phases.md) — SDD flow (SPEC → DECOMPOSE → EXECUTE)
- [TUI](./tui.md) — TUI navigation and views
- [Daemon](./daemon.md) — Daemon lifecycle and protocol
- [Prompts](./prompts.md) — Prompt template system
