# TUI Test Notes

## Bug 1: Goal description truncated in New Pipeline dialog
- **Screenshot**: Goal text "Найти какой-нибудь модуль не покрытый unit тестами и покры..." gets cut off
- Text doesn't wrap or scroll, just truncated at the edge of the dialog box
- Expected: text should either wrap to multiple lines or allow horizontal scrolling

## Bug 2: Plan Approval dialog shows "Done." immediately after pipeline start
- After starting a new pipeline, the Plan Approval dialog appears almost instantly
- Plan Summary shows only "Done." — no actual plan content
- Expected: the planning agent should run, produce a real plan, and only THEN the approval dialog should appear
- Live Output panel shows the system prompt being streamed — the agent prompt itself, not agent output
- Status bar says "Awaiting plan approval" immediately

## Bug 3: Plan Approval overlay ignores keyboard input (a/r/Esc do nothing)
- When Plan Approval overlay is shown, pressing 'a' (approve), 'r' (reject), or Esc produces zero reaction
- The UI is stuck — no way to interact with the approval dialog
- Status bar says "Awaiting plan approval" but user cannot proceed
- **Ctrl+C тоже не работает** — TUI полностью зависает, невозможно выйти
- Вероятно event loop заблокирован (не обрабатывает crossterm events вообще)

## Bug 4: Cannot create new pipeline while previous one is in waiting_for_approval state
- Old pipeline stuck in "waiting_f plan" status (status text also truncated in table)
- Pressing 'n' to create a new pipeline does nothing — no dialog appears
- TUI не позволяет создать новый pipeline пока предыдущий завис в waiting_for_approval
- Also: status "waiting_f" is truncated — column too narrow to show "waiting_for_approval"

## Bug 5: Duplicate Claude response in Specs dialogue view
- In Specs dialogue, Claude's response is displayed twice — identical text block repeated
- Both blocks contain the same questions (1. Какой контекст покрывать тестами? 2. Какая глубина покрытия?)
- Likely the same message is appended to the dialogue buffer twice
- Happens before user's reply ("You: 1. projects 2. да")

## Bug 6: No visibility into agent activity after user replies in Specs dialogue
- After user sends reply ("1. projects 2. да"), only "Claude:" label appears with empty content
- "Waiting for Claude..." shown at bottom, but no streaming output visible
- Expected: should see live agent activity — tool calls (Read, Glob, Grep etc.), subagent spawns, intermediate text
- User has zero visibility into what the agent is doing — just a blank waiting state
- Need to stream all tool_use, tool_result, text_delta events into the dialogue view in real time

## Bug 7: Spec dialogue session lost — "Session completed" with empty Claude response
- After waiting for Claude's response (bug 6), pressing Escape and returning shows:
  - "Claude:" with empty content after user's reply
  - Bottom panel says "Session completed (Esc to exit)" — session ended prematurely
- The agent's second response was never captured/displayed
- Session appears to have completed without actually finishing the dialogue

## Bug 8: INVALID_STATE error when trying to resume spec dialogue
- After re-entering the spec dialogue, error appears:
  - `[Error: INVALID_STATE: spec 'Найти не покрытый юнит тестами модуль и покрыть его' has no previous session to resume]`
- The session_id was lost — daemon cannot resume the Claude session
- Expected: session should persist and be resumable, or at minimum show the last known state

## Bug 9: Status bar hints truncated — not fully visible
- Bottom status bar: "Enter:detail Space:collapse g:gen a:appr" — text cut off at right edge
- "a:appr" is truncated (should be "a:approve"), and likely more hints after it are not visible
- Status bar doesn't scroll/wrap, just clips at terminal width

## Bug 10: Pipeline detail view is a dead end — no controls, no info
- Pipeline detail for stuck run shows only "Stage: plan (iteration 1)" and "Summary: Done."
- Live Output says "Waiting for output..." — nothing happening
- **No controls available**: can't cancel, can't retry, can't approve/reject from here, can't view logs
- **No information**: no run ID, no timestamps, no goal text, no error details, no agent logs
- Title truncated: "Ещё больше тестов — St..." — status cut off
- This is a dead end — user is stuck looking at a useless view with no way to act
- **Needed**: cancel button, retry, view full log, see goal/config, approve/reject if waiting, delete run
