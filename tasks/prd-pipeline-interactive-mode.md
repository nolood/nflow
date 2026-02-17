# PRD: Pipeline Interactive Mode — Manual/Auto Modes, Real-Time TUI, CLI Commands

## Introduction

Enhance the Pipeline flow (tab 5 in TUI) to support two execution modes: **Manual** (human-in-the-loop) and **Automatic** (fully autonomous). In Manual mode, Claude asks planning questions that the user answers, the plan requires user approval before proceeding, and the user gives final sign-off after all stages complete. In Automatic mode, a separate "auto-answerer" Claude agent receives Claude's planning questions, searches the codebase for answers, and provides them — removing the need for human involvement. Both modes stream real-time agent output to TUI and CLI, and the Implement stage agent must build and run tests before completing.

Additionally, all Pipeline functionality must be available via CLI commands (currently missing entirely), and the pipeline agent must actually compile the project and run tests during execution.

## Goals

- Support Manual and Automatic pipeline execution modes with a single `mode` parameter
- In Manual mode: user answers planning questions, approves the plan, and gives final approval after all stages
- In Automatic mode: a separate Claude agent auto-answers planning questions by searching the codebase
- Real-time streaming of agent output to both TUI and CLI during pipeline execution
- Full pipeline management via CLI commands (`nflow pipeline start/status/list/cancel/approve/reject/log`)
- Implement stage agent must run `cargo build` / `cargo test` (or project-appropriate build/test commands) and verify they pass before completing
- User can reject final result in Manual mode with feedback, triggering a new iteration cycle with summary of previous attempt

## User Stories

### US-001: Add `mode` field to PipelineRun and database schema
**Description:** As a developer, I need to store the pipeline execution mode (manual/auto) so the daemon knows which flow to execute.

**Acceptance Criteria:**
- [ ] Add `mode` field to `PipelineRun` struct in `nflow-core/src/pipeline.rs`: enum `PipelineMode { Manual, Auto }`
- [ ] Add `mode TEXT NOT NULL DEFAULT 'auto'` column to `pipeline_runs` table via new migration `004_pipeline_mode.sql`
- [ ] Update `db/pipeline.rs` — `insert_pipeline_run`, `row_to_pipeline_run` to handle `mode` field
- [ ] Update `PipelineRun::new()` to accept `mode` parameter
- [ ] Existing tests pass, new unit tests for mode serialization/deserialization
- [ ] `cargo build --workspace` succeeds

### US-002: Add pipeline question/answer protocol to events and core types
**Description:** As a developer, I need core types and events to represent planning questions from Claude and answers from the user or auto-agent, so the system can mediate the Q&A flow.

**Acceptance Criteria:**
- [ ] Add `PipelineQuestion` struct to `nflow-core/src/pipeline.rs`: `{ id: Uuid, pipeline_run_id: Uuid, question: String, context: Option<String>, answered: bool, answer: Option<String>, answered_by: Option<String> /* "user" | "auto-agent" */, created_at, answered_at }`
- [ ] Add new event variant `Event::PipelineQuestion { pipeline_run_id, question_id, question, context }` in `events.rs`
- [ ] Add new event variant `Event::PipelineQuestionAnswered { pipeline_run_id, question_id, answer, answered_by }` in `events.rs`
- [ ] Add `pipeline_questions` table in migration: `id, pipeline_run_id, question, context, answered, answer, answered_by, created_at, answered_at`
- [ ] Add DB functions: `insert_question`, `get_pending_questions`, `answer_question`
- [ ] Unit tests for new types and serialization

### US-003: Add plan approval protocol to events and core types
**Description:** As a developer, I need types and events to represent plan approval/rejection so the pipeline can pause and wait for user decision in manual mode.

**Acceptance Criteria:**
- [ ] Add `PipelineStatus::WaitingForApproval` variant to `PipelineStatus` enum
- [ ] Add new event variant `Event::PipelinePlanReady { pipeline_run_id, project_id, plan_summary }` in `events.rs`
- [ ] Add new event variant `Event::PipelinePlanApproved { pipeline_run_id }` in `events.rs`
- [ ] Add new event variant `Event::PipelinePlanRejected { pipeline_run_id, feedback }` in `events.rs`
- [ ] Add `PipelineStatus::WaitingForFinalApproval` variant for post-review user approval
- [ ] Add `Event::PipelineFinalApprovalReady { pipeline_run_id, project_id, summary }` event
- [ ] Add `Event::PipelineFinalApproved { pipeline_run_id }` event
- [ ] Add `Event::PipelineFinalRejected { pipeline_run_id, feedback }` event
- [ ] Update `pipeline_status_to_str` / `pipeline_status_from_str` in `db/pipeline.rs`
- [ ] Update `next_action()` state machine: after Review passes in Manual mode, go to `WaitingForFinalApproval` instead of `Complete`
- [ ] Unit tests for new states and transitions

### US-004: Implement Manual mode planning Q&A flow in daemon
**Description:** As a user running a pipeline in manual mode, I want Claude to ask me questions during the Plan stage so I can provide guidance, and then I approve the plan before implementation starts.

**Acceptance Criteria:**
- [ ] Daemon's pipeline executor detects questions from Plan agent's stream-json output (Claude uses a structured question format in the prompt)
- [ ] When a question is detected: save to DB, emit `PipelineQuestion` event, pause agent execution (or let it continue and collect questions batch-style)
- [ ] Implement `pipeline.answer` command handler: receives `{ pipeline_run_id, question_id, answer }`, saves to DB, emits `PipelineQuestionAnswered`, resumes agent with answer
- [ ] After Plan stage completes in Manual mode: set pipeline status to `WaitingForApproval`, emit `PipelinePlanReady` event
- [ ] Implement `pipeline.approve` command handler: if pipeline is in `WaitingForApproval`, transition to next stage
- [ ] Implement `pipeline.reject` command handler: if pipeline is in `WaitingForApproval`, mark as failed or restart Plan with feedback
- [ ] Update `pipeline_plan.md` prompt to instruct Claude to output questions in a structured format (JSON array) before producing the plan
- [ ] Integration tests: manual mode pipeline pauses at plan, resumes on approve

### US-005: Implement Auto mode with auto-answerer agent
**Description:** As a user running a pipeline in automatic mode, I want a separate Claude agent to automatically answer planning questions by searching the codebase, so the pipeline runs without my intervention.

**Acceptance Criteria:**
- [ ] When Plan agent outputs a question in auto mode: daemon spawns a separate Claude Code process ("auto-answerer agent")
- [ ] Auto-answerer agent receives: the question text, project root path, and read-only tool access (`Read, Glob, Grep`)
- [ ] Auto-answerer agent prompt (new `prompts/pipeline_auto_answer.md`): search the codebase for relevant information, if found — provide a concrete answer, if not found — recommend a sensible default with rationale
- [ ] Auto-answerer response is fed back into the Plan agent as the answer
- [ ] Emit `PipelineQuestionAnswered` event with `answered_by: "auto-agent"`
- [ ] Auto-answerer has max-turns limit (e.g., 10) and timeout (e.g., 60s) to prevent runaway
- [ ] If auto-answerer fails, use a fallback default answer and log warning
- [ ] New `agent_runs` record created for auto-answerer with type tracking
- [ ] Unit tests and integration tests for auto-answer flow

### US-006: Implement final approval flow in Manual mode
**Description:** As a user in manual mode, after all pipeline stages complete successfully, I want to review the result and either approve it or send it back for another iteration with my feedback.

**Acceptance Criteria:**
- [ ] After Review stage passes (approved=true) in Manual mode: set status to `WaitingForFinalApproval`, emit `PipelineFinalApprovalReady` event with summary of all changes
- [ ] Implement `pipeline.final-approve` command handler: transitions pipeline to `Completed`
- [ ] Implement `pipeline.final-reject` command handler: receives user feedback, increments iteration, transitions back to Implement stage with combined feedback (review + user), emits `PipelineStageChange`
- [ ] The feedback from user rejection is included in the next Implement agent's context as `user_feedback` alongside the review feedback
- [ ] If max_iterations reached after user rejection: mark as Failed
- [ ] Pipeline detail view shows "Waiting for your approval" state with user's options
- [ ] Integration test: reject triggers new iteration with accumulated context

### US-007: Real-time agent output streaming in TUI pipeline detail view
**Description:** As a user, I want to see live agent output in the TUI when viewing a running pipeline, so I can monitor what Claude is doing in real-time.

**Acceptance Criteria:**
- [ ] Pipeline detail view shows scrollable live output pane when a stage is `running`
- [ ] Output comes from `PipelineAgentOutput` events via the event bus subscription
- [ ] Output pane auto-scrolls to bottom (latest output) by default
- [ ] User can scroll up with `k`/arrow-up to read earlier output; auto-scroll resumes when scrolled to bottom
- [ ] Output pane is limited to last 1000 lines in memory (ring buffer) to prevent OOM
- [ ] Stage status indicators update in real-time (spinner for running, checkmark for completed, X for failed)
- [ ] When switching between pipeline runs, output buffer resets to the selected run's output
- [ ] `cargo build --workspace` succeeds, TUI renders without panics

### US-008: Pipeline question/answer UI in TUI
**Description:** As a user in manual mode, I want to see questions from Claude in the TUI and type my answers inline, so I can participate in the planning process.

**Acceptance Criteria:**
- [ ] When `PipelineQuestion` event is received for the current pipeline: show a question dialog/overlay in the pipeline detail view
- [ ] Question dialog shows: question text, optional context, and a text input field for the answer
- [ ] User types answer and presses Enter to submit — sends `pipeline.answer` command to daemon
- [ ] Multiple pending questions are queued and shown one at a time
- [ ] Question count badge shown in pipeline list for pipelines waiting for answers
- [ ] After all questions answered, pipeline continues automatically
- [ ] Esc dismisses the question temporarily (can come back to it)

### US-009: Plan approval UI in TUI
**Description:** As a user in manual mode, I want to see the plan output and approve or reject it in the TUI.

**Acceptance Criteria:**
- [ ] When pipeline enters `WaitingForApproval` state: show plan output in the detail view with approve/reject controls
- [ ] Plan output rendered as formatted text (markdown rendering) in a scrollable pane
- [ ] Keybindings: `a` to approve, `r` to reject (opens text input for rejection feedback)
- [ ] Approve sends `pipeline.approve`, reject sends `pipeline.reject` with feedback text
- [ ] Status bar shows "Awaiting plan approval" with colored indicator
- [ ] After approval, pipeline transitions to Implement and TUI shows the transition

### US-010: Final approval UI in TUI
**Description:** As a user in manual mode, after all stages complete, I want to review the summary and approve or reject the final result in the TUI.

**Acceptance Criteria:**
- [ ] When pipeline enters `WaitingForFinalApproval`: show summary of all changes (from all Implement stages) + review results
- [ ] Scrollable summary view with all iteration outputs
- [ ] Keybindings: `a` to approve (completes pipeline), `r` to reject (text input for feedback, triggers new iteration)
- [ ] Rejection feedback clearly shown in the next iteration's context
- [ ] Status bar shows "Awaiting final approval"

### US-011: Add `mode` selector to TUI new pipeline dialog
**Description:** As a user creating a new pipeline in TUI, I want to choose between Manual and Automatic mode.

**Acceptance Criteria:**
- [ ] New pipeline dialog has a third field: Mode selector (Manual / Auto)
- [ ] Tab cycles through: Description → Max Iterations → Mode → (back to Description)
- [ ] Mode defaults to Auto
- [ ] Left/Right arrow keys or Tab toggle between Manual and Auto
- [ ] Selected mode passed in `pipeline.start` command params
- [ ] Mode is displayed in the pipeline list view as a column or badge

### US-012: CLI pipeline commands — full CRUD
**Description:** As a user, I want to manage pipelines entirely from the CLI without needing the TUI.

**Acceptance Criteria:**
- [ ] `nflow pipeline start "<description>" [--mode manual|auto] [--max-iterations N]` — starts a new pipeline, streams output to stdout
- [ ] `nflow pipeline list` — lists all pipelines for the current project (table format: ID, Description, Mode, State, Iteration, Created)
- [ ] `nflow pipeline status <pipeline-id>` — shows pipeline details + stage history
- [ ] `nflow pipeline cancel <pipeline-id>` — cancels a running pipeline
- [ ] `nflow pipeline log <pipeline-id> [--stage plan|implement|review] [--iteration N]` — streams or shows stage logs
- [ ] All commands use the existing daemon socket protocol
- [ ] Commands added to `nflow-cli/src/cli.rs` as a `Pipeline` subcommand group
- [ ] `cargo build --workspace` succeeds
- [ ] Actually build the binary and run each command to verify it works

### US-013: CLI pipeline approval commands
**Description:** As a user in manual mode, I want to approve/reject plans and final results from the CLI.

**Acceptance Criteria:**
- [ ] `nflow pipeline approve <pipeline-id>` — approves plan or final result (context-aware: if `WaitingForApproval` approves plan, if `WaitingForFinalApproval` approves final)
- [ ] `nflow pipeline reject <pipeline-id> "<feedback>"` — rejects with feedback message
- [ ] `nflow pipeline answer <pipeline-id> <question-id> "<answer>"` — answers a planning question
- [ ] `nflow pipeline questions <pipeline-id>` — lists pending questions for a pipeline
- [ ] Streaming mode: `nflow pipeline start` in manual mode shows questions inline and prompts for answers interactively (stdin)
- [ ] CLI prints clear status messages for each action
- [ ] Build and run each command to verify

### US-014: Implement stage must build and test
**Description:** As a user, I want the Implement stage agent to actually compile the project and run tests before declaring success, ensuring changes don't break the build.

**Acceptance Criteria:**
- [ ] Update `prompts/pipeline_implement.md` to include mandatory instructions: "After making all code changes, you MUST run the project's build command and test suite. Report build/test results in your output."
- [ ] Implement agent output schema extended: add `build_result: { success: bool, output: String }` and `test_result: { success: bool, output: String, tests_passed: u32, tests_failed: u32 }`
- [ ] If build fails, the Implement stage output marks `success: false` and includes the build error
- [ ] If tests fail, output includes which tests failed and error messages
- [ ] The Review stage receives build/test results as part of its input context
- [ ] Build verification is mandatory (not optional) — if the agent skips it, the review stage must catch this and request a re-run

### US-015: Build and run CLI for end-to-end verification
**Description:** As a developer, after implementing all features I must build the project and run the CLI to verify everything works end-to-end.

**Acceptance Criteria:**
- [ ] `cargo build --workspace` completes with no errors
- [ ] `cargo test --workspace --exclude nflow-daemon` passes
- [ ] `cargo test -p nflow-daemon -- --test-threads=1` passes
- [ ] `cargo clippy --workspace --all-targets` passes with no warnings
- [ ] Build the release binary: `cargo build --release`
- [ ] Run `./target/release/nflow pipeline --help` and verify all subcommands are listed
- [ ] Run `./target/release/nflow pipeline list` against a running daemon and verify output
- [ ] **IMPORTANT**: After each build, restart the daemon before testing CLI commands (daemon uses the old binary until restarted)
- [ ] Run `./target/release/nflow pipeline start "test task" --mode auto` and verify it starts

## Functional Requirements

- FR-1: `pipeline.start` command must accept a `mode` parameter ("manual" or "auto", default "auto")
- FR-2: In manual mode, Plan stage must output structured questions before generating the plan
- FR-3: In manual mode, pipeline pauses at `WaitingForApproval` after Plan completes until user approves/rejects
- FR-4: In manual mode, pipeline pauses at `WaitingForFinalApproval` after successful Review until user approves/rejects
- FR-5: In auto mode, a separate Claude Code agent process answers planning questions by searching the codebase
- FR-6: Auto-answerer agent has read-only codebase access (Read, Glob, Grep) and max 10 turns / 60s timeout
- FR-7: If auto-answerer fails, pipeline continues with a fallback default answer and logs a warning
- FR-8: Real-time agent output streams to TUI via `PipelineAgentOutput` events with a 1000-line ring buffer
- FR-9: Real-time agent output streams to CLI via `pipeline.start` streaming response
- FR-10: Implement stage agent MUST run build and test commands before completing
- FR-11: User rejection in Manual mode triggers a new iteration cycle with previous context + user feedback
- FR-12: All pipeline operations available via CLI commands: `start`, `list`, `status`, `cancel`, `log`, `approve`, `reject`, `answer`, `questions`
- FR-13: Pipeline state machine extended: `Pending → Running → WaitingForApproval → Running → WaitingForFinalApproval → Completed/Failed`
- FR-14: New pipeline dialog in TUI has mode selector (Manual/Auto)

## Non-Goals

- No web UI or REST API — only TUI and CLI
- No parallel pipeline execution within a single project (still one active pipeline per project)
- No custom stage templates or user-defined stages
- No git branching integration for pipelines (still in-place execution)
- No token usage tracking or cost estimation
- No auto-detection of build/test commands — the prompt instructs the agent to find and run them
- No partial rollback of changes on pipeline failure

## Technical Considerations

- **CRITICAL — Daemon restart after build**: After every `cargo build` the daemon binary changes on disk, but the running daemon process still uses the old binary in memory. You MUST restart the daemon (`nflow daemon stop && nflow daemon start`, or kill the process and let CLI auto-start it) after each build before testing CLI commands. Otherwise you will be testing against stale code and new commands/handlers will not be recognized.
- **Event bus**: All new events go through the existing `EventBus` broadcast channel. No architectural change needed.
- **Daemon handlers**: New command handlers in `handlers.rs` for `pipeline.approve`, `pipeline.reject`, `pipeline.answer`, `pipeline.questions`, `pipeline.final-approve`, `pipeline.final-reject`. This file is already large (~10k lines); consider a `handlers/pipeline.rs` extraction if it becomes unwieldy.
- **Auto-answerer agent**: Spawned as a separate `claude` process via `nflow-claude` crate, same as other stage agents. Uses a new prompt template `pipeline_auto_answer.md`. PID tracked in `agent_runs`.
- **Pipeline executor**: Currently a self-contained tokio task. Will need to support "pause and wait for external signal" semantics using `tokio::sync::watch` or `tokio::sync::Notify` for approval gates.
- **CLI**: New `Pipeline` subcommand in `nflow-cli/src/cli.rs` using clap. All commands communicate via the existing Unix socket NDJSON protocol.
- **Migration**: New `004_pipeline_mode.sql` adds `mode` column to `pipeline_runs` and creates `pipeline_questions` table.
- **TUI state**: New overlay types for question dialog, plan approval, and final approval. Existing `Overlay` enum in `app.rs` extended.
- **Ring buffer**: Use `VecDeque` with capacity 1000 for agent output lines in TUI.
- **Prompt changes**: `pipeline_plan.md` updated to instruct structured question output. New `pipeline_auto_answer.md` prompt. `pipeline_implement.md` updated with mandatory build/test instructions.

## Success Metrics

- Manual mode pipeline completes full cycle: questions → plan approval → implement (with build/test) → review → final approval
- Auto mode pipeline completes without human intervention, auto-answerer finds relevant code context
- `nflow pipeline start` in CLI works end-to-end with streaming output
- All existing pipeline tests continue to pass
- `cargo build --workspace && cargo test --workspace --exclude nflow-daemon && cargo test -p nflow-daemon -- --test-threads=1` all green
- TUI shows real-time agent output without lag or memory growth

## Open Questions

- Should questions be batched (Claude outputs all questions at once) or streamed one-by-one? Batch is simpler for MVP.
- Should the auto-answerer agent model be configurable (e.g., use a cheaper model like Haiku for auto-answering)?
- What build/test commands should the Implement agent try? The prompt can instruct it to look for `Makefile`, `Cargo.toml`, `package.json`, etc., and use the appropriate commands.
- Should rejected plans be saved for context, or discarded? Saving provides better context for re-planning.
