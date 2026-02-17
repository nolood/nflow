# ADR 001: TUI Pipeline Streaming Deduplication and Tool Display

**Date:** 2026-02-11
**Status:** Accepted

---

## Context

The TUI Pipeline tab had four critical streaming display issues affecting user experience:

1. **Duplicate streaming output:** Every streaming line (TextDelta) appeared twice in both the "Output" and "Live Output" panels
2. **Root cause duplication:** Both panels suffered from the same underlying duplication issue
3. **Missing tool visibility:** No display of tool usage (`ToolUse`) or subagent spawning during pipeline execution, making debugging impossible
4. **No auto-scroll in Output panel:** The right Output panel lacked auto-scroll behavior while streaming, while the left Live Output panel correctly scrolled

### Technical Root Cause

The daemon's pipeline execution (`run_pipeline_stage` in `handlers.rs`) sends streaming events through two separate channels:

1. **Direct streaming response:** Each event (TextDelta, ToolUse, ToolResult) is sent as an immediate response line to the requesting client
2. **Event bus broadcast:** The same events are also broadcast via `state.broadcast_event()` for all connected TUI clients to receive

The TUI was subscribed to **both** channels:
- It received streaming responses directly from its pipeline command request
- It simultaneously received the same events via the event bus subscription

This caused every line to be appended twice to the display buffers.

### Race Condition

A subtle race condition exists: the `pipeline_agent_output` event might arrive via the event bus **before** the streaming response completes and sets `self.pipeline_streaming = None`. This could cause the dedup logic to fail if it only checked one condition.

---

## Decision

### 1. Streaming Deduplication

Added deduplication logic in `handle_daemon_event()` in `crates/nflow-tui/src/main.rs`:

```rust
DaemonEvent::PipelineAgentOutput { run_id, detail } => {
    // Skip if we're actively streaming this pipeline run to avoid duplicates.
    // The streaming response channel already appends these events.
    // Check both conditions to handle race: event arrives before streaming flag cleared.
    if self.in_pipeline_streaming() && detail.is_streaming {
        return Ok(());
    }
    // ... rest of handling
}
```

**Two-condition check:**
- `self.in_pipeline_streaming()`: Checks if `pipeline_streaming` is `Some`
- `detail.is_streaming`: Validates the event itself is marked as streaming

This handles the race where the event arrives before the streaming flag is cleared.

### 2. Tool and Subagent Display

**Daemon changes** (`crates/nflow-daemon/src/handlers.rs`):

Forward `ToolUse` and `ToolResult` events through the streaming response and event bus, matching existing TextDelta handling:

```rust
ClaudeEvent::ToolUse(tu) => {
    let detail = AgentOutputDetail {
        output_type: "tool_use".into(),
        text: serde_json::to_string(&tu)?,
        is_streaming: true,
    };
    tx.send(to_ndjson(&detail)?).await?;
    state.broadcast_event(DaemonEvent::PipelineAgentOutput { run_id, detail }).await;
}

ClaudeEvent::ToolResult(tr) => {
    let detail = AgentOutputDetail {
        output_type: "tool_result".into(),
        text: serde_json::to_string(&tr)?,
        is_streaming: true,
    };
    tx.send(to_ndjson(&detail)?).await?;
    state.broadcast_event(DaemonEvent::PipelineAgentOutput { run_id, detail }).await;
}
```

**TUI changes** (`crates/nflow-tui/src/main.rs`):

Added match arms to render tool usage and subagent spawns:

```rust
"tool_use" => {
    if let Ok(tu) = serde_json::from_str::<ToolUse>(&detail.text) {
        let display = if tu.name == "Skill" {
            // Extract subagent description from args
            if let Some(args_obj) = tu.input.as_object() {
                if let Some(desc) = args_obj.get("args").and_then(|v| v.as_str()) {
                    format!("🤖 Subagent: {}", desc)
                } else {
                    format!("🤖 Subagent: {}", tu.name)
                }
            } else {
                format!("🤖 Subagent: {}", tu.name)
            }
        } else {
            format!("🔧 Tool: {}", tu.name)
        };
        self.append_pipeline_output(&display, false);
    }
}

"tool_result" => {
    self.append_pipeline_output("✅ Tool result received", false);
}
```

### 3. Auto-scroll for Output Panel

**TUI changes** (`crates/nflow-tui/src/ui.rs`):

Added scroll-to-bottom logic matching the Live Output panel's behavior:

```rust
// Auto-scroll Output panel during streaming
if app.in_pipeline_streaming() {
    if !app.pipeline_output.is_empty() {
        let line_count = app.pipeline_output.len();
        let visible_lines = output_area.height.saturating_sub(2) as usize;
        if line_count > visible_lines {
            app.pipeline_output_scroll = line_count.saturating_sub(visible_lines);
        }
    }
}
```

---

## Consequences

### Positive

- **No duplicate output:** Streaming lines appear exactly once in both panels
- **Enhanced debugging:** Tool usage and subagent spawning are now visible in real-time
- **Better UX:** Both panels auto-scroll during streaming, keeping latest output visible
- **Clean separation:** Dedup logic is centralized in event handler with clear race condition handling
- **Minimal overhead:** Single conditional check per event, no performance impact

### Negative

- **Fragile coupling:** Dedup relies on coordination between daemon streaming and event bus. If daemon changes one but not the other, duplication could return.
- **Two-source dependency:** The fix depends on understanding that streaming events come from two sources. Future maintainers must preserve this knowledge.

### Mitigations

- **Code comments:** Extensive comments in both `main.rs` (dedup logic) and `handlers.rs` (dual-channel sending) document the relationship
- **Test coverage:** The fix was verified through manual testing of actual pipeline execution with tool usage
- **Documentation:** This ADR serves as the authoritative reference for the streaming architecture

---

## Alternatives Considered

### Alternative 1: Unsubscribe from Event Bus During Streaming

**Approach:** Temporarily unsubscribe the TUI from `pipeline_agent_output` events while actively streaming a pipeline command.

**Rejected because:**
- Complex state management (tracking subscription state per pipeline run)
- Risk of missing events if unsubscribe/resubscribe timing is wrong
- Doesn't solve the architectural issue (dual-channel sending)

### Alternative 2: Daemon Sends Only to Event Bus

**Approach:** Remove direct streaming responses, rely solely on event bus for all clients.

**Rejected because:**
- Breaking change for CLI clients expecting direct streaming responses
- Event bus is broadcast—all clients receive all events, requiring client-side filtering
- Direct streaming is semantically correct for request-response pattern

### Alternative 3: Daemon Sends Only Direct Streaming

**Approach:** Remove event bus broadcast for streaming events, use only direct responses.

**Rejected because:**
- Other TUI clients (not the one that started the pipeline) wouldn't see real-time updates
- Breaks multi-client monitoring use case
- Event bus broadcast is needed for system-wide observability

---

## Files Changed

- `crates/nflow-tui/src/main.rs` — Dedup logic, tool/subagent display, race condition handling
- `crates/nflow-tui/src/ui.rs` — Auto-scroll for Output panel
- `crates/nflow-daemon/src/handlers.rs` — ToolUse/ToolResult streaming event forwarding

---

## References

- Issue discovered during manual testing of Pipeline interactive mode (PRD: `tasks/prd-pipeline-interactive-mode.md`)
- Related to daemon-client protocol: `docs/daemon.md`
- TUI architecture: `docs/tui.md`
