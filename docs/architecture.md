# nflow — Architecture

## Overview

nflow is a CLI/TUI orchestrator for Claude Code agents. It manages the full lifecycle of software development: from writing specifications through interactive dialogue, to decomposing specs into a DAG of epics/stories/tasks (specs can be decomposed in independent waves), to executing those tasks via parallel Claude Code agents in isolated git worktrees.

## High-Level Architecture

```
                    ┌─────────────┐
                    │   User      │
                    └──────┬──────┘
                           │
              ┌────────────┴────────────┐
              │                         │
        ┌─────▼─────┐           ┌──────▼──────┐
        │  nflow-cli │           │  nflow-tui  │
        │  (clap)    │           │  (ratatui)  │
        └─────┬──────┘           └──────┬──────┘
              │     Unix Socket         │
              └────────────┬────────────┘
                           │
                    ┌──────▼──────┐
                    │ nflow-daemon│
                    │             │
                    │ ┌─────────┐ │
                    │ │Scheduler│ │
                    │ └─────────┘ │
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
        ┌─────▼─────┐ ┌───▼───┐ ┌─────▼─────┐
        │nflow-core │ │nflow- │ │nflow-git  │
        │           │ │claude │ │           │
        │ project   │ │       │ │ worktree  │
        │ spec      │ │runner │ │ branch    │
        │ work_item │ │stream │ │ mr        │
        │ dag       │ │prompt │ │           │
        │ db        │ │session│ │           │
        └───────────┘ └───────┘ └───────────┘
```

## Components

### nflow-cli

CLI interface built with `clap`. Thin client that sends commands to the daemon via Unix socket. Used by AI agents for testing and automation.

### nflow-tui

TUI interface built with `ratatui` + `crossterm`. Connects to daemon via Unix socket. Provides dashboard, spec viewer, DAG visualization, agent output streaming. Primary interface for human users.

### nflow-daemon

Long-running background process. Core of the system. Responsibilities:
- Listens on Unix socket at `~/.nflow/nflow.sock`
- Runs the scheduler loop
- Manages Claude Code child processes
- Serves state to CLI/TUI clients
- Persists all state to SQLite

Lifecycle:
- Started explicitly with `nflow daemon start` or auto-started on first command
- Stays running after terminal close
- Stopped with `nflow daemon stop`
- PID file at `~/.nflow/daemon.pid`

### nflow-core

Business logic library. No IO — pure data structures and logic.
- `project.rs` — project CRUD, path binding
- `spec.rs` — spec lifecycle (draft → approved → decomposed)
- `work_item.rs` — epic/story/task CRUD
- `dag.rs` — DAG construction, topological sort, ready-item detection (per wave)
- `scheduler.rs` — scheduling algorithm (pick ready stories across all waves, respect parallel limit)
- `db.rs` — SQLite access layer via `rusqlite`

### nflow-claude

Wrapper around Claude Code CLI (`claude`). Handles:
- `runner.rs` — spawning `claude -p` processes with correct flags
- `stream.rs` — parsing `stream-json` output line by line
- `session.rs` — tracking claude session IDs, resuming sessions
- `prompt.rs` — generating prompts from templates + context data

### nflow-git

Git operations. Uses `git2` (libgit2) where possible, falls back to CLI for complex operations.
- `worktree.rs` — create/remove git worktrees
- `branch.rs` — create branches, detect base branch
- `mr.rs` — create MR/PR via `gh` or `glab` CLI

## Crate Structure

```
nflow/
├── Cargo.toml                 # Workspace root
├── crates/
│   ├── nflow-core/            # Business logic, data model, DB
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── project.rs
│   │   │   ├── spec.rs
│   │   │   ├── work_item.rs
│   │   │   ├── dag.rs
│   │   │   ├── scheduler.rs
│   │   │   └── db.rs
│   │   └── Cargo.toml
│   │
│   ├── nflow-claude/          # Claude Code CLI wrapper
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── runner.rs
│   │   │   ├── stream.rs
│   │   │   ├── session.rs
│   │   │   └── prompt.rs
│   │   └── Cargo.toml
│   │
│   ├── nflow-git/             # Git operations
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── worktree.rs
│   │   │   ├── branch.rs
│   │   │   └── mr.rs
│   │   └── Cargo.toml
│   │
│   ├── nflow-daemon/          # Daemon process
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── server.rs      # Unix socket server
│   │   │   ├── handler.rs     # Command handlers
│   │   │   └── scheduler.rs   # Scheduler loop
│   │   └── Cargo.toml
│   │
│   ├── nflow-cli/             # CLI client
│   │   ├── src/
│   │   │   └── main.rs
│   │   └── Cargo.toml
│   │
│   └── nflow-tui/             # TUI client
│       ├── src/
│       │   ├── main.rs
│       │   ├── app.rs
│       │   ├── views/
│       │   │   ├── dashboard.rs
│       │   │   ├── spec.rs
│       │   │   ├── plan.rs
│       │   │   ├── execution.rs
│       │   │   └── agent_log.rs
│       │   └── widgets/
│       └── Cargo.toml
│
├── migrations/                # SQLite migrations
│   ├── 001_init.sql
│   └── ...
│
├── prompts/                   # Prompt templates for Claude
│   ├── spec_session.md
│   ├── decompose.md
│   ├── task_execution.md
│   └── verify_task.md
│
└── docs/                      # Documentation
```

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `ratatui` + `crossterm` | TUI rendering |
| `clap` | CLI argument parsing |
| `rusqlite` | SQLite database |
| `tokio` | Async runtime (daemon, socket, process management) |
| `serde` + `serde_json` | JSON parsing (claude stream-json) |
| `petgraph` | DAG data structure, topological sort |
| `git2` | Git operations (libgit2 bindings) |
| `uuid` | ID generation |
| `chrono` | Timestamps |
| `tracing` + `tracing-subscriber` | Structured logging |
| `signal-hook` | Unix signal handling in daemon |
| `toml` | Config file parsing |

## Communication Protocol

CLI/TUI communicate with daemon via Unix socket using a simple JSON-based protocol.

Request:
```json
{
  "id": "uuid",
  "command": "spec.new",
  "params": { "project_id": "...", "with_codebase": false }
}
```

Response:
```json
{
  "id": "uuid",
  "status": "ok",
  "data": { ... }
}
```

For streaming (agent output), the daemon sends newline-delimited JSON events over the socket:
```json
{"type": "agent_output", "task_id": "...", "text": "Reading src/main.rs..."}
{"type": "status_change", "task_id": "...", "status": "done"}
```

## Filesystem Layout

```
~/.nflow/
├── config.toml              # Global configuration
├── nflow.sock               # Unix socket (when daemon is running)
├── daemon.pid               # Daemon PID file
├── nflow.db                 # SQLite database
├── logs/
│   └── daemon.log           # Daemon log
├── projects/
│   └── {project-name}/
│       ├── specs/           # Markdown spec files
│       │   ├── auth-login.md
│       │   └── auth-oauth.md
│       └── agent-logs/      # Per-task agent output logs
│           ├── W1-T1.log
│           ├── W1-T1v.log
│           └── W1-T2.log
└── worktrees/
    └── {project-name}/
        ├── story-auth-login/       # git worktree
        └── story-auth-register/    # git worktree
```
