use std::io::{self, Stdout};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::error::{Result, TuiError};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Set up the terminal for TUI rendering.
///
/// Enables raw mode, enters alternate screen, enables mouse capture,
/// and returns a ready-to-use Terminal.
pub fn setup() -> Result<Tui> {
    enable_raw_mode()
        .map_err(|e| TuiError::Terminal(format!("failed to enable raw mode: {}", e)))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| TuiError::Terminal(format!("failed to enter alternate screen: {}", e)))?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)
        .map_err(|e| TuiError::Terminal(format!("failed to create terminal: {}", e)))?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
///
/// Disables raw mode, leaves alternate screen, and shows the cursor.
/// This must be called before exit to avoid leaving the terminal in a broken state.
pub fn restore(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()
        .map_err(|e| TuiError::Terminal(format!("failed to disable raw mode: {}", e)))?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .map_err(|e| TuiError::Terminal(format!("failed to leave alternate screen: {}", e)))?;
    terminal
        .show_cursor()
        .map_err(|e| TuiError::Terminal(format!("failed to show cursor: {}", e)))?;
    Ok(())
}
