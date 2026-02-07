# nflow — Security Considerations

## Unix Socket Permissions

The daemon listens on `~/.nflow/nflow.sock`.

- Socket is created with file mode `0600` (owner read/write only)
- Only the user who started the daemon can connect
- The `~/.nflow/` directory itself should be `0700`

nflow enforces these permissions on startup:
1. Check `~/.nflow/` directory permissions, fix to `0700` if needed
2. Create socket with `umask(0o177)` to ensure `0600`
3. Log a warning if directory permissions are too open

## Agent Sandboxing

During execution phase, Claude agents run with `--allowedTools "Read,Write,Edit,Bash,Glob,Grep"`. The `Bash` tool means agents can execute arbitrary commands within the worktree.

### Mitigations

1. **Worktree isolation**: Each agent works in a separate git worktree, not in the main repo. Damage is limited to the worktree.

2. **No network tools by default**: `WebSearch` and `WebFetch` are not in the allowed tools list. Agents cannot make network requests beyond what `Bash` allows.

3. **Max turns limit**: `max_turns_per_task` (default: 50) prevents runaway agents.

4. **User review via MR**: All changes go through a merge request. Nothing is merged automatically.

5. **Process limits**: The daemon enforces `max_parallel` to prevent resource exhaustion.

### What is NOT sandboxed

- Bash commands within the worktree have full user permissions
- An agent could theoretically read/write files outside the worktree via Bash
- An agent could install packages, run network commands, etc.
- **Verify agents** have `Write` and `Edit` tools removed, but retain `Bash` access for running build/test commands. In theory, a verify agent could modify files via Bash (`echo > file`). The prompt explicitly forbids this, and in practice Claude Code follows the instructions. This is a known limitation — enforced by prompt, not by tool restriction.

This is consistent with how Claude Code works in general — it operates with the user's permissions. The worktree provides logical isolation, not security isolation.

### Optional hardening (future)

- Restrict Bash commands to specific prefixes via `--allowedTools "Bash(git *),Bash(cargo *)"` etc.
- Run agents in containers or namespaces
- Use `--permission-mode plan` for read-only analysis tasks

## Secret Files

nflow does NOT store any secrets. Authentication for:
- Claude API: managed by `claude` CLI itself (OAuth or API key)
- GitHub: managed by `gh` CLI
- GitLab: managed by `glab` CLI

The SQLite database at `~/.nflow/nflow.db` contains project paths, spec content references, and task descriptions. It does not contain credentials.

## Spec Content

Spec files are plain markdown stored at `~/.nflow/projects/{name}/specs/`. They may contain sensitive business logic descriptions. The `~/.nflow/` directory permissions (0700) protect them from other users on the system.

## Cost Control

nflow does not manage API budgets directly. Cost control is handled by:
- `max_turns_per_task` — limits the number of agentic turns per task (default: 50 for impl, 30 for verify)
- `max_time_per_task` — wall-clock timeout kills runaway agents (default: 30 min)
- `max_parallel` — limits concurrent agent processes

Users can set up spending alerts and limits in their Claude/Anthropic account dashboard.
