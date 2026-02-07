use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::Frame;

use crate::app::{App, ConfirmAction, DaemonState, DialogueSessionState, Overlay, View};

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

    // Render plan sub-views on top
    if app.plan_generate.is_some() {
        render_generate_dialog(app, frame, frame.area());
    } else if app.plan_feedback.is_some() {
        render_feedback_input(app, frame, frame.area());
    } else if app.plan_confirm.is_some() {
        render_confirm_popup(app, frame, frame.area());
    } else if app.plan_detail.is_some() {
        render_detail_popup(app, frame, frame.area());
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
    match app.current_view {
        View::Specs => {
            if app.spec_pager.is_some() {
                render_spec_pager(app, frame, area);
            } else if app.spec_dialogue.is_some() {
                render_spec_dialogue(app, frame, area);
            } else {
                render_specs_list(app, frame, area);
            }
        }
        View::Plan => {
            render_plan_tree(app, frame, area);
        }
        View::Execute => {
            let block = Block::default().borders(Borders::ALL).title("Execute");
            let paragraph = Paragraph::new("Execute view — shows running agents").block(block);
            frame.render_widget(paragraph, area);
        }
        View::Logs => {
            let block = Block::default().borders(Borders::ALL).title("Logs");
            let paragraph = Paragraph::new("Logs view — shows agent output").block(block);
            frame.render_widget(paragraph, area);
        }
    }
}

/// Color for a spec status badge.
fn status_color(status: &str) -> Color {
    match status {
        "draft" => Color::Yellow,
        "approved" => Color::Green,
        "decomposed" => Color::Blue,
        _ => Color::DarkGray,
    }
}

/// Format a created_at timestamp for display (date only).
fn format_timestamp(ts: &str) -> String {
    // created_at comes as RFC 3339, e.g. "2026-02-07T15:30:00+00:00"
    // Show just the date portion for the compact list
    if ts.len() >= 10 {
        ts[..10].to_string()
    } else {
        ts.to_string()
    }
}

/// Render the specs list view with a table of specs.
fn render_specs_list(app: &App, frame: &mut Frame, area: Rect) {
    let specs = &app.specs_list;

    if specs.items.is_empty() {
        let block = Block::default().borders(Borders::ALL).title("Specs");
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No specs found. Press 'n' to create a new spec.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Keybindings: n:new  a:approve  d:delete  v:view  r:resume",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    // Build table rows
    let rows: Vec<Row> = specs
        .items
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let color = status_color(&spec.status);
            let name_style = if i == specs.selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let row = Row::new(vec![
                Line::from(Span::styled(&*spec.name, name_style)),
                Line::from(Span::styled(
                    &*spec.status,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format_timestamp(&spec.created_at),
                    Style::default().fg(Color::DarkGray),
                )),
            ]);

            if i == specs.selected {
                row.style(Style::default().bg(Color::DarkGray))
            } else {
                row
            }
        })
        .collect();

    let header = Row::new(vec![
        Line::from(Span::styled(
            "Name",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Status",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Created",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ])
    .style(Style::default().bg(Color::Black))
    .bottom_margin(0);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Specs")
        .title_style(Style::default().add_modifier(Modifier::BOLD));

    let widths = [
        Constraint::Percentage(50),
        Constraint::Percentage(20),
        Constraint::Percentage(30),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1);

    frame.render_widget(table, area);
}

/// Status icon for a work item status string.
fn status_icon(status: &str) -> (&str, Color) {
    match status {
        "pending" => ("○", Color::DarkGray),
        "ready" => ("●", Color::White),
        "in_progress" => ("▶", Color::Cyan),
        "done" => ("✓", Color::Green),
        "failed" => ("✗", Color::Red),
        "cancelled" => ("⊘", Color::DarkGray),
        _ => (" ", Color::DarkGray),
    }
}

/// Render the plan tree view with collapsible hierarchy.
fn render_plan_tree(app: &App, frame: &mut Frame, area: Rect) {
    let tree = &app.plan_tree;

    if tree.nodes.is_empty() {
        let block = Block::default().borders(Borders::ALL).title("Plan");
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No decomposition waves found.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Use 'nflow plan generate' to create a plan.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let verify_hint = if tree.hide_verify {
        "h:show verify"
    } else {
        "h:hide verify"
    };
    let title = format!(
        "Plan — j/k:nav Space:collapse Enter:detail {} g:gen f:feedback a:approve d:discard",
        verify_hint
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD));

    let inner_height = area.height.saturating_sub(2) as usize;
    let visible = tree.visible_nodes();

    // Scroll offset to keep selection visible
    let scroll_offset = if tree.selected >= inner_height {
        tree.selected - inner_height + 1
    } else {
        0
    };

    let mut lines: Vec<Line> = Vec::new();
    for (vi, &(_, node)) in visible
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(inner_height)
    {
        let is_selected = vi == tree.selected;

        // Build indentation
        let indent = "  ".repeat(node.depth as usize);

        // Collapse indicator
        let collapse_indicator = if node.has_children {
            if node.collapsed {
                "▸ "
            } else {
                "▾ "
            }
        } else {
            "  "
        };

        // Status icon
        let (icon, icon_color) = if node.depth == 0 {
            // Wave node - no status icon
            ("", Color::DarkGray)
        } else {
            status_icon(&node.status)
        };

        // Build the line spans
        let mut spans = Vec::new();

        // Indent + collapse
        spans.push(Span::raw(format!("{}{}", indent, collapse_indicator)));

        // Status icon
        if !icon.is_empty() {
            spans.push(Span::styled(
                format!("{} ", icon),
                Style::default().fg(icon_color),
            ));
        }

        // Short ID
        let id_color = match node.depth {
            0 => Color::Yellow,
            1 => Color::Magenta,
            2 => Color::Cyan,
            _ => Color::DarkGray,
        };
        spans.push(Span::styled(
            &node.short_id,
            Style::default().fg(id_color).add_modifier(Modifier::BOLD),
        ));

        spans.push(Span::raw(" "));

        // Title
        let title_style = if node.depth == 3 {
            if node.kind.as_deref() == Some("verify") {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default().fg(Color::White)
            }
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(&*node.title, title_style));

        // Story-specific: progress + dependencies
        if node.depth == 2 {
            if let Some(ref progress) = node.progress {
                spans.push(Span::styled(
                    format!(" ({})", progress),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !node.depends_on.is_empty() {
                spans.push(Span::styled(
                    format!(" [blocks: {}]", node.depends_on.join(", ")),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }

        // Task kind indicator
        if node.depth == 3 {
            if let Some(ref kind) = node.kind {
                let kind_style = if kind == "verify" {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Blue)
                };
                spans.push(Span::styled(format!(" [{}]", kind), kind_style));
            }
        }

        let mut line = Line::from(spans);
        if is_selected {
            line = line.style(Style::default().bg(Color::DarkGray));
        }
        lines.push(line);
    }

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the spec content pager sub-view.
fn render_spec_pager(app: &App, frame: &mut Frame, area: Rect) {
    let pager = match &app.spec_pager {
        Some(p) => p,
        None => return,
    };

    let title = format!(
        "Spec: {} (q/Esc to close, j/k scroll, Ctrl+d/u page, g/G top/bottom)",
        pager.spec_name
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD));

    // Calculate visible height (area minus border)
    let inner_height = area.height.saturating_sub(2);

    // Clamp scroll offset to valid range
    let max_scroll = pager.total_lines.saturating_sub(inner_height);
    let scroll = pager.scroll_offset.min(max_scroll);

    // Build lines with wrapping support
    let content_lines: Vec<Line> = pager.lines.iter().map(|l| Line::from(l.as_str())).collect();

    let paragraph = Paragraph::new(content_lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

/// Render the spec dialogue sub-view with chat history and input field.
fn render_spec_dialogue(app: &App, frame: &mut Frame, area: Rect) {
    let dialogue = match &app.spec_dialogue {
        Some(d) => d,
        None => return,
    };

    // Split: 80% chat history, 20% input area (min 3 lines for input)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(80), // chat history
            Constraint::Percentage(20), // input area
        ])
        .split(area);

    // Ensure input area has at least 3 rows
    let (chat_area, input_area) = if chunks[1].height < 3 {
        let input_height = 3u16.min(area.height);
        let chat_height = area.height.saturating_sub(input_height);
        (
            Rect::new(area.x, area.y, area.width, chat_height),
            Rect::new(area.x, area.y + chat_height, area.width, input_height),
        )
    } else {
        (chunks[0], chunks[1])
    };

    // --- Chat history ---
    let title = format!("Spec: {} — Dialogue", dialogue.spec_name);
    let chat_block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD));

    let mut chat_lines: Vec<Line> = Vec::new();
    for msg in &dialogue.messages {
        let (prefix_style, text_style) = if msg.sender == "Claude" {
            (
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            )
        } else {
            (
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            )
        };

        let prefix = format!("{}: ", msg.sender);
        let mut first = true;
        for text_line in msg.text.lines() {
            if first {
                chat_lines.push(Line::from(vec![
                    Span::styled(prefix.clone(), prefix_style),
                    Span::styled(text_line, text_style),
                ]));
                first = false;
            } else {
                // Continuation lines indented by prefix width
                let indent = " ".repeat(prefix.len());
                chat_lines.push(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(text_line, text_style),
                ]));
            }
        }
        if msg.text.is_empty() {
            chat_lines.push(Line::from(vec![Span::styled(prefix, prefix_style)]));
        }
        // Blank line between messages
        chat_lines.push(Line::from(""));
    }

    // Add streaming indicator if currently streaming
    if dialogue.session_state == DialogueSessionState::Streaming {
        chat_lines.push(Line::from(Span::styled(
            "Claude is thinking...",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    // Calculate scroll: auto-scroll to bottom, offset adjusts
    let inner_height = chat_area.height.saturating_sub(2); // borders
    let total_lines = chat_lines.len() as u16;
    let max_scroll = total_lines.saturating_sub(inner_height);
    let scroll = max_scroll.saturating_sub(dialogue.scroll_offset);

    let chat = Paragraph::new(chat_lines)
        .block(chat_block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(chat, chat_area);

    // --- Input area ---
    let input_title = match dialogue.session_state {
        DialogueSessionState::WaitingForInput => {
            "Your answer (Enter to send, Ctrl+D to end, Esc to exit)"
        }
        DialogueSessionState::Streaming => "Waiting for Claude...",
        DialogueSessionState::Completed => "Session completed (Esc to exit)",
    };

    let input_border_color = match dialogue.session_state {
        DialogueSessionState::WaitingForInput => Color::Green,
        DialogueSessionState::Streaming => Color::Yellow,
        DialogueSessionState::Completed => Color::DarkGray,
    };

    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(input_title)
        .title_style(Style::default().fg(input_border_color))
        .border_style(Style::default().fg(input_border_color));

    let input_text = if dialogue.session_state == DialogueSessionState::WaitingForInput {
        format!("{}_", dialogue.input) // Show cursor
    } else {
        dialogue.input.clone()
    };

    let input = Paragraph::new(input_text)
        .block(input_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(input, input_area);
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

/// Render the plan generate dialog (select specs + with_codebase checkbox).
fn render_generate_dialog(app: &App, frame: &mut Frame, area: Rect) {
    let gen = match &app.plan_generate {
        Some(g) => g,
        None => return,
    };

    let popup_area = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Generate Plan — Space:toggle j/k:nav c:codebase Enter:submit Esc:cancel")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));

    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled(
        " Select specs to decompose:",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    if gen.specs.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No approved specs available.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, spec) in gen.specs.iter().enumerate() {
            let checkbox = if spec.selected { "[x]" } else { "[ ]" };
            let is_cursor = i == gen.cursor;
            let style = if is_cursor {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(
                format!("   {} {}", checkbox, spec.name),
                style,
            )));
        }
    }

    lines.push(Line::from(""));

    // --with-codebase checkbox
    let codebase_check = if gen.with_codebase { "[x]" } else { "[ ]" };
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            format!("{} Include codebase context (c to toggle)", codebase_check),
            Style::default().fg(Color::Yellow),
        ),
    ]));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

/// Render the plan feedback input popup.
fn render_feedback_input(app: &App, frame: &mut Frame, area: Rect) {
    let fb = match &app.plan_feedback {
        Some(f) => f,
        None => return,
    };

    let popup_area = centered_rect(60, 30, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Plan Feedback — Enter:submit Esc:cancel")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Yellow));

    let lines = vec![
        Line::from(Span::styled(
            " Enter feedback for the current plan:",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" > {}_", fb.input),
            Style::default().fg(Color::Green),
        )),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

/// Render the confirmation popup (approve/discard).
fn render_confirm_popup(app: &App, frame: &mut Frame, area: Rect) {
    let confirm = match &app.plan_confirm {
        Some(c) => c,
        None => return,
    };

    let popup_area = centered_rect(40, 20, area);
    frame.render_widget(Clear, popup_area);

    let border_color = match confirm.action {
        ConfirmAction::ApprovePlan => Color::Green,
        ConfirmAction::DiscardPlan => Color::Red,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Confirm")
        .title_style(
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(border_color));

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", confirm.action.message()),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  y", Style::default().fg(Color::Green)),
            Span::styled(" = Yes    ", Style::default().fg(Color::DarkGray)),
            Span::styled("n", Style::default().fg(Color::Red)),
            Span::styled(" = No    ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::DarkGray)),
            Span::styled(" = Cancel", Style::default().fg(Color::DarkGray)),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

/// Render the plan node detail popup.
fn render_detail_popup(app: &App, frame: &mut Frame, area: Rect) {
    let detail = match &app.plan_detail {
        Some(d) => d,
        None => return,
    };

    let popup_area = centered_rect(60, 50, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            "Detail: {} — press any key to close",
            detail.short_id
        ))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));

    let (icon, icon_color) = status_icon(&detail.status);
    let depth_label = match detail.depth {
        0 => "Wave",
        1 => "Epic",
        2 => "Story",
        3 => "Task",
        _ => "Item",
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Type: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(depth_label, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled(
                "  ID:   ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &detail.short_id,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "  Title: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&detail.title, Style::default().fg(Color::White)),
        ]),
    ];

    if !detail.status.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "  Status: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{} ", icon), Style::default().fg(icon_color)),
            Span::styled(&detail.status, Style::default().fg(icon_color)),
        ]));
    }

    if let Some(ref progress) = detail.progress {
        lines.push(Line::from(vec![
            Span::styled(
                "  Progress: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(progress, Style::default().fg(Color::White)),
        ]));
    }

    if let Some(ref kind) = detail.kind {
        lines.push(Line::from(vec![
            Span::styled(
                "  Kind: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                kind,
                Style::default().fg(if kind == "verify" {
                    Color::DarkGray
                } else {
                    Color::Blue
                }),
            ),
        ]));
    }

    if !detail.depends_on.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "  Dependencies: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                detail.depends_on.join(", "),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
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
