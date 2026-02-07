# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is nflow

nflow is a CLI/TUI orchestrator for Claude Code agents. It manages the full software development lifecycle: writing specs through interactive dialogue, decomposing specs into a DAG of epics/stories/tasks (in independent waves), and executing those tasks via parallel Claude Code agents in isolated git worktrees. Each story produces a branch, commits, and a merge request.

## Project Status

This project is in the design/documentation phase. All architecture docs are in `docs/`. No source code exists yet — implementation will create a Rust workspace under `crates/`.

## Build & Development Commands

```bash
cargo build --release              # Build all crates
cargo test                         # Run all tests
cargo test -p nflow-core           # Run tests for a specific crate
cargo clippy                       # Lint
cargo fmt --check                  # Check formatting
cargo install --path crates/nflow-cli  # Install CLI binary
```

Requires: Rust 1.75+, Claude Code CLI, Git 2.20+, `gh`/`glab` CLI for PR/MR creation.

## Architecture

Six-crate Rust workspace:

```
crates/
├── nflow-core/      # Business logic library (pure, no IO)
│                    # project, spec, work_item, dag, scheduler, db (rusqlite)
├── nflow-claude/    # Claude Code CLI wrapper
│                    # runner (spawns `claude -p`), stream (parses stream-json),
│                    # session (tracks/resumes sessions), prompt (template rendering)
├── nflow-git/       # Git operations (git2 + CLI fallback)
│                    # worktree, branch, mr (gh/glab)
├── nflow-daemon/    # Long-running background process (tokio)
│                    # Unix socket server, command handlers, scheduler loop
├── nflow-cli/       # Thin CLI client (clap), sends commands to daemon via socket
└── nflow-tui/       # TUI client (ratatui + crossterm), connects to daemon via socket
```

**Key architectural decisions:**
- `nflow-core` is pure logic — no IO, no async. All other crates depend on it.
- The daemon is the **sole writer** to SQLite. CLI/TUI are read-only clients that communicate via Unix socket (`~/.nflow/nflow.sock`) using NDJSON protocol.
- Scheduler runs on the daemon's main event loop (not a separate task) — command handling and scheduling are serialized, eliminating race conditions.
- All state lives in SQLite at `~/.nflow/nflow.db` with WAL mode. Specs are markdown files on disk referenced by path from the DB.

## Three-Phase Flow

1. **SPEC** — Interactive Claude dialogue produces markdown specs (`nflow spec new`)
2. **DECOMPOSE** — Claude decomposes approved specs into epic→story→task DAG as JSON (`nflow plan generate`). Auto-generates verify tasks after each impl task.
3. **EXECUTE** — Scheduler runs Claude agents in git worktrees. Each story = 1 worktree + 1 branch + 1 MR. Tasks run sequentially within a story (impl→verify alternation). Stories run in parallel across dependency chains (up to `max_parallel`).

## Work Item Hierarchy & IDs

- Epic → Story → Task (impl/verify). Dependencies are between stories only.
- Short IDs: `E1, S1, T1, T1v`. Wave-prefixed for uniqueness: `W1-S1, W2-T3v`.
- Internally all IDs are UUIDs. Short IDs are display/CLI input only.
- Verify tasks are auto-generated (not from Claude) with `kind='verify'`, `short_id="{id}v"`.

## Claude Code Invocation Patterns

- Spec sessions: `claude -p` with `--append-system-prompt-file prompts/spec_session.md`, resumed via `--resume {session_id}` chain
- Decomposition: `claude -p "{specs_concat}"` with `--append-system-prompt-file prompts/decompose.md`
- Impl tasks: `claude -p` with `--allowedTools "Read,Write,Edit,Bash,Glob,Grep"` and `--max-turns 50`
- Verify tasks: `claude -p` with `--allowedTools "Read,Bash,Glob,Grep"` (no Write/Edit) and `--max-turns 30`
- All use `--output-format stream-json --verbose --include-partial-messages`

## Prompt Templates

Located in `prompts/`, embedded at compile time via `include_str!`. User overrides in `~/.nflow/prompts/`. Uses simple `{variable}` substitution. Key templates: `spec_session.md`, `decompose.md`, `task_execution.md`, `verify_task.md`, `mr_body.md`.

## Database & Migrations

SQLite with custom migration system (~50 LOC). Numbered files in `migrations/` (`001_init.sql`, etc.), embedded at compile time. Applied on daemon start with backup. Schema version tracked in `schema_version` table.

Key tables: `projects`, `specs`, `work_items` (unified for epic/story/task), `dependencies`, `agent_runs`, `decomposition_sessions`.

## Daemon Lifecycle

- Started via `Command::new("nflow-daemon").spawn()` + `setsid()` (no double-fork, incompatible with tokio)
- Auto-started on first command needing it, with `flock` on `~/.nflow/daemon.lock` to prevent races
- Crash recovery: checks stale `agent_runs` PIDs with `pid_start_time` from `/proc/{pid}/stat` to detect PID reuse
- Graceful shutdown: SIGTERM → stop scheduler → wait 60s for agents → SIGTERM → 10s → SIGKILL

## Filesystem Layout

```
~/.nflow/
├── config.toml, nflow.sock, daemon.pid, nflow.db
├── logs/daemon.log
├── projects/{name}/specs/*.md, agent-logs/{W1-T1}.log
└── worktrees/{name}/{branch}/
```

## scripts/ralph/

Ralph is a separate tool — an autonomous agent loop script (`ralph.sh`) that iterates over a PRD (`prd.json`), implementing user stories one at a time. It's not part of nflow's core; it's a standalone utility in this repo.
