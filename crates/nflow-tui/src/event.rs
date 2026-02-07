use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Overlay, View};

/// Poll for a crossterm event with the given timeout.
///
/// Returns `Some(Event)` if an event is available, `None` on timeout.
pub fn poll_event(timeout: Duration) -> std::io::Result<Option<Event>> {
    if event::poll(timeout)? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}

/// Actions that views can request in response to key events.
/// The main loop processes these (e.g., sending daemon commands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewAction {
    /// No action needed.
    None,
    /// Request to create a new spec.
    SpecNew,
    /// Request to approve the selected spec.
    SpecApprove,
    /// Request to delete the selected spec.
    SpecDelete,
    /// Request to view the selected spec.
    SpecView,
    /// Request to resume the selected spec session.
    SpecResume,
}

/// Handle a key event, returning true if the app should continue, false to quit.
/// Also returns a ViewAction if the view requests an async operation.
pub fn handle_key_event(app: &mut App, key: KeyEvent) -> (bool, ViewAction) {
    // Ctrl+C always quits
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return (false, ViewAction::None);
    }

    // Escape closes any open overlay
    if key.code == KeyCode::Esc && app.has_overlay() {
        app.close_overlay();
        return (true, ViewAction::None);
    }

    match key.code {
        // Quit (only when no overlay is open)
        KeyCode::Char('q') if !app.has_overlay() => {
            app.should_quit = true;
            (false, ViewAction::None)
        }

        // Overlay toggles (global — work from anywhere)
        KeyCode::Char('?') => {
            app.toggle_overlay(Overlay::Help);
            (true, ViewAction::None)
        }
        KeyCode::Char('p') if !app.has_overlay() => {
            app.toggle_overlay(Overlay::ProjectSwitcher);
            (true, ViewAction::None)
        }
        KeyCode::Char('/') if !app.has_overlay() => {
            app.toggle_overlay(Overlay::Filter);
            (true, ViewAction::None)
        }

        // View switching by number (only when no overlay)
        KeyCode::Char('1') if !app.has_overlay() => {
            app.switch_view(View::Specs);
            (true, ViewAction::None)
        }
        KeyCode::Char('2') if !app.has_overlay() => {
            app.switch_view(View::Plan);
            (true, ViewAction::None)
        }
        KeyCode::Char('3') if !app.has_overlay() => {
            app.switch_view(View::Execute);
            (true, ViewAction::None)
        }
        KeyCode::Char('4') if !app.has_overlay() => {
            app.switch_view(View::Logs);
            (true, ViewAction::None)
        }

        // Tab/Shift+Tab for view cycling (only when no overlay)
        KeyCode::Tab if !app.has_overlay() => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.prev_view();
            } else {
                app.next_view();
            }
            (true, ViewAction::None)
        }
        KeyCode::BackTab if !app.has_overlay() => {
            app.prev_view();
            (true, ViewAction::None)
        }

        // View-specific keybindings (only when no overlay)
        _ if !app.has_overlay() => {
            let action = handle_view_key(app, key);
            (true, action)
        }

        _ => (true, ViewAction::None),
    }
}

/// Handle view-specific key events based on the current view.
fn handle_view_key(app: &mut App, key: KeyEvent) -> ViewAction {
    match app.current_view {
        View::Specs => handle_specs_key(app, key),
        _ => ViewAction::None,
    }
}

/// Handle key events in the Specs list view.
fn handle_specs_key(app: &mut App, key: KeyEvent) -> ViewAction {
    match key.code {
        // Navigation
        KeyCode::Up | KeyCode::Char('k') => {
            app.specs_list.select_prev();
            ViewAction::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.specs_list.select_next();
            ViewAction::None
        }

        // Actions
        KeyCode::Char('n') => ViewAction::SpecNew,
        KeyCode::Char('a') => ViewAction::SpecApprove,
        KeyCode::Char('d') => ViewAction::SpecDelete,
        KeyCode::Char('v') => ViewAction::SpecView,
        KeyCode::Char('r') => ViewAction::SpecResume,

        _ => ViewAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SpecItem;

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_key_with_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn test_quit_on_q() {
        let mut app = App::new("test".to_string());
        let (cont, _) = handle_key_event(&mut app, make_key(KeyCode::Char('q')));
        assert!(!cont);
        assert!(app.should_quit);
    }

    #[test]
    fn test_quit_on_ctrl_c() {
        let mut app = App::new("test".to_string());
        let (cont, _) = handle_key_event(
            &mut app,
            make_key_with_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(!cont);
        assert!(app.should_quit);
    }

    #[test]
    fn test_number_keys_switch_views() {
        let mut app = App::new("test".to_string());

        handle_key_event(&mut app, make_key(KeyCode::Char('2')));
        assert_eq!(app.current_view, View::Plan);

        handle_key_event(&mut app, make_key(KeyCode::Char('3')));
        assert_eq!(app.current_view, View::Execute);

        handle_key_event(&mut app, make_key(KeyCode::Char('4')));
        assert_eq!(app.current_view, View::Logs);

        handle_key_event(&mut app, make_key(KeyCode::Char('1')));
        assert_eq!(app.current_view, View::Specs);
    }

    #[test]
    fn test_tab_cycles_views() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

        handle_key_event(&mut app, make_key(KeyCode::Tab));
        assert_eq!(app.current_view, View::Plan);

        handle_key_event(&mut app, make_key(KeyCode::Tab));
        assert_eq!(app.current_view, View::Execute);
    }

    #[test]
    fn test_backtab_cycles_backward() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.current_view, View::Specs);

        handle_key_event(&mut app, make_key(KeyCode::BackTab));
        assert_eq!(app.current_view, View::Logs);
    }

    #[test]
    fn test_unknown_key_continues() {
        let mut app = App::new("test".to_string());
        let (cont, _) = handle_key_event(&mut app, make_key(KeyCode::Char('x')));
        assert!(cont);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_question_mark_toggles_help() {
        let mut app = App::new("test".to_string());
        assert_eq!(app.overlay, None);

        handle_key_event(&mut app, make_key(KeyCode::Char('?')));
        assert_eq!(app.overlay, Some(Overlay::Help));

        handle_key_event(&mut app, make_key(KeyCode::Char('?')));
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn test_p_opens_project_switcher() {
        let mut app = App::new("test".to_string());
        handle_key_event(&mut app, make_key(KeyCode::Char('p')));
        assert_eq!(app.overlay, Some(Overlay::ProjectSwitcher));
    }

    #[test]
    fn test_slash_opens_filter() {
        let mut app = App::new("test".to_string());
        handle_key_event(&mut app, make_key(KeyCode::Char('/')));
        assert_eq!(app.overlay, Some(Overlay::Filter));
    }

    #[test]
    fn test_escape_closes_overlay() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);
        assert!(app.has_overlay());

        let (cont, _) = handle_key_event(&mut app, make_key(KeyCode::Esc));
        assert!(cont);
        assert!(!app.has_overlay());
    }

    #[test]
    fn test_q_does_not_quit_with_overlay() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);

        let (cont, _) = handle_key_event(&mut app, make_key(KeyCode::Char('q')));
        assert!(cont);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_number_keys_blocked_with_overlay() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);

        handle_key_event(&mut app, make_key(KeyCode::Char('2')));
        // View should not change when overlay is open
        assert_eq!(app.current_view, View::Specs);
    }

    // --- Specs view keybindings ---

    fn app_with_specs() -> App {
        let mut app = App::new("test".to_string());
        app.specs_list.items = vec![
            SpecItem {
                name: "auth-spec".to_string(),
                status: "draft".to_string(),
                session_active: false,
                created_at: "2026-02-07T15:30:00+00:00".to_string(),
            },
            SpecItem {
                name: "payment-spec".to_string(),
                status: "approved".to_string(),
                session_active: false,
                created_at: "2026-02-07T16:00:00+00:00".to_string(),
            },
            SpecItem {
                name: "search-spec".to_string(),
                status: "decomposed".to_string(),
                session_active: false,
                created_at: "2026-02-07T17:00:00+00:00".to_string(),
            },
        ];
        app
    }

    #[test]
    fn test_specs_arrow_down_navigates() {
        let mut app = app_with_specs();
        assert_eq!(app.specs_list.selected, 0);

        handle_key_event(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.specs_list.selected, 1);

        handle_key_event(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.specs_list.selected, 2);

        // Should not go past the last item
        handle_key_event(&mut app, make_key(KeyCode::Down));
        assert_eq!(app.specs_list.selected, 2);
    }

    #[test]
    fn test_specs_arrow_up_navigates() {
        let mut app = app_with_specs();
        app.specs_list.selected = 2;

        handle_key_event(&mut app, make_key(KeyCode::Up));
        assert_eq!(app.specs_list.selected, 1);

        handle_key_event(&mut app, make_key(KeyCode::Up));
        assert_eq!(app.specs_list.selected, 0);

        // Should not go past the first item
        handle_key_event(&mut app, make_key(KeyCode::Up));
        assert_eq!(app.specs_list.selected, 0);
    }

    #[test]
    fn test_specs_jk_navigates() {
        let mut app = app_with_specs();
        assert_eq!(app.specs_list.selected, 0);

        handle_key_event(&mut app, make_key(KeyCode::Char('j')));
        assert_eq!(app.specs_list.selected, 1);

        handle_key_event(&mut app, make_key(KeyCode::Char('k')));
        assert_eq!(app.specs_list.selected, 0);
    }

    #[test]
    fn test_specs_n_returns_spec_new_action() {
        let mut app = app_with_specs();
        let (cont, action) = handle_key_event(&mut app, make_key(KeyCode::Char('n')));
        assert!(cont);
        assert_eq!(action, ViewAction::SpecNew);
    }

    #[test]
    fn test_specs_a_returns_spec_approve_action() {
        let mut app = app_with_specs();
        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Char('a')));
        assert_eq!(action, ViewAction::SpecApprove);
    }

    #[test]
    fn test_specs_d_returns_spec_delete_action() {
        let mut app = app_with_specs();
        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Char('d')));
        assert_eq!(action, ViewAction::SpecDelete);
    }

    #[test]
    fn test_specs_v_returns_spec_view_action() {
        let mut app = app_with_specs();
        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Char('v')));
        assert_eq!(action, ViewAction::SpecView);
    }

    #[test]
    fn test_specs_r_returns_spec_resume_action() {
        let mut app = app_with_specs();
        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Char('r')));
        assert_eq!(action, ViewAction::SpecResume);
    }

    #[test]
    fn test_specs_keys_blocked_with_overlay() {
        let mut app = app_with_specs();
        app.toggle_overlay(Overlay::Help);

        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Char('n')));
        assert_eq!(action, ViewAction::None);

        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Down));
        assert_eq!(action, ViewAction::None);
        assert_eq!(app.specs_list.selected, 0);
    }

    #[test]
    fn test_specs_keys_only_in_specs_view() {
        let mut app = app_with_specs();
        app.switch_view(View::Plan);

        let (_, action) = handle_key_event(&mut app, make_key(KeyCode::Char('n')));
        assert_eq!(action, ViewAction::None);
    }
}
