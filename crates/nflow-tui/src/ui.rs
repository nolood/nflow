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
        render_overlay(overlay, app, frame, frame.area());
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

    // Render execute sub-views on top
    if app.execute_confirm.is_some() {
        render_execute_confirm_popup(app, frame, frame.area());
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
            render_execute_view(app, frame, area);
        }
        View::Logs => {
            render_logs_view(app, frame, area);
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

    // Get the active filter query (editing query if editing, otherwise applied query)
    let filter_query = if app.filter_state.editing {
        &app.filter_state.query
    } else {
        &app.filter_state.applied_query
    };
    let visible_items = specs.visible_items_filtered(filter_query);

    // Build table rows
    let rows: Vec<Row> = visible_items
        .iter()
        .enumerate()
        .map(|(vi, &(_i, spec))| {
            let color = status_color(&spec.status);
            let name_style = if vi == specs.selected {
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

            if vi == specs.selected {
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

    // Get the active filter query (editing query if editing, otherwise applied query)
    let filter_query = if app.filter_state.editing {
        &app.filter_state.query
    } else {
        &app.filter_state.applied_query
    };
    let visible = tree.visible_nodes_filtered(filter_query);

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

/// Render the execute split view with task tree (left 40%) and agent output (right 60%).
fn render_execute_view(app: &App, frame: &mut Frame, area: Rect) {
    let tree = &app.execute_tree;

    if tree.nodes.is_empty() {
        let block = Block::default().borders(Borders::ALL).title("Execute");
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No execution data available.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Approve a plan and start execution to see progress here.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    // Split: 40% tree, 60% output
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    render_execute_tree(app, frame, chunks[0]);
    render_execute_output(app, frame, chunks[1]);
}

/// Render the execute task tree (left pane).
fn render_execute_tree(app: &App, frame: &mut Frame, area: Rect) {
    let tree = &app.execute_tree;

    let verify_hint = if tree.hide_verify {
        "h:show verify"
    } else {
        "h:hide verify"
    };
    let title = format!("Tasks — j/k:nav Space:collapse {} Enter:log", verify_hint);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD));

    let inner_height = area.height.saturating_sub(2) as usize;

    // Get the active filter query (editing query if editing, otherwise applied query)
    let filter_query = if app.filter_state.editing {
        &app.filter_state.query
    } else {
        &app.filter_state.applied_query
    };
    let visible = tree.visible_nodes_filtered(filter_query);

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

        let indent = "  ".repeat(node.depth as usize);

        let collapse_indicator = if node.has_children {
            if node.collapsed {
                "▸ "
            } else {
                "▾ "
            }
        } else {
            "  "
        };

        let (icon, icon_color) = if node.depth == 0 {
            ("", Color::DarkGray)
        } else {
            status_icon(&node.status)
        };

        let mut spans = Vec::new();

        spans.push(Span::raw(format!("{}{}", indent, collapse_indicator)));

        if !icon.is_empty() {
            spans.push(Span::styled(
                format!("{} ", icon),
                Style::default().fg(icon_color),
            ));
        }

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

        // Story progress and MR URL
        if node.depth == 2 {
            if let Some(ref progress) = node.progress {
                spans.push(Span::styled(
                    format!(" ({})", progress),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(ref mr_url) = node.mr_url {
                spans.push(Span::styled(
                    format!(" MR: {}", mr_url),
                    Style::default().fg(Color::Green),
                ));
            }
        }

        // Task kind
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

/// Render the execute output pane (right pane).
fn render_execute_output(app: &App, frame: &mut Frame, area: Rect) {
    let output = &app.execute_output;

    let title = if let Some(ref task_id) = output.task_id {
        if output.is_streaming {
            format!("Output: {} (live)", task_id)
        } else {
            format!("Output: {} (historical)", task_id)
        }
    } else {
        "Output — select a task to view".to_string()
    };

    let border_color = if output.is_streaming {
        Color::Cyan
    } else {
        Color::White
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(border_color));

    if output.lines.is_empty() {
        let hint = if output.task_id.is_some() {
            "No output available for this task."
        } else {
            "Select a task in the tree and press Enter to view its output."
        };
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2);

    // For streaming: auto-scroll to bottom (offset 0 = bottom)
    // For historical: scroll from top (offset 0 = top)
    let content_lines: Vec<Line> = output
        .lines
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();

    if output.is_streaming {
        // Streaming: show most recent lines, scroll_offset moves up from bottom
        let total = content_lines.len() as u16;
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = max_scroll.saturating_sub(output.scroll_offset);

        let paragraph = Paragraph::new(content_lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    } else {
        // Historical: normal top-to-bottom scroll
        let total = content_lines.len() as u16;
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = output.scroll_offset.min(max_scroll);

        let paragraph = Paragraph::new(content_lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }
}

/// Render the full-screen logs view.
fn render_logs_view(app: &App, frame: &mut Frame, area: Rect) {
    let logs = &app.logs_view;

    // If no task is loaded, show the task list from execute tree
    if logs.task_id.is_none() {
        render_logs_task_list(app, frame, area);
        return;
    }

    let task_id = logs.task_id.as_deref().unwrap_or("?");
    let status_label = logs.task_status.as_deref().unwrap_or("unknown");

    let title = if logs.is_streaming {
        format!(
            "Log: {} [{}] (live) — j/k:line Ctrl+d/u:page g/G:top/bottom Esc:back",
            task_id, status_label
        )
    } else {
        format!(
            "Log: {} [{}] — j/k:line Ctrl+d/u:page g/G:top/bottom Esc:back",
            task_id, status_label
        )
    };

    let border_color = if logs.is_streaming {
        Color::Cyan
    } else if status_label == "failed" {
        Color::Red
    } else {
        Color::White
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(border_color));

    if logs.lines.is_empty() {
        let hint = if logs.is_streaming {
            "Waiting for output..."
        } else {
            "No log content available for this task."
        };
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2);

    // Build styled content lines
    let content_lines: Vec<Line> = logs
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let entry = logs.entries.get(i);
            let style = match entry.map(|e| e.entry_type.as_str()) {
                Some("tool_call") => Style::default().fg(Color::Blue),
                Some("tool_result") => Style::default().fg(Color::Green),
                Some("error") => Style::default().fg(Color::Red),
                _ => Style::default(),
            };
            Line::from(Span::styled(l.as_str(), style))
        })
        .collect();

    if logs.is_streaming {
        // Streaming: show most recent lines, scroll_offset moves up from bottom
        let total = content_lines.len() as u16;
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = max_scroll.saturating_sub(logs.scroll_offset);

        let paragraph = Paragraph::new(content_lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    } else {
        // Historical: normal top-to-bottom scroll
        let total = content_lines.len() as u16;
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = logs.scroll_offset.min(max_scroll);

        let paragraph = Paragraph::new(content_lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }
}

/// Render the task list for the logs view (when no task is selected).
fn render_logs_task_list(app: &App, frame: &mut Frame, area: Rect) {
    let tree = &app.execute_tree;

    if tree.nodes.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Logs — Select a task");
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No tasks available. Start execution first.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let title = "Logs — j/k:navigate Enter:view log";
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().add_modifier(Modifier::BOLD));

    // Show only tasks (depth 3) from the execute tree
    let tasks: Vec<&crate::app::ExecuteTreeNode> =
        tree.nodes.iter().filter(|n| n.depth == 3).collect();

    if tasks.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No tasks found in the execution tree.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize;

    // Get the active filter query (editing query if editing, otherwise applied query)
    let filter_query = if app.filter_state.editing {
        &app.filter_state.query
    } else {
        &app.filter_state.applied_query
    };

    // Use execute_tree.selected for selection tracking in list mode
    let visible = tree.visible_nodes_filtered(filter_query);
    let task_visible: Vec<(usize, &crate::app::ExecuteTreeNode)> = visible
        .iter()
        .filter(|(_, n)| n.depth == 3)
        .copied()
        .collect();

    let selected_vis_idx = tree.selected;

    // Scrolling: compute viewport
    let scroll_offset = if selected_vis_idx >= inner_height {
        selected_vis_idx - inner_height + 1
    } else {
        0
    };

    let lines: Vec<Line> = task_visible
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(inner_height)
        .map(|(vi, (_, node))| {
            let (icon, icon_color) = status_icon(&node.status);
            let kind_label = node
                .kind
                .as_deref()
                .map(|k| format!("[{}]", k))
                .unwrap_or_default();

            let is_selected = vi == selected_vis_idx;
            let style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let prefix = if is_selected { "▶ " } else { "  " };

            Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(format!("{} ", icon), Style::default().fg(icon_color)),
                Span::styled(format!("{} ", node.short_id), style),
                Span::styled(&node.title, style),
                Span::styled(
                    format!(" {}", kind_label),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Get a color based on work item status.
#[allow(dead_code)] // Used by logs task list when needed for standalone coloring
fn status_color_from_status(status: &str) -> Color {
    match status {
        "pending" => Color::DarkGray,
        "ready" => Color::Yellow,
        "in_progress" => Color::Blue,
        "done" => Color::Green,
        "failed" => Color::Red,
        "cancelled" => Color::DarkGray,
        _ => Color::White,
    }
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
    // If filter is active or being edited, show filter bar instead
    if app.filter_state.is_editing() || app.filter_state.is_active() {
        render_filter_bar(app, frame, area);
        return;
    }

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

    // Running counts from execute tree
    let (running, total, done, failed) = app.execute_tree.count_task_stats();
    if total > 0 {
        spans.push(Span::raw(" | "));
        let running_style = if running > 0 {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(
            format!("Running: {}/{}", running, total),
            running_style,
        ));

        spans.push(Span::raw(" | "));
        let done_style = if done > 0 {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!("Done: {}", done), done_style));

        if failed > 0 {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                format!("Failed: {}", failed),
                Style::default().fg(Color::Red),
            ));
        }
    } else {
        // Fallback to agent count when no tasks
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
    }

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

/// Render the filter bar (replaces status bar when filter is active).
fn render_filter_bar(app: &App, frame: &mut Frame, area: Rect) {
    // Get the query to display (editing or applied)
    let query = if app.filter_state.is_editing() {
        &app.filter_state.query
    } else {
        &app.filter_state.applied_query
    };

    // Count matches based on current view
    let (visible, total) = match app.current_view {
        View::Specs => {
            let visible = app.specs_list.visible_items_filtered(query);
            (visible.len(), app.specs_list.items.len())
        }
        View::Plan => {
            let visible = app.plan_tree.visible_nodes_filtered(query);
            (visible.len(), app.plan_tree.nodes.len())
        }
        View::Execute | View::Logs => {
            let visible = app.execute_tree.visible_nodes_filtered(query);
            (visible.len(), app.execute_tree.nodes.len())
        }
    };

    let mut spans = vec![
        Span::styled(" Filter: /", Style::default().fg(Color::Yellow)),
        Span::styled(query, Style::default().fg(Color::White)),
        Span::styled("/ ", Style::default().fg(Color::Yellow)),
    ];

    // Show match count
    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        format!("Showing {}/{} items", visible, total),
        Style::default().fg(Color::Cyan),
    ));

    // Show hints
    spans.push(Span::raw(" | "));
    if app.filter_state.is_editing() {
        spans.push(Span::styled(
            "Enter:apply Esc:cancel",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(
            "Esc:clear /:edit",
            Style::default().fg(Color::DarkGray),
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
fn render_overlay(overlay: &Overlay, app: &App, frame: &mut Frame, area: Rect) {
    match overlay {
        Overlay::Help => render_help_overlay(app.current_view, frame, area),
        Overlay::ProjectSwitcher => render_project_switcher_overlay(app, frame, area),
    }
}

/// Helper to create a keybinding line for the help overlay.
fn help_key(key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {:<11}", key), Style::default().fg(Color::Yellow)),
        Span::raw(desc.to_string()),
    ])
}

/// Helper to create a section header line for the help overlay.
fn help_section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

/// Return keybindings specific to the given view.
fn view_keybindings(view: View) -> Vec<Line<'static>> {
    match view {
        View::Specs => vec![
            help_key("j/k", "Navigate spec list"),
            help_key("n", "New spec"),
            help_key("r", "Resume spec dialogue"),
            help_key("v", "View spec content"),
            help_key("a", "Approve spec"),
            help_key("d", "Delete spec"),
        ],
        View::Plan => vec![
            help_key("j/k", "Navigate plan tree"),
            help_key("Space", "Toggle collapse"),
            help_key("Enter", "View item detail"),
            help_key("h", "Toggle verify tasks"),
            help_key("g", "Generate plan"),
            help_key("f", "Give feedback"),
            help_key("a", "Approve plan"),
            help_key("d", "Discard plan"),
        ],
        View::Execute => vec![
            help_key("j/k", "Navigate tree"),
            help_key("J/K", "Scroll output pane"),
            help_key("Space", "Toggle collapse"),
            help_key("h", "Toggle verify tasks"),
            help_key("Enter", "View task output"),
            help_key("r", "Run execution"),
            help_key("s", "Stop story"),
            help_key("c", "Cancel story"),
            help_key("e", "Escalate story"),
        ],
        View::Logs => vec![
            help_key("j/k", "Navigate task list"),
            help_key("Enter", "View full task log"),
            help_key("Esc", "Exit log detail"),
        ],
    }
}

/// Render the help overlay with view-specific and global keybinding reference.
fn render_help_overlay(current_view: View, frame: &mut Frame, area: Rect) {
    // Dim the background for semi-transparent effect
    let dim_style = Style::default().fg(Color::DarkGray);
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(dim_style);
            }
        }
    }

    let popup_area = centered_rect(60, 70, area);
    frame.render_widget(Clear, popup_area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // View-specific section
    lines.push(help_section(&format!("{} View", current_view.label())));
    lines.push(Line::from(""));
    lines.extend(view_keybindings(current_view));

    // Global section
    lines.push(Line::from(""));
    lines.push(help_section("Global"));
    lines.push(Line::from(""));
    lines.push(help_key("1-4", "Switch view"));
    lines.push(help_key("Tab", "Next view"));
    lines.push(help_key("Shift+Tab", "Previous view"));
    lines.push(help_key("?", "Toggle help"));
    lines.push(help_key("p", "Project switcher"));
    lines.push(help_key("/", "Filter / search"));
    lines.push(help_key("q", "Quit"));
    lines.push(help_key("Ctrl+C", "Force quit"));

    // Dismiss hint
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Press any key to dismiss".to_string(),
        Style::default().fg(Color::DarkGray),
    )));

    let title = format!(" Help - {} ", current_view.label());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

/// Render the project switcher overlay with project list.
fn render_project_switcher_overlay(app: &App, frame: &mut Frame, area: Rect) {
    let popup_area = centered_rect(50, 50, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Project Switcher — j/k:nav Enter:select Esc:cancel ")
        .title_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Magenta));

    let switcher = match &app.project_switcher {
        Some(s) => s,
        None => {
            let content = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Loading projects...",
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(block);
            frame.render_widget(content, popup_area);
            return;
        }
    };

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    if switcher.projects.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No projects found.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, project) in switcher.projects.iter().enumerate() {
            let is_selected = i == switcher.selected;
            let is_current = project.name == app.project;

            let marker = if is_current { "▸ " } else { "  " };
            let agents = if project.active_agent_count > 0 {
                format!(" ({} agents)", project.active_agent_count)
            } else {
                String::new()
            };
            let text = format!("{}{}{}", marker, project.name, agents);

            let style = if is_selected {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else if is_current {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };

            lines.push(Line::from(Span::styled(format!("  {}", text), style)));

            // Show path on the line below in dimmed style
            let path_style = if is_selected {
                Style::default().bg(Color::DarkGray).fg(Color::Gray)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(Span::styled(
                format!("    {}", project.path),
                path_style,
            )));
        }
    }

    lines.push(Line::from(""));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
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
        ConfirmAction::StopStory(_) => Color::Yellow,
        ConfirmAction::CancelStory(_) => Color::Red,
        ConfirmAction::EscalateStory(_) => Color::Red,
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
/// Render a confirmation popup for execute actions (stop, cancel, escalate).
fn render_execute_confirm_popup(app: &App, frame: &mut Frame, area: Rect) {
    let confirm = match &app.execute_confirm {
        Some(c) => c,
        None => return,
    };

    let popup_area = centered_rect(40, 20, area);
    frame.render_widget(Clear, popup_area);

    let border_color = match &confirm.action {
        ConfirmAction::StopStory(_) => Color::Yellow,
        ConfirmAction::CancelStory(_) => Color::Red,
        ConfirmAction::EscalateStory(_) => Color::Red,
        _ => Color::White,
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
