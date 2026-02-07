use crate::error::Result;
use crate::socket_client::{ResponseStatus, SocketClient};

/// A single spec entry for display in the specs list view.
#[derive(Debug, Clone)]
pub struct SpecItem {
    pub name: String,
    pub status: String,
    #[allow(dead_code)] // Used by future spec dialogue/resume views
    pub session_active: bool,
    pub created_at: String,
}

/// State for the specs list view.
#[derive(Debug)]
pub struct SpecsListState {
    pub items: Vec<SpecItem>,
    pub selected: usize,
}

impl SpecsListState {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
        }
    }

    /// Move selection up.
    pub fn select_prev(&mut self) {
        if !self.items.is_empty() && self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if !self.items.is_empty() && self.selected < self.items.len() - 1 {
            self.selected += 1;
        }
    }

    /// Get the currently selected item (used by action handlers in future stories).
    #[allow(dead_code)]
    pub fn selected_item(&self) -> Option<&SpecItem> {
        self.items.get(self.selected)
    }

    /// Update specs from daemon response data.
    pub fn update_from_response(&mut self, data: &serde_json::Value) {
        if let Some(specs) = data.get("specs").and_then(|v| v.as_array()) {
            self.items = specs
                .iter()
                .map(|s| SpecItem {
                    name: s
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: s
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    session_active: s
                        .get("session_active")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    created_at: s
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect();

            // Clamp selection to new bounds
            if self.items.is_empty() {
                self.selected = 0;
            } else if self.selected >= self.items.len() {
                self.selected = self.items.len() - 1;
            }
        }
    }
}

/// A single message in the spec dialogue chat.
#[derive(Debug, Clone)]
pub struct DialogueMessage {
    /// Who sent this message ("Claude" or "You").
    pub sender: String,
    /// The message text.
    pub text: String,
}

/// State of the spec dialogue streaming session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogueSessionState {
    /// Streaming output from Claude (input disabled).
    Streaming,
    /// Waiting for user input (input enabled).
    WaitingForInput,
    /// Session completed or ended.
    Completed,
}

/// State for the spec dialogue sub-view.
#[derive(Debug)]
pub struct SpecDialogueState {
    /// Chat messages history.
    pub messages: Vec<DialogueMessage>,
    /// Current user input buffer.
    pub input: String,
    /// Current session state (streaming, waiting for input, completed).
    pub session_state: DialogueSessionState,
    /// The spec ID for this dialogue (set once streaming starts).
    pub spec_id: Option<String>,
    /// The spec name being dialogued.
    pub spec_name: String,
    /// Streaming request ID (to validate response lines).
    pub request_id: Option<String>,
    /// Scroll offset for the message area (0 = bottom, auto-scroll).
    pub scroll_offset: u16,
}

impl SpecDialogueState {
    /// Create a new dialogue state for the given spec.
    pub fn new(spec_name: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            session_state: DialogueSessionState::Streaming,
            spec_id: None,
            spec_name,
            request_id: None,
            scroll_offset: 0,
        }
    }

    /// Add a message from Claude.
    pub fn add_claude_message(&mut self, text: String) {
        if let Some(last) = self.messages.last_mut() {
            if last.sender == "Claude" {
                // Append to existing Claude message for contiguous text events
                last.text.push_str(&text);
                self.scroll_offset = 0; // Auto-scroll
                return;
            }
        }
        self.messages.push(DialogueMessage {
            sender: "Claude".to_string(),
            text,
        });
        self.scroll_offset = 0;
    }

    /// Add a message from the user.
    pub fn add_user_message(&mut self, text: String) {
        self.messages.push(DialogueMessage {
            sender: "You".to_string(),
            text,
        });
        self.scroll_offset = 0;
    }

    /// Scroll up in the message area.
    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    /// Scroll down in the message area.
    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
}

/// Overlay that can be displayed on top of the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Help,
    ProjectSwitcher,
    Filter,
}

/// The active view in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Specs,
    Plan,
    Execute,
    Logs,
}

impl View {
    pub fn label(&self) -> &'static str {
        match self {
            View::Specs => "Specs",
            View::Plan => "Plan",
            View::Execute => "Execute",
            View::Logs => "Logs",
        }
    }

    pub fn all() -> &'static [View] {
        &[View::Specs, View::Plan, View::Execute, View::Logs]
    }

    pub fn index(&self) -> usize {
        match self {
            View::Specs => 0,
            View::Plan => 1,
            View::Execute => 2,
            View::Logs => 3,
        }
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            0 => View::Specs,
            1 => View::Plan,
            2 => View::Execute,
            3 => View::Logs,
            _ => View::Specs,
        }
    }
}

/// Connection state to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Connected,
    Disconnected,
    Connecting,
}

impl DaemonState {
    pub fn label(&self) -> &'static str {
        match self {
            DaemonState::Connected => "Connected",
            DaemonState::Disconnected => "Disconnected",
            DaemonState::Connecting => "Connecting...",
        }
    }
}

/// Main application state for the TUI.
pub struct App {
    /// Current active view.
    pub current_view: View,
    /// Project name.
    pub project: String,
    /// Daemon connection state.
    pub daemon_state: DaemonState,
    /// Whether the app should exit.
    pub should_quit: bool,
    /// Status message for the status bar.
    pub status_message: String,
    /// Currently active overlay (if any).
    pub overlay: Option<Overlay>,
    /// Current wave number (from daemon status).
    pub current_wave: Option<u32>,
    /// Number of active agents (from daemon status).
    pub active_agent_count: u32,
    /// Specs list view state.
    pub specs_list: SpecsListState,
    /// Active spec dialogue state (if in dialogue sub-view).
    pub spec_dialogue: Option<SpecDialogueState>,
}

impl App {
    /// Create a new App with the given project name.
    pub fn new(project: String) -> Self {
        Self {
            current_view: View::Specs,
            project,
            daemon_state: DaemonState::Disconnected,
            should_quit: false,
            status_message: String::new(),
            overlay: None,
            current_wave: None,
            active_agent_count: 0,
            specs_list: SpecsListState::new(),
            spec_dialogue: None,
        }
    }

    /// Switch to the next view (Tab).
    pub fn next_view(&mut self) {
        let idx = self.current_view.index();
        let next = (idx + 1) % View::all().len();
        self.current_view = View::from_index(next);
    }

    /// Switch to the previous view (Shift+Tab).
    pub fn prev_view(&mut self) {
        let idx = self.current_view.index();
        let prev = if idx == 0 {
            View::all().len() - 1
        } else {
            idx - 1
        };
        self.current_view = View::from_index(prev);
    }

    /// Switch to a specific view by number key (1-4).
    pub fn switch_view(&mut self, view: View) {
        self.current_view = view;
    }

    /// Mark daemon as connected and update status.
    pub fn set_connected(&mut self) {
        self.daemon_state = DaemonState::Connected;
        self.status_message.clear();
    }

    /// Mark daemon as disconnected.
    pub fn set_disconnected(&mut self, message: &str) {
        self.daemon_state = DaemonState::Disconnected;
        self.status_message = message.to_string();
    }

    /// Toggle an overlay. If the same overlay is already open, close it.
    /// If a different overlay is open, switch to the new one.
    pub fn toggle_overlay(&mut self, overlay: Overlay) {
        if self.overlay == Some(overlay) {
            self.overlay = None;
        } else {
            self.overlay = Some(overlay);
        }
    }

    /// Close any open overlay.
    pub fn close_overlay(&mut self) {
        self.overlay = None;
    }

    /// Returns true if any overlay is currently shown.
    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    /// Returns true if a spec dialogue is active.
    pub fn in_dialogue(&self) -> bool {
        self.spec_dialogue.is_some()
    }

    /// Enter spec dialogue mode for a given spec.
    pub fn enter_dialogue(&mut self, spec_name: String) {
        self.spec_dialogue = Some(SpecDialogueState::new(spec_name));
    }

    /// Exit spec dialogue mode and return to specs list.
    pub fn exit_dialogue(&mut self) {
        self.spec_dialogue = None;
    }

    /// Attempt to connect to the daemon and fetch initial state.
    pub async fn connect(&mut self, client: &mut SocketClient) -> Result<()> {
        self.daemon_state = DaemonState::Connecting;

        // Verify connection by sending a status command
        let resp = client
            .send_command(
                "exec.status",
                serde_json::json!({ "project_name": &self.project }),
            )
            .await;

        match resp {
            Ok(r) if r.status == ResponseStatus::Ok => {
                self.set_connected();
                // Parse wave and agent count from status response
                if let Some(wave) = r.data.get("current_wave").and_then(|v| v.as_u64()) {
                    self.current_wave = Some(wave as u32);
                }
                if let Some(count) = r.data.get("active_agents").and_then(|v| v.as_u64()) {
                    self.active_agent_count = count as u32;
                }
            }
            Ok(r) => {
                // Connected but command failed — still connected to daemon
                let msg = r.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
                // NOT_FOUND for project is ok — daemon is running, project just not initialized
                self.set_connected();
                if !msg.is_empty() {
                    self.status_message = msg.to_string();
                }
            }
            Err(e) => {
                self.set_disconnected(&e.to_string());
                return Err(e);
            }
        }

        // Fetch specs list
        self.fetch_specs(client).await.ok();

        Ok(())
    }

    /// Fetch the specs list from the daemon.
    pub async fn fetch_specs(&mut self, client: &mut SocketClient) -> Result<()> {
        let resp = client
            .send_command(
                "spec.list",
                serde_json::json!({ "project_name": &self.project }),
            )
            .await?;

        if resp.status == ResponseStatus::Ok {
            self.specs_list.update_from_response(&resp.data);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_cycle() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

        app.next_view();
        assert_eq!(app.current_view, View::Plan);

        app.next_view();
        assert_eq!(app.current_view, View::Execute);

        app.next_view();
        assert_eq!(app.current_view, View::Logs);

        app.next_view();
        assert_eq!(app.current_view, View::Specs);
    }

    #[test]
    fn test_view_cycle_backward() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

        app.prev_view();
        assert_eq!(app.current_view, View::Logs);

        app.prev_view();
        assert_eq!(app.current_view, View::Execute);
    }

    #[test]
    fn test_switch_view_by_number() {
        let mut app = App::new("test".to_string());
        app.switch_view(View::Execute);
        assert_eq!(app.current_view, View::Execute);

        app.switch_view(View::Specs);
        assert_eq!(app.current_view, View::Specs);
    }

    #[test]
    fn test_daemon_state_transitions() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.daemon_state, DaemonState::Disconnected);

        app.set_connected();
        assert_eq!(app.daemon_state, DaemonState::Connected);
        assert!(app.status_message.is_empty());

        app.set_disconnected("connection lost");
        assert_eq!(app.daemon_state, DaemonState::Disconnected);
        assert_eq!(app.status_message, "connection lost");
    }

    #[test]
    fn test_view_labels() {
        assert_eq!(View::Specs.label(), "Specs");
        assert_eq!(View::Plan.label(), "Plan");
        assert_eq!(View::Execute.label(), "Execute");
        assert_eq!(View::Logs.label(), "Logs");
    }

    #[test]
    fn test_daemon_state_labels() {
        assert_eq!(DaemonState::Connected.label(), "Connected");
        assert_eq!(DaemonState::Disconnected.label(), "Disconnected");
        assert_eq!(DaemonState::Connecting.label(), "Connecting...");
    }

    #[test]
    fn test_toggle_overlay() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.overlay, None);

        app.toggle_overlay(Overlay::Help);
        assert_eq!(app.overlay, Some(Overlay::Help));

        // Toggle same overlay closes it
        app.toggle_overlay(Overlay::Help);
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn test_toggle_overlay_switches() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);
        assert_eq!(app.overlay, Some(Overlay::Help));

        // Toggle different overlay switches to it
        app.toggle_overlay(Overlay::ProjectSwitcher);
        assert_eq!(app.overlay, Some(Overlay::ProjectSwitcher));
    }

    #[test]
    fn test_close_overlay() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Filter);
        assert!(app.has_overlay());

        app.close_overlay();
        assert!(!app.has_overlay());
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn test_initial_wave_and_agents() {
        let app = App::new("test".to_string());
        assert_eq!(app.current_wave, None);
        assert_eq!(app.active_agent_count, 0);
    }

    // --- SpecsListState tests ---

    #[test]
    fn test_specs_list_initial_state() {
        let state = SpecsListState::new();
        assert!(state.items.is_empty());
        assert_eq!(state.selected, 0);
        assert!(state.selected_item().is_none());
    }

    #[test]
    fn test_specs_list_navigation() {
        let mut state = SpecsListState::new();
        state.items = vec![
            SpecItem {
                name: "a".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "2026-01-01".to_string(),
            },
            SpecItem {
                name: "b".to_string(),
                status: "approved".to_string(),
                session_active: false,
                created_at: "2026-01-02".to_string(),
            },
        ];

        assert_eq!(state.selected, 0);
        assert_eq!(state.selected_item().unwrap().name, "a");

        state.select_next();
        assert_eq!(state.selected, 1);
        assert_eq!(state.selected_item().unwrap().name, "b");

        // Can't go past end
        state.select_next();
        assert_eq!(state.selected, 1);

        state.select_prev();
        assert_eq!(state.selected, 0);

        // Can't go past start
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_specs_list_navigation_empty() {
        let mut state = SpecsListState::new();
        // Navigation on empty list should not panic
        state.select_next();
        assert_eq!(state.selected, 0);
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_specs_list_update_from_response() {
        let mut state = SpecsListState::new();
        let data = serde_json::json!({
            "specs": [
                {
                    "name": "auth-spec",
                    "status": "draft",
                    "session_active": true,
                    "created_at": "2026-02-07T15:30:00+00:00"
                },
                {
                    "name": "payment-spec",
                    "status": "approved",
                    "session_active": false,
                    "created_at": "2026-02-07T16:00:00+00:00"
                }
            ]
        });

        state.update_from_response(&data);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].name, "auth-spec");
        assert_eq!(state.items[0].status, "draft");
        assert!(state.items[0].session_active);
        assert_eq!(state.items[1].name, "payment-spec");
        assert_eq!(state.items[1].status, "approved");
    }

    #[test]
    fn test_specs_list_update_clamps_selection() {
        let mut state = SpecsListState::new();
        state.items = vec![
            SpecItem {
                name: "a".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
            SpecItem {
                name: "b".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
            SpecItem {
                name: "c".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "".to_string(),
            },
        ];
        state.selected = 2;

        // Update with fewer items should clamp selection
        let data = serde_json::json!({
            "specs": [
                { "name": "only-one", "status": "draft", "session_active": false, "created_at": "" }
            ]
        });
        state.update_from_response(&data);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_specs_list_update_empty_response() {
        let mut state = SpecsListState::new();
        let data = serde_json::json!({ "specs": [] });
        state.update_from_response(&data);
        assert!(state.items.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_initial_specs_list() {
        let app = App::new("test".to_string());
        assert!(app.specs_list.items.is_empty());
        assert_eq!(app.specs_list.selected, 0);
    }

    // --- SpecDialogueState tests ---

    #[test]
    fn test_dialogue_initial_state() {
        let state = SpecDialogueState::new("test-spec".to_string());
        assert!(state.messages.is_empty());
        assert!(state.input.is_empty());
        assert_eq!(state.session_state, DialogueSessionState::Streaming);
        assert_eq!(state.spec_name, "test-spec");
        assert!(state.spec_id.is_none());
        assert!(state.request_id.is_none());
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_dialogue_add_claude_message() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_claude_message("Hello!".to_string());
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].sender, "Claude");
        assert_eq!(state.messages[0].text, "Hello!");
    }

    #[test]
    fn test_dialogue_add_user_message() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_user_message("My answer".to_string());
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].sender, "You");
        assert_eq!(state.messages[0].text, "My answer");
    }

    #[test]
    fn test_dialogue_claude_message_coalescing() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_claude_message("Part 1".to_string());
        state.add_claude_message(" Part 2".to_string());
        // Contiguous Claude messages should be merged
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].text, "Part 1 Part 2");
    }

    #[test]
    fn test_dialogue_message_interleaving() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.add_claude_message("Question?".to_string());
        state.add_user_message("Answer!".to_string());
        state.add_claude_message("Thanks!".to_string());
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0].sender, "Claude");
        assert_eq!(state.messages[1].sender, "You");
        assert_eq!(state.messages[2].sender, "Claude");
    }

    #[test]
    fn test_dialogue_scroll() {
        let mut state = SpecDialogueState::new("test".to_string());
        assert_eq!(state.scroll_offset, 0);

        state.scroll_up();
        assert_eq!(state.scroll_offset, 1);

        state.scroll_up();
        assert_eq!(state.scroll_offset, 2);

        state.scroll_down();
        assert_eq!(state.scroll_offset, 1);

        state.scroll_down();
        assert_eq!(state.scroll_offset, 0);

        // Can't go below 0
        state.scroll_down();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_dialogue_auto_scroll_on_new_message() {
        let mut state = SpecDialogueState::new("test".to_string());
        state.scroll_offset = 5;
        state.add_claude_message("New message".to_string());
        // Auto-scroll resets offset to 0
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_app_dialogue_lifecycle() {
        let mut app = App::new("test".to_string());
        assert!(!app.in_dialogue());
        assert!(app.spec_dialogue.is_none());

        app.enter_dialogue("my-spec".to_string());
        assert!(app.in_dialogue());
        assert_eq!(app.spec_dialogue.as_ref().unwrap().spec_name, "my-spec");

        app.exit_dialogue();
        assert!(!app.in_dialogue());
        assert!(app.spec_dialogue.is_none());
    }
}
