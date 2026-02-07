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

/// Handle a key event, returning true if the app should continue, false to quit.
pub fn handle_key_event(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+C always quits
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return false;
    }

    // Escape closes any open overlay
    if key.code == KeyCode::Esc && app.has_overlay() {
        app.close_overlay();
        return true;
    }

    match key.code {
        // Quit (only when no overlay is open)
        KeyCode::Char('q') if !app.has_overlay() => {
            app.should_quit = true;
            false
        }

        // Overlay toggles (global — work from anywhere)
        KeyCode::Char('?') => {
            app.toggle_overlay(Overlay::Help);
            true
        }
        KeyCode::Char('p') if !app.has_overlay() => {
            app.toggle_overlay(Overlay::ProjectSwitcher);
            true
        }
        KeyCode::Char('/') if !app.has_overlay() => {
            app.toggle_overlay(Overlay::Filter);
            true
        }

        // View switching by number (only when no overlay)
        KeyCode::Char('1') if !app.has_overlay() => {
            app.switch_view(View::Specs);
            true
        }
        KeyCode::Char('2') if !app.has_overlay() => {
            app.switch_view(View::Plan);
            true
        }
        KeyCode::Char('3') if !app.has_overlay() => {
            app.switch_view(View::Execute);
            true
        }
        KeyCode::Char('4') if !app.has_overlay() => {
            app.switch_view(View::Logs);
            true
        }

        // Tab/Shift+Tab for view cycling (only when no overlay)
        KeyCode::Tab if !app.has_overlay() => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.prev_view();
            } else {
                app.next_view();
            }
            true
        }
        KeyCode::BackTab if !app.has_overlay() => {
            app.prev_view();
            true
        }

        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_key_with_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn test_quit_on_q() {
        let mut app = App::new("test".to_string());
        let cont = handle_key_event(&mut app, make_key(KeyCode::Char('q')));
        assert!(!cont);
        assert!(app.should_quit);
    }

    #[test]
    fn test_quit_on_ctrl_c() {
        let mut app = App::new("test".to_string());
        let cont = handle_key_event(
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
        let cont = handle_key_event(&mut app, make_key(KeyCode::Char('x')));
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

        let cont = handle_key_event(&mut app, make_key(KeyCode::Esc));
        assert!(cont);
        assert!(!app.has_overlay());
    }

    #[test]
    fn test_q_does_not_quit_with_overlay() {
        let mut app = App::new("test".to_string());
        app.toggle_overlay(Overlay::Help);

        let cont = handle_key_event(&mut app, make_key(KeyCode::Char('q')));
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
}
