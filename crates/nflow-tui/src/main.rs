mod app;
mod daemon_client;
mod error;
mod event;
mod socket_client;
mod terminal;
mod ui;

use std::env;
use std::time::Duration;

use clap::Parser;
use crossterm::event::Event;

use app::App;
use daemon_client::ensure_daemon;
use error::TuiError;
use socket_client::SocketClient;

/// nflow TUI — interactive terminal interface for nflow orchestrator
#[derive(Debug, Parser)]
#[command(name = "nflow-tui", version, about)]
struct Args {
    /// Override project name (default: detected from current directory)
    #[arg(long)]
    project: Option<String>,
}

/// Resolve the project name from CLI flag or auto-detection from cwd.
fn resolve_project(flag: &Option<String>) -> error::Result<String> {
    if let Some(name) = flag {
        return Ok(name.clone());
    }
    let cwd = env::current_dir().map_err(|e| {
        TuiError::Io(std::io::Error::new(
            e.kind(),
            "failed to determine current directory",
        ))
    })?;
    let dir_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    Ok(dir_name.to_string())
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if let Err(e) = run(args).await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn run(args: Args) -> error::Result<()> {
    let project = resolve_project(&args.project)?;

    // Ensure daemon is running (auto-start if needed)
    eprintln!("Starting daemon...");
    ensure_daemon()?;

    // Connect to daemon
    let mut client = SocketClient::connect().await?;

    // Create app state
    let mut app = App::new(project);

    // Try initial connection
    app.connect(&mut client).await.ok();

    // Set up terminal
    let mut tui = terminal::setup()?;

    // Main event loop
    let result = event_loop(&mut tui, &mut app).await;

    // Always restore terminal on exit
    terminal::restore(&mut tui)?;

    result
}

/// Main event loop: render frame, poll events, handle input.
async fn event_loop(tui: &mut terminal::Tui, app: &mut App) -> error::Result<()> {
    loop {
        // Render current frame
        tui.draw(|frame| ui::render(app, frame))
            .map_err(|e| TuiError::Terminal(format!("failed to draw frame: {}", e)))?;

        // Poll for input events (16ms ≈ 60fps)
        let evt = event::poll_event(Duration::from_millis(16))
            .map_err(|e| TuiError::Terminal(format!("failed to poll events: {}", e)))?;

        if let Some(Event::Key(key)) = evt {
            if !event::handle_key_event(app, key) {
                break;
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_project_explicit() {
        let result = resolve_project(&Some("my-project".to_string())).unwrap();
        assert_eq!(result, "my-project");
    }

    #[test]
    fn test_resolve_project_auto() {
        let result = resolve_project(&None).unwrap();
        assert!(!result.is_empty());
    }
}
