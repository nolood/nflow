use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};
use ratatui::Frame;

use crate::app::{App, DaemonState, Overlay, View};

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

    // Render overlay on top if active
    if let Some(overlay) = &app.overlay {
        render_overlay(overlay, frame, frame.area());
    }
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

    let mut spans = vec![
        Span::styled(format!(" {} ", app.daemon_state.label()), daemon_style),
        Span::raw(" | "),
        Span::styled(
            format!("Project: {}", app.project),
            Style::default().fg(Color::White),
        ),
    ];

    // Current wave
    if let Some(wave) = app.current_wave {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            format!("Wave: {}", wave),
            Style::default().fg(Color::Cyan),
        ));
    }

    // Active agent count
    spans.push(Span::raw(" | "));
    let agent_style = if app.active_agent_count > 0 {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    spans.push(Span::styled(
        format!("Agents: {}", app.active_agent_count),
        agent_style,
    ));

    // Keyboard hints
    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        "q:quit ?:help p:project /:filter Tab:next",
        Style::default().fg(Color::DarkGray),
    ));

    // Status message
    if !app.status_message.is_empty() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            &app.status_message,
            Style::default().fg(Color::Yellow),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compute a centered rect of the given percentage within the provided area.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Render an overlay popup on top of the current view.
fn render_overlay(overlay: &Overlay, frame: &mut Frame, area: Rect) {
    match overlay {
        Overlay::Help => render_help_overlay(frame, area),
        Overlay::ProjectSwitcher => render_project_switcher_overlay(frame, area),
        Overlay::Filter => render_filter_overlay(frame, area),
    }
}

/// Render the help overlay with keybinding reference.
fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let popup_area = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup_area);

    let help_lines = vec![
        Line::from(Span::styled(
            "Keyboard Shortcuts",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  1-4      ", Style::default().fg(Color::Yellow)),
            Span::raw("Switch to Specs/Plan/Execute/Logs view"),
        ]),
        Line::from(vec![
            Span::styled("  Tab      ", Style::default().fg(Color::Yellow)),
            Span::raw("Next view"),
        ]),
        Line::from(vec![
            Span::styled("  Shift+Tab", Style::default().fg(Color::Yellow)),
            Span::raw("Previous view"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ?        ", Style::default().fg(Color::Yellow)),
            Span::raw("Toggle this help"),
        ]),
        Line::from(vec![
            Span::styled("  p        ", Style::default().fg(Color::Yellow)),
            Span::raw("Project switcher"),
        ]),
        Line::from(vec![
            Span::styled("  /        ", Style::default().fg(Color::Yellow)),
            Span::raw("Filter / search"),
        ]),
        Line::from(vec![
            Span::styled("  q        ", Style::default().fg(Color::Yellow)),
            Span::raw("Quit TUI (daemon continues)"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+C   ", Style::default().fg(Color::Yellow)),
            Span::raw("Force quit"),
        ]),
        Line::from(vec![
            Span::styled("  Esc      ", Style::default().fg(Color::Yellow)),
            Span::raw("Close overlay"),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Help")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(help_lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

/// Render the project switcher overlay (placeholder for future implementation).
fn render_project_switcher_overlay(frame: &mut Frame, area: Rect) {
    let popup_area = centered_rect(50, 40, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Project Switcher")
        .title_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Magenta));

    let content = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No projects loaded yet.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Press Esc to close.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(block);

    frame.render_widget(content, popup_area);
}

/// Render the filter overlay (placeholder for future implementation).
fn render_filter_overlay(frame: &mut Frame, area: Rect) {
    let popup_area = centered_rect(50, 30, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Filter")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Yellow));

    let content = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Type to filter...",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Press Esc to close.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(block);

    frame.render_widget(content, popup_area);
}
