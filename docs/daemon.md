# nflow — Daemon Architecture

## Overview

The nflow daemon is a long-running background process that:
- Manages all Claude Code agent processes
- Runs the scheduler loop
- Serves state to CLI and TUI clients
- Persists state to SQLite

## Lifecycle

### Start

```bash
nflow daemon start
```

1. Check if daemon is already running (PID file + process check)
2. Spawn `nflow-daemon` as a child process via `Command::new("nflow-daemon").spawn()` (no classic double-fork — incompatible with tokio). The daemon process immediately ignores `SIGHUP` (`signal(SIGHUP, SIG_IGN)`) on startup, then calls `setsid()` to become a session leader and detach from the terminal. After `setsid()` completes, normal signal handling is restored. This prevents the daemon from being killed if the terminal closes between spawn and setsid. With `--foreground`, runs in the current process instead.
3. Write PID to `~/.nflow/daemon.pid`
4. Open/create SQLite database at `~/.nflow/nflow.db`
5. Run pending migrations
6. Bind Unix socket at `~/.nflow/nflow.sock`
7. Start scheduler loop
8. Accept client connections

### Stop

```bash
nflow daemon stop
```

1. Send SIGTERM to daemon PID
2. Daemon gracefully shuts down:
   - Stop accepting new connections
   - Stop scheduler (don't start new tasks)
   - Wait for running agents to finish (with timeout)
   - Close database
   - Remove socket file
   - Remove PID file

### Auto-Start

Any `nflow` command that requires the daemon will auto-start it if not running.

```
nflow spec new "auth" → daemon not running → start daemon → proceed
```

To prevent race conditions when multiple CLI commands detect "daemon not running" simultaneously, the auto-start acquires an exclusive file lock (`flock`) on `~/.nflow/daemon.lock` before checking/spawning. The second command blocks until the first finishes starting the daemon, then detects it's already running and proceeds normally.

## Socket Protocol

### Transport

Unix domain socket at `~/.nflow/nflow.sock`. Newline-delimited JSON (NDJSON) messages.

**NDJSON rules:**
- Each message is a single JSON object followed by a newline (`\n`)
- A message MUST NOT contain unescaped newlines — newlines within string values MUST be JSON-escaped (`\n` → `\\n`)
- The receiver splits input on `\n` and parses each line as an independent JSON object
- Empty lines are ignored

### Handshake

On connection, the client sends a handshake message:
```json
{"protocol_version": 1}
```

The daemon responds:
```json
{"protocol_version": 1, "daemon_version": "0.1.0"}
```

If the protocol versions are incompatible, the daemon responds with an error and closes the connection:
```json
{"status": "error", "error": {"code": "INCOMPATIBLE_VERSION", "message": "Client protocol v2 is not supported. Daemon supports v1. Please update nflow."}}
```

The protocol version is incremented only on breaking changes to the message format. Adding new commands or new optional fields does not require a version bump.

### Request Format

```json
{
  "id": "request-uuid",
  "command": "command.name",
  "params": { ... }
}
```

### Response Format

Success:
```json
{
  "id": "request-uuid",
  "status": "ok",
  "data": { ... }
}
```

Error:
```json
{
  "id": "request-uuid",
  "status": "error",
  "error": {
    "code": "NOT_FOUND",
    "message": "Project not found"
  }
}
```

### Error Codes

| Code | HTTP-like | Description |
|------|-----------|-------------|
| `NOT_FOUND` | 404 | Requested resource (project, spec, task, story) does not exist |
| `INVALID_STATE` | 409 | Operation not allowed in current state (e.g., approve a non-draft spec, feedback on approved plan) |
| `INVALID_PARAMS` | 400 | Missing or invalid command parameters |
| `ALREADY_EXISTS` | 409 | Resource already exists (e.g., project with same name) |
| `DEPENDENCY_ERROR` | 409 | Cannot delete resource due to dependencies (e.g., decomposed spec) |
| `AGENT_ERROR` | 500 | Claude agent process failed to start |
| `EXECUTION_BLOCKED` | 409 | Cannot execute: work items have unmet dependencies or are already running |
| `INTERNAL_ERROR` | 500 | Unexpected daemon error |
| `NOT_RUNNING` | 503 | Daemon is shutting down, not accepting new commands |

### Streaming Response

For commands that produce ongoing output (spec sessions, agent logs), the daemon sends a stream of events after the initial response:

```json
{"id": "req-uuid", "status": "ok", "streaming": true}
{"type": "event", "stream_id": "req-uuid", "event": {"kind": "agent_output", "text": "Reading file..."}}
{"type": "event", "stream_id": "req-uuid", "event": {"kind": "agent_output", "text": "Editing file..."}}
{"type": "event", "stream_id": "req-uuid", "event": {"kind": "stream_end"}}
```

### Command List

| Command | Description |
|---------|-------------|
| `daemon.status` | Get daemon status |
| `project.init` | Initialize project |
| `project.list` | List projects |
| `project.delete` | Delete project and all associated data |
| `spec.new` | Start spec session |
| `spec.list` | List specs |
| `spec.view` | View spec content |
| `spec.resume` | Resume spec session |
| `spec.approve` | Approve spec |
| `spec.reopen` | Move approved spec back to draft |
| `spec.delete` | Delete a spec |
| `plan.generate` | Generate plan (new wave) from unassigned specs |
| `plan.show` | Get plan data (all waves or specific wave) |
| `plan.feedback` | Submit plan feedback (targets latest draft wave) |
| `plan.approve` | Approve plan (targets latest draft wave) |
| `exec.run` | Start execution |
| `exec.status` | Get execution status |
| `exec.stop` | Stop agent(s) |
| `exec.retry` | Retry failed task |
| `exec.skip` | Skip failed task |
| `exec.continue` | Continue failed story after manual fix |
| `exec.cancel` | Cancel a pending/ready story |
| `exec.pause` | Disable execution for a project |
| `exec.log` | Get/stream task log |
| `plan.discard` | Discard a wave's plan and work items |
| `spec.answer` | Forward user's answer to spec session |
| `worktree.list` | List active worktrees for a project |
| `worktree.clean` | Remove worktrees for done/cancelled stories |
| `cleanup.logs` | Remove old agent log files |
| `config.get` | Get config value |
| `config.set` | Set config value |

### User Input Forwarding

During spec sessions, Claude asks questions via the `AskUserQuestion` tool. The flow:

1. Daemon detects `AskUserQuestion` tool call in Claude's stream output
2. Daemon sends event to connected client:
   ```json
   {"type": "event", "event": {"kind": "user_question", "question": "...", "options": [...]}}
   ```
3. Client displays question to user, collects answer
4. Client sends answer:
   ```json
   {"command": "spec.answer", "params": {"session_id": "...", "answer": "user's answer"}}
   ```
5. Daemon starts a new Claude process: `claude -p "{answer}" --resume {session_id} --output-format stream-json`. The answer is passed as plain text in `-p`.

## Scheduler

### Loop

The scheduler runs as part of the daemon's **main event loop**, not as a separate tokio task. This eliminates race conditions between client commands and scheduling decisions — all state mutations (command handling + scheduling) are serialized on the same loop.

```rust
// Main event loop
loop {
    tokio::select! {
        // Handle client commands (socket messages)
        Some(cmd) = command_rx.recv() => {
            handle_command(cmd).await;
        }

        // Scheduler tick (every 2 seconds)
        _ = scheduler_interval.tick() => {
            // 1. Reap completed processes
            check_agent_processes().await;

            // 2. Update statuses
            propagate_status_changes().await;

            // 3. Schedule new work (only for projects with execution enabled)
            let running = count_running_agents().await;
            if running < max_parallel {
                let ready_stories = find_ready_stories_with_execution_enabled().await;
                for story in ready_stories.take(max_parallel - running) {
                    start_story(story).await;
                }
            }
        }
    }
}
```

Because both command handling and scheduling share the same loop, operations like `plan.discard` (which checks for in_progress stories, then deletes work items) cannot race with the scheduler picking up new stories. The scheduler tick only runs when no command is being processed.

### Process Management

Each Claude agent runs as a child process of the daemon:

```rust
let child = Command::new("claude")
    .args(["-p", &prompt])
    .args(["--output-format", "stream-json"])
    .args(["--verbose"])
    .args(["--include-partial-messages"])
    .args(["--append-system-prompt-file", &context_file])
    .args(["--allowedTools", "Read,Write,Edit,Bash,Glob,Grep"])
    .args(["--max-turns", &max_turns.to_string()])
    .current_dir(&worktree_path)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
```

The daemon:
- Stores child PID in `agent_runs` table
- Spawns a tokio task to read stdout line by line
- Parses stream-json events
- Forwards events to connected clients
- Writes raw output to log file
- Detects process exit and updates status
- Enforces `max_time_per_task` wall-clock timeout (default: 30 min). If exceeded, sends SIGTERM to the agent process, waits 10s, then SIGKILL. Task status → `failed` with error "wall-clock timeout exceeded". No separate idle/silence timeout — a single wall-clock limit covers all hang scenarios without the complexity of tracking last stdout timestamp.

### Graceful Shutdown

On SIGTERM:
1. Set `shutting_down` flag
2. Scheduler stops picking new stories
3. Wait up to 60 seconds for running agents to finish
4. If agents still running after timeout, send SIGTERM to them
5. Wait 10 more seconds, then SIGKILL any remaining
6. Close database, remove socket, remove PID file

### Crash Recovery

On daemon start, check for stale state:
1. Find `agent_runs` with `status = 'running'`
2. For each, check if PID is still alive AND verify `pid_start_time` matches the stored value (read from `/proc/{pid}/stat` field 22 on Linux). If the start time differs, the PID was reused by a different process.
3. If PID is dead or start time doesn't match → mark as `failed` with error "daemon crashed"
4. If PID is alive and start time matches → the agent is still running from before the crash. Adopt the process (resume reading its stdout). This is rare but possible if only the daemon's main loop crashed while child processes survived.
5. Reset any `specs` with `session_active = 1` back to `session_active = 0` (stale from crash)
6. Find `work_items` with `status = 'in_progress'` whose agent_run is now `failed`
7. Reset to `ready` (stories) or `failed` (tasks that were mid-execution)
8. Resume normal operation

## Concurrency Model

The daemon uses a **single-writer** architecture:

- All database writes happen on the daemon's main event loop (single-threaded for writes)
- CLI/TUI clients send commands via the socket; the daemon processes them sequentially
- Multiple TUI clients can connect simultaneously and receive state updates
- Agent output streaming runs on separate tokio tasks but only writes to log files and sends events — database updates happen on the main loop via channels

**What happens with concurrent commands:**
- If two clients send `plan.feedback` simultaneously, the first one triggers a Claude re-generation. The second one is queued and executes after the first completes — it operates on the result of the first regeneration.
- If one client runs `exec.stop` while another runs `exec.run`, commands execute in arrival order.
- State events (status changes, agent output) are broadcast to all connected clients.

No optimistic locking or version checks needed — the daemon serializes all mutations.

## Migrations

See `docs/data-model.md` → Migrations section for the schema versioning mechanism.
