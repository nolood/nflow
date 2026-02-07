use crate::error::Result;
use crate::socket_client::{ResponseStatus, SocketClient};

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
}
