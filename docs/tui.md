# nflow — TUI Design

## Launch

```bash
nflow tui
```

Connects to the daemon via Unix socket. If daemon is not running, offers to start it.

## Project Selection

On launch, nflow detects the project from the current working directory. If no project is found, or if multiple projects exist, a project selector is shown:

```
┌─ nflow ──────────────────────────────────────────────────┐
│                                                          │
│  Select project:                                         │
│                                                          │
│  > my-app         /home/user/my-app        3 specs       │
│    payments-svc   /home/user/payments-svc  1 spec        │
│    infra          /home/user/infra         0 specs       │
│                                                          │
│  [Enter] select  [n]ew project  [q]uit                   │
└──────────────────────────────────────────────────────────┘
```

The current project is shown in the TUI header bar. Switch projects at any time with `p` (opens the project selector overlay).

## Navigation

The TUI has four main views, switchable via tabs:

| Key | Tab | Purpose |
|-----|-----|---------|
| `1` | Specs | View and manage specifications |
| `2` | Plan | View DAG, approve/reject plan |
| `3` | Execute | Monitor running agents, view status |
| `4` | Logs | Detailed agent output streaming |

Global keys:
- `q` — quit TUI (daemon continues running)
- `?` — help overlay
- `p` — switch project
- `Tab` / `Shift+Tab` — switch tabs
- `j/k` or arrows — navigate lists
- `Enter` — select/expand
- `/` — filter/search

---

## View 1: Specs

```
┌─ Specs ──────────────────────────────────────────────────┐
│                                                          │
│  Name              Status       Updated                  │
│  ─────────────────────────────────────                   │
│  auth-login        approved     2026-02-07 14:30         │
│  auth-oauth        draft        2026-02-07 15:10         │
│  payments-stripe   approved     2026-02-07 12:00         │
│                                                          │
│  [n]ew  [r]esume  [a]pprove  [v]iew  [d]elete           │
└──────────────────────────────────────────────────────────┘
```

Actions:
- `n` — new spec session. Prompts for: name (text input), "Include codebase access?" (checkbox, equivalent to `--with-codebase`). Opens dialogue view.
- `r` — resume selected draft spec session
- `a` — approve selected spec
- `v` — view spec content (opens in pager sub-view)
- `d` — delete selected spec (prompts for confirmation if spec is `approved`; blocked if `decomposed`)

### Spec Dialogue Sub-View

When creating/resuming a spec, the TUI switches to a dialogue view:

```
┌─ Spec Session: auth-login ───────────────────────────────┐
│                                                          │
│  Claude: What authentication method do you want          │
│  to support? (password, OAuth, magic link, etc.)         │
│                                                          │
│  You: Password-based login with email. We'll add         │
│  OAuth later as a separate spec.                         │
│                                                          │
│  Claude: What password requirements should we            │
│  enforce? (min length, complexity, etc.)                 │
│                                                          │
│  > _                                                     │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  [Enter] send  [Esc] pause session  [Ctrl+D] finish     │
└──────────────────────────────────────────────────────────┘
```

This view shows the conversation in a scrollable area with an input field at the bottom. Claude's questions and user's answers are displayed as a chat.

---

## View 2: Plan

Waves are shown as top-level groups. Each wave displays its status and progress.

```
┌─ Plan ───────────────────────────────────────────────────┐
│                                                          │
│  WAVE 1 [executing] (3/5 stories done)                   │
│  ├─ EPIC-1: Authentication System                        │
│  │  ├─ STORY-1: User Login [done] (MR: !42)             │
│  │  │  ├─ T1  impl   Create login API endpoint [done]    │
│  │  │  ├─ T1v verify  Verify: login endpoint [done]      │
│  │  │  └─ ...                                            │
│  │  ├─ STORY-2: User Registration [running]              │
│  │  │  ├─ T4  impl   Create registration endpoint [done] │
│  │  │  ├─ T4v verify  Verify: registration [in_progress] │
│  │  │  └─ ...                                            │
│  │  └─ STORY-3: OAuth Integration [pending]              │
│  │     │  blocked by: STORY-1                            │
│  │     └─ ...                                            │
│                                                          │
│  WAVE 2 [draft] — reviewing plan                         │
│  ├─ EPIC-2: Payment System                               │
│  │  ├─ STORY-4: Stripe Integration [pending]             │
│  │  │  ├─ T8  impl   Create Stripe client [pending]      │
│  │  │  ├─ T8v verify  Verify: Stripe client [pending]    │
│  │  │  └─ ...                                            │
│  │  └─ STORY-5: Invoice Generation [pending]             │
│  │     └─ ...                                            │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  Waves: 2 │ Stories: 8 │ Tasks: 22                       │
│  [g]enerate [f]eedback [a]pprove [d]iscard [h]ide verify [Enter] details│
└──────────────────────────────────────────────────────────┘
```

Verify tasks are shown with a distinct `verify` label. The `[h]ide verify` toggle can collapse verify tasks for a cleaner view.

Actions:
- `g` — generate new wave from unassigned approved specs
- `f` — give feedback on the current draft wave (opens text input, regenerates plan)
- `a` — approve the current draft wave
- `d` — discard a wave (prompts for confirmation; targets selected wave)
- `h` — toggle visibility of verify tasks (collapse/expand)
- `Enter` — expand selected item, show full description

### Item Detail Popup

```
┌─ T3: Add login form component ───────────────────────────┐
│                                                          │
│  Type:    task (impl)                                    │
│  Status:  pending                                        │
│  Story:   STORY-1: User Login                            │
│  Epic:    EPIC-1: Authentication System                  │
│                                                          │
│  Description:                                            │
│  Create a React login form component with:               │
│  - Email input field with validation                     │
│  - Password input field                                  │
│  - "Remember me" checkbox                                │
│  - Submit button with loading state                      │
│  - Error message display                                 │
│  - Redirect to dashboard on success                      │
│                                                          │
│  Acceptance Criteria:                                    │
│  - Component renders without errors                      │
│  - Email field rejects invalid email format              │
│  - Submit button shows spinner during API call           │
│  - Error message displays on 401 response                │
│  - Successful login redirects to /dashboard              │
│                                                          │
│  [Esc] close                                             │
└──────────────────────────────────────────────────────────┘
```

---

## View 3: Execute

Split view — task tree on left (grouped by wave), active agent output on right.

```
┌─ Execute ────────────────────────────────────────────────┐
│  Running: 3/3  │  Done: 12/28  │  Failed: 0             │
├──────────────────────────┬───────────────────────────────┤
│ TASK LIST                │ AGENT OUTPUT (w1:T5v verify)  │
│                          │                               │
│ ▼ WAVE 1 [executing]    │ > Running: cargo build        │
│ ✅ STORY-1: Login        │   Compiling nflow v0.1.0      │
│   ✅ T1  impl endpoint   │   Finished in 2.3s            │
│   ✅ T1v verify          │ > Running: cargo test         │
│   ✅ T2  impl hashing    │   test validate_email ... ok  │
│   ✅ T2v verify          │   test register_user ... ok   │
│   ✅ T3  impl form       │   12 passed, 0 failed         │
│   ✅ T3v verify          │ > Checking acceptance criteria │
│ 🔄 STORY-2: Register    │   [PASS] POST /register → 201│
│   ✅ T4  impl endpoint   │   [PASS] Invalid email → 400  │
│   ✅ T4v verify          │   [PASS] Duplicate → 409      │
│   ✅ T5  impl validation │ > VERIFICATION PASSED         │
│   🔄 T5v verify         │                               │
│   ⬚  T6  impl confirm   │                               │
│   ⬚  T6v verify         │                               │
│ ⬚  STORY-3: OAuth       │                               │
│                          │                               │
│ ▼ WAVE 2 [executing]    │                               │
│ 🔄 STORY-4: Stripe      │                               │
│   ✅ T8  impl client     │                               │
│   🔄 T8v verify         │                               │
│   ⬚  T9  impl webhook   │                               │
│ ⬚  STORY-5: Invoices    │                               │
│                          │                               │
├──────────────────────────┴───────────────────────────────┤
│ [r]un [s]top [c]ancel [Enter] log [e]scalate [h]ide verify │
└──────────────────────────────────────────────────────────┘
```

Left pane:
- Waves as collapsible top-level groups
- Tree of stories/tasks with status icons within each wave
- Cursor to select items
- Status updates in real-time

Right pane:
- Streams output of the selected task's agent
- Auto-follows latest output
- Shows tool calls in a compact format

Actions:
- `r` — start scheduler / run specific story
- `s` — stop selected running agent
- `c` — cancel selected pending/ready story (sets status to `cancelled`)
- `Enter` — switch to full log view for selected task
- `e` — escalate: client-side shortcut that sends `exec.stop` to daemon, then displays the worktree path so the user can work manually. Not a separate daemon command — just `stop` + show path.
- `h` — toggle visibility of verify tasks

---

## View 4: Logs

Full-screen scrollable log for a specific task.

```
┌─ Log: W1-T5 — Add email validation ─────────────────────┐
│                                                          │
│ [14:32:01] Session started                               │
│ [14:32:02] Reading: src/models/user.rs                   │
│ [14:32:03] Reading: src/routes/register.rs               │
│ [14:32:05] Editing: src/models/user.rs                   │
│            + pub fn validate_email(email: &str) -> bool  │
│ [14:32:08] Writing: src/validators/email.rs              │
│ [14:32:10] Bash: cargo test                              │
│            running 12 tests                              │
│            test validate_email_valid ... ok               │
│            test validate_email_invalid ... ok             │
│            ...                                           │
│ [14:32:15] Bash: git add -A                              │
│ [14:32:15] Bash: git commit -m "Add email validation"    │
│ [14:32:16] Task completed successfully                   │
│                                                          │
├──────────────────────────────────────────────────────────┤
│ [j/k] scroll  [G] bottom  [g] top  [Esc] back           │
└──────────────────────────────────────────────────────────┘
```

Displays parsed agent output:
- Timestamps for each action
- Tool calls with arguments
- Abbreviated file contents for edits
- Command output for bash calls
- Clear success/failure indication

---

## Status Icons

| Icon | Meaning |
|------|---------|
| `⬚` | pending |
| `⏳` | ready (waiting for scheduler) |
| `🔄` | in_progress |
| `✅` | done |
| `❌` | failed |
| `⛔` | cancelled |

Note: In actual terminal rendering, these may be replaced with colored ASCII characters for compatibility:
- `[ ]` pending
- `[~]` ready (yellow)
- `[>]` in progress (blue)
- `[x]` done (green)
- `[!]` failed (red)
- `[-]` cancelled (gray)
