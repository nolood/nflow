# nflow — Design Decisions

Decisions made during design review of the core documentation. Each decision resolves an ambiguity or gap in the existing docs.

---

## 1. Interaction with Claude: `--resume` chain with plain text

**Context:** During spec sessions, Claude asks questions via `AskUserQuestion`. nflow needs to forward the user's answer back to Claude.

**Decision:** Use `--resume` chain. Each question-answer cycle is a separate Claude process:

1. `claude -p "{prompt}" --output-format stream-json` — Claude asks a question via `AskUserQuestion`, process exits
2. nflow captures `session_id` from the final `result` event in the stream
3. nflow shows the question to the user, collects the answer
4. `claude -p "{answer}" --resume {session_id} --output-format stream-json` — Claude continues

The answer is passed as **plain text** in `-p`. No structured tool-result imitation. Claude naturally understands that the next message after its question is the answer — the conversation history (restored via `--resume`) provides sufficient context.

**Rejected alternatives:**
- PTY (pseudo-terminal): fragile parsing of interactive output, no clean JSON
- Structured answer (imitating tool result): depends on Claude Code internals, unnecessary complexity

---

## 2. Hung agent detection: wall-clock timeout only

**Context:** Agents can hang — stdout stops producing output, but the process is alive.

**Decision:** Use only `max_time_per_task` (default: 1800s / 30 min) as a wall-clock timeout. No separate idle timeout (silence detection).

If the process exceeds the wall-clock limit: SIGTERM, wait 10s, SIGKILL. Task status → `failed`.

**Rationale:** A single timeout is simpler. The wall-clock limit covers all hang scenarios. An idle timeout would add complexity (tracking last stdout timestamp) with marginal benefit — a truly hung agent will hit the wall-clock limit eventually.

---

## 3. Daemonization: background process via `Command::spawn()`

**Context:** The daemon needs to run in the background and survive terminal closure.

**Decision:** No classic double-fork. `nflow daemon start` spawns `nflow-daemon` as a child process:

```rust
Command::new("nflow-daemon")
    .stdout(File::create(log_path)?)
    .stderr(File::create(log_path)?)
    .spawn()?;
```

The daemon process calls `setsid()` on startup to become a session leader, detaching from the terminal. PID is written to `~/.nflow/daemon.pid`.

**Rationale:** Classic double-fork is incompatible with tokio — you cannot fork after the async runtime is initialized. The spawn approach is simpler and avoids this problem entirely. The daemon process is reparented to init/systemd when the parent exits.

---

## 4. `nflow continue`: commit hash stored in database

**Context:** When a user fixes a failed story manually and runs `nflow continue`, nflow needs to determine which tasks are already done.

**Decision:** Store the commit hash in the database when an impl task completes. No heuristics based on commit counting.

After a successful impl task, nflow runs `git log -1 --format=%H` in the worktree and stores the hash in the `work_items` table (or `agent_runs`).

`nflow continue` logic:
1. Tasks with a stored `commit_hash` → `done`
2. The currently failed task → user called `continue`, so mark as `done`
3. Resume from the next `pending` task
4. If no pending tasks remain → proceed to push/MR

**Rejected alternative:** Counting commits in `git log {base}..HEAD` and mapping N commits to first N impl tasks — fragile with manual commits, amends, rebases.

---

## 5. Spec session completion: exit code 0 + file exists

**Context:** How does nflow know that Claude has finished writing a spec?

**Decision:** The Claude process exits with code 0 and the spec file exists on disk → spec status = `draft`. If exit code != 0 → session interrupted, can be resumed with `nflow spec resume`.

No special markers ("SPEC COMPLETE") in the output. No content validation (checking for required sections). The spec is a draft — the user reviews and approves it manually.

---

## 6. Plan feedback: delete old work items before calling Claude

**Context:** When the user gives feedback on a plan, Claude regenerates it. What happens to the old work items?

**Decision:** Delete all work items from the decomposition session **before** calling Claude with the feedback. Then insert the new work items from the regenerated JSON.

If Claude fails during regeneration, the database has no work items for this session, but the decomposition session remains in `in_progress` status. The user can retry `nflow plan feedback` or `nflow plan generate`.

**Rationale:** Simpler than atomic replacement. No risk of conflicting old/new items in the database during generation.

---

## 7. Task execution context: no spec content in prompts

**Context:** The `task_execution.md` template had a `{spec_content}` variable. With multiple specs, this could add 10,000+ words of context to every task prompt.

**Decision:** Remove `{spec_content}` from `task_execution.md`. Task descriptions must be self-sufficient.

The decompose prompt already requires: "Each task description must be detailed enough for an AI agent to implement it without additional context." If the decomposition is good, spec content is redundant.

Task agents receive: project info, epic context (title + description + acceptance criteria), story context, task description + acceptance criteria, summary of completed tasks in the story.

---

## 8. Push/MR failure: story → `failed`, tasks stay `done`

**Context:** All tasks completed successfully, but `git push` or `gh pr create` failed (network, auth, rate limit).

**Decision:** Story status → `failed`. All task statuses remain `done` (the work is complete, only delivery failed).

`nflow continue` on such a story: sees all tasks are `done` → skips directly to rebase/push/MR creation. Effectively retries only the final delivery step.

**Rejected alternative:** A separate `push_failed` status — adds complexity without meaningful benefit. The `failed` status + "all tasks done" is sufficient to determine that only push/MR needs to be retried.

---

## 9. Impl task success criteria: exit 0 + new commit + task ID in message

**Context:** How does nflow determine that an impl task completed successfully?

**Decision:** Three conditions must be met:
1. Agent process exited with code 0
2. HEAD changed (new commit exists — compare HEAD before and after)
3. Commit message contains `[{wave_short_id}]` (e.g., `[W1-T1] Create login API endpoint`)

The `task_execution.md` prompt instructs the agent to use this commit message format:
```
git commit -m "[{wave_short_id}] {description}"
```

nflow validates after completion:
```bash
git log -1 --format=%s  # must contain [{wave_short_id}]
git log -1 --format=%H  # must differ from pre-task HEAD
```

If any condition fails → task status = `failed`.

**Benefit:** Exact mapping of commits to tasks across waves. `nflow continue` can also use `[W{n}-{short_id}]` in commit messages to match manual commits to tasks.

---

## 10. Parallel story conflicts: not solved by nflow

**Context:** Stories running in parallel may produce merge conflicts when rebasing onto the updated base branch.

**Decision:** nflow does not attempt to prevent or minimize file-level conflicts between parallel stories. This is the user's responsibility.

The decomposition already creates `depends_on` for logical dependencies. File-level conflicts are rare if stories are well-decomposed (touching different areas of the codebase). When conflicts occur, the existing flow handles it: rebase fails → story `failed` → user resolves conflicts in worktree → `nflow continue`.

**Rejected alternatives:**
- Instructing Claude to add `depends_on` for stories touching the same files — unreliable, Claude can't guarantee this
- Static analysis of file references in task descriptions — over-engineering for a rare problem

---

## 11. Execution start: explicit `nflow run` with configurable auto-execute

**Context:** The daemon runs a scheduler loop every 2 seconds. After `nflow plan approve`, ready stories exist. Should the scheduler automatically start executing them, or wait for explicit `nflow run`?

**Decision:** Configurable via `auto_execute` in config (default: `false`).

- `auto_execute = false` (default): after plan approval, stories remain `ready` but the scheduler does not pick them up. User must run `nflow run` to enable execution for the project. Safer — prevents accidental agent launches.
- `auto_execute = true`: scheduler picks up `ready` stories immediately after plan approval.

`nflow run` sets an `execution_enabled` flag per project. The scheduler loop checks this flag before scheduling stories for a given project.

```toml
# Start execution automatically after plan approval
auto_execute = false
```

`nflow run` is always available regardless of the setting — it explicitly enables execution and can be used with `--story <id>` to run a specific story.
