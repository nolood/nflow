use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::Frame;

use crate::app::{App, DaemonState, View};

/// Render the entire TUI frame.
pub fn render(app: &App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header with tabs
            Constraint::Min(1),    // main content
            Constraint::Length(1), // status bar
        ])
        .split(frame.area());

    render_header(app, frame, chunks[0]);
    render_content(app, frame, chunks[1]);
    render_status_bar(app, frame, chunks[2]);
}

/// Render the header with view tabs.
fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let titles: Vec<Line> = View::all()
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let num = format!("{}", i + 1);
            let label = v.label();
            Line::from(vec![
                Span::styled(
                    num,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(":"),
                Span::raw(label),
            ])
        })
        .collect();

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("nflow"))
        .select(app.current_view.index())
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw(" | "));

    frame.render_widget(tabs, area);
}

/// Render the main content area based on the active view.
fn render_content(app: &App, frame: &mut Frame, area: Rect) {
    let content = match app.current_view {
        View::Specs => "Specs view — press 'n' to create a new spec",
        View::Plan => "Plan view — shows decomposition waves",
        View::Execute => "Execute view — shows running agents",
        View::Logs => "Logs view — shows agent output",
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(app.current_view.label());

    let paragraph = Paragraph::new(content).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the status bar at the bottom.
fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let daemon_style = match app.daemon_state {
        DaemonState::Connected => Style::default().fg(Color::Green),
        DaemonState::Disconnected => Style::default().fg(Color::Red),
        DaemonState::Connecting => Style::default().fg(Color::Yellow),
    };

    let spans = vec![
        Span::styled(format!(" {} ", app.daemon_state.label()), daemon_style),
        Span::raw(" | "),
        Span::styled(
            format!("Project: {}", app.project),
            Style::default().fg(Color::White),
        ),
        Span::raw(" | "),
        Span::styled(
            "q:quit ?:help Tab:next view",
            Style::default().fg(Color::DarkGray),
        ),
    ];

    if !app.status_message.is_empty() {
        let mut s = spans;
        s.push(Span::raw(" | "));
        s.push(Span::styled(
            &app.status_message,
            Style::default().fg(Color::Yellow),
        ));
        frame.render_widget(Paragraph::new(Line::from(s)), area);
    } else {
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}
