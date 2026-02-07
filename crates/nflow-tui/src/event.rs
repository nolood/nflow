use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, View};

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

    match key.code {
        // Quit
        KeyCode::Char('q') => {
            app.should_quit = true;
            false
        }

        // View switching by number
        KeyCode::Char('1') => {
            app.switch_view(View::Specs);
            true
        }
        KeyCode::Char('2') => {
            app.switch_view(View::Plan);
            true
        }
        KeyCode::Char('3') => {
            app.switch_view(View::Execute);
            true
        }
        KeyCode::Char('4') => {
            app.switch_view(View::Logs);
            true
        }

        // Tab/Shift+Tab for view cycling
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.prev_view();
            } else {
                app.next_view();
            }
            true
        }
        KeyCode::BackTab => {
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
}
