# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is nflow

nflow is a CLI/TUI orchestrator for Claude Code agents. It manages the full software development lifecycle: writing specs through interactive dialogue, decomposing specs into a DAG of epics/stories/tasks (in independent waves), and executing those tasks via parallel Claude Code agents in isolated git worktrees. Each story produces a branch, commits, and a merge request.

## Build & Development Commands

```bash
cargo build                        # Build all crates (debug)
cargo build --release              # Build all crates (release)
cargo test --workspace --exclude nflow-daemon  # Run all tests except daemon
cargo test -p nflow-daemon -- --test-threads=1 # Daemon tests (must be single-threaded for E2E safety)
cargo test -p nflow-core           # Run tests for a specific crate
cargo clippy --workspace --all-targets # Lint
cargo fmt --check                  # Check formatting
```

CI runs `cargo fmt --check` → `cargo clippy` → `cargo build --workspace` → tests (daemon single-threaded). See `.github/workflows/ci.yml`.

Requires: Rust 1.75+, Claude Code CLI, Git 2.20+, `gh`/`glab` CLI for PR/MR creation.

## Architecture

Seven-crate Rust workspace (six production + one test utility):

```
crates/
├── nflow-core/      # Pure business logic (no IO, no async)
├── nflow-claude/    # Claude Code CLI wrapper (process spawn, stream-json parsing)
├── nflow-git/       # Git operations via tokio::process (worktrees, branches, MR/PR)
├── nflow-daemon/    # Long-running background process (tokio, Unix socket, scheduler)
├── nflow-cli/       # Thin CLI client (clap), sends commands to daemon via socket
├── nflow-tui/       # TUI client (ratatui + crossterm), connects to daemon via socket
└── mock-claude/     # Test utility: mimics `claude` CLI for E2E tests without API calls
```

**Key architectural decisions:**
- `nflow-core` is pure logic — no IO, no async. All other crates depend on it.
- The daemon is the **sole writer** to SQLite. CLI/TUI are read-only clients that communicate via Unix socket (`~/.nflow/nflow.sock`) using NDJSON protocol.
- Scheduler runs on the daemon's main event loop (not a separate task) — command handling and scheduling are serialized, eliminating race conditions. Ticks every 2 seconds.
- All state lives in SQLite at `~/.nflow/nflow.db` with WAL mode. Specs are markdown files on disk referenced by path from the DB.

## Daemon-Client Protocol

Unix socket at `~/.nflow/nflow.sock`, NDJSON (newline-delimited JSON).

**Handshake** (on connect): client sends `{"protocol_version": 1}`, daemon responds with `{"protocol_version": 1, "status": "ok"}`.

**Request/Response**: `{"id": "req-123", "command": "project.init", "params": {...}}` → `{"id": "req-123", "status": "ok", "data": {...}}`.

**Streaming**: multiple lines with `done: false`, final line with `done: true`.

**Command namespaces**: `project.*`, `spec.*`, `plan.*`, `exec.*`, `pipeline.*`, `worktree.*`, `cleanup.*`, `config.*`. All command dispatch is in `nflow-daemon/src/handlers.rs` (large file — ~10k lines).

## Two Development Flows

nflow supports two workflows:

### SDD Flow (SPEC → DECOMPOSE → EXECUTE)

The full structured development lifecycle:

1. **SPEC** — Interactive Claude dialogue produces markdown specs (`nflow spec new`)
2. **DECOMPOSE** — Claude decomposes approved specs into epic→story→task DAG as JSON (`nflow plan generate`). Auto-generates verify tasks after each impl task.
3. **EXECUTE** — Scheduler runs Claude agents in git worktrees. Each story = 1 worktree + 1 branch + 1 MR. Tasks run sequentially within a story (impl→verify alternation). Stories run in parallel across dependency chains (up to `max_parallel`).

Use for: complex features, formal planning, parallel story execution, structured MR workflow.

### Pipeline Flow (Plan → Implement → Review)

Simplified 3-stage sequential flow for rapid development:

1. **PLAN** — Claude analyzes task and creates implementation plan
2. **IMPLEMENT** — Claude executes plan and makes code changes (in-place, no branches)
3. **REVIEW** — Claude verifies implementation, loops back if issues found (max iterations: 5 default)

Use for: rapid prototyping, small features, bug fixes, quick exploration. See `docs/pipeline.md` for details.

## Work Item Hierarchy & IDs

- Epic → Story → Task (impl/verify). Dependencies are between stories only.
- Short IDs: `E1, S1, T1, T1v`. Wave-prefixed for uniqueness: `W1-S1, W2-T3v`.
- Internally all IDs are UUIDs. Short IDs are display/CLI input only.
- Verify tasks are auto-generated with `kind='verify'`, `short_id="{id}v"`.
- Status state machine: pending → ready → in_progress → done/failed/cancelled.

## Database

SQLite with custom migration system. Numbered files in `crates/nflow-daemon/migrations/` (`001_init.sql`, etc.), applied on daemon start.

Key tables: `projects`, `specs`, `work_items` (unified for epic/story/task with self-referential `parent_id`), `dependencies`, `agent_runs`, `decomposition_sessions`, `pipeline_runs`, `pipeline_stages`.

DB access layer is in `nflow-daemon/src/db/` (modules: `projects`, `specs`, `work_items`, `agent_runs`, `decomposition_sessions`, `pipeline`).

## Test Organization

- **nflow-core**: Inline `#[cfg(test)]` in each module (state machines, DAG, scheduler, short IDs)
- **nflow-daemon**: 7 test modules — `protocol_tests`, `commands_project_spec_tests`, `commands_plan_tests`, `commands_exec_tests`, `lifecycle_tests`, `e2e_tests`, `binary_e2e_tests`
- **mock-claude**: Controlled via env vars (`MOCK_CLAUDE_FAIL`, `MOCK_CLAUDE_SPEC_COMPLETE`, `MOCK_CLAUDE_DECOMPOSE_JSON`, etc.) — detects mode from prompt/system-prompt patterns
- Daemon tests must run single-threaded (`--test-threads=1`) due to shared socket/DB resources in E2E tests

## Prompt Templates

Located in `prompts/`, embedded at compile time via `include_str!`. User overrides in `~/.nflow/prompts/`. Uses simple `{variable}` substitution (`nflow-claude/src/prompt.rs`).

**SDD Flow templates**: `spec_session.md`, `decompose.md`, `task_execution.md`, `verify_task.md`, `mr_body.md`

**Pipeline Flow templates**: `pipeline_plan.md`, `pipeline_implement.md`, `pipeline_review.md`

## Daemon Lifecycle

- Started via `Command::new("nflow-daemon").spawn()` + `setsid()` (no double-fork, incompatible with tokio)
- Mode via `NFLOW_DAEMON_MODE` env var: `background` (daemonize), `foreground` (log to stderr), or default
- Auto-started on first CLI command needing it, with `flock` on `~/.nflow/daemon.lock` to prevent races
- Crash recovery: validates stale `agent_runs` PIDs using `/proc/{pid}/stat` start time to detect PID reuse
- Graceful shutdown: SIGTERM → stop scheduler → wait 60s for agents → SIGTERM → 10s → SIGKILL

## Environment Variables

- `NFLOW_HOME`: Override default `~/.nflow`
- `NFLOW_SOCKET`: Override socket path
- `NFLOW_LOG_LEVEL`: Override log level
- `NFLOW_MAX_PARALLEL`: Override max parallel agents
- `NFLOW_DAEMON_MODE`: `background`/`foreground` for daemon startup

## scripts/ralph/

Ralph is a separate tool — an autonomous agent loop script (`ralph.sh`) that iterates over a PRD (`prd.json`), implementing user stories one at a time. It's not part of nflow's core; it's a standalone utility in this repo.
