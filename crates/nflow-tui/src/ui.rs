use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::Frame;

use crate::app::{
    App, ConfirmAction, DaemonState, DecompositionPhase, DialogueSessionState, Overlay, View,
};

/// Braille spinner frames for animated progress indicators.
const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Get the current spinner character based on wall-clock time.
fn spinner_char() -> char {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let frame_index = (millis / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[frame_index]
}

/// Convert a markdown string into styled ratatui Lines.
///
/// Supports: headers (#), code blocks (```), inline code (`), bold (**),
/// italic (*), and list items (- / *).
fn markdown_to_lines(text: &str, base_style: Style) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;

    for raw_line in text.lines() {
        // Code block toggle
        if raw_line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                // Show language hint if present
                let lang = raw_line.trim_start().trim_start_matches('`').trim();
                if lang.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "───",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("─── {} ───", lang),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "───",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            continue;
        }

        if in_code_block {
            lines.push(Line::from(Span::styled(
                format!("  {}", raw_line),
                Style::default().fg(Color::Green),
            )));
            continue;
        }

        // Headers
        if raw_line.starts_with("### ") {
            lines.push(Line::from(Span::styled(
                raw_line[4..].to_string(),
                base_style.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if raw_line.starts_with("## ") {
            lines.push(Line::from(Span::styled(
                raw_line[3..].to_string(),
                base_style.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if raw_line.starts_with("# ") {
            lines.push(Line::from(Span::styled(
                raw_line[2..].to_string(),
                base_style
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }

        // Horizontal rules
        let trimmed = raw_line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            lines.push(Line::from(Span::styled(
                "────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }

        // List items: render bullet
        let (list_prefix, content) = if raw_line.starts_with("- ") {
            ("  • ".to_string(), &raw_line[2..])
        } else if raw_line.starts_with("* ") {
            ("  • ".to_string(), &raw_line[2..])
        } else if raw_line.len() > 2
            && raw_line.as_bytes()[0].is_ascii_digit()
            && raw_line[1..].starts_with(". ")
        {
            (
                format!("  {}. ", raw_line.as_bytes()[0] as char),
                &raw_line[3..],
            )
        } else {
            (String::new(), raw_line)
        };

        // Parse inline styles: **bold**, *italic*, `code`
        let spans = parse_inline_markdown(content, base_style);

        if list_prefix.is_empty() {
            lines.push(Line::from(spans));
        } else {
            let mut all_spans = vec![Span::styled(list_prefix, base_style)];
            all_spans.extend(spans);
            lines.push(Line::from(all_spans));
        }
    }

    lines
}

/// Parse inline markdown (bold, italic, code) into styled Spans.
fn parse_inline_markdown(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut buf = String::new();

    while let Some(&(i, c)) = chars.peek() {
        match c {
            '`' => {
                // Flush buffer
                if !buf.is_empty() {
                    spans.push(Span::styled(buf.clone(), base_style));
                    buf.clear();
                }
                chars.next();
                // Collect until closing `
                let mut code = String::new();
                while let Some(&(_, ch)) = chars.peek() {
                    if ch == '`' {
                        chars.next();
                        break;
                    }
                    code.push(ch);
                    chars.next();
                }
                spans.push(Span::styled(code, Style::default().fg(Color::Green)));
            }
            '*' => {
                // Check for ** (bold) or * (italic)
                let rest = &text[i..];
                if rest.starts_with("**") {
                    // Flush buffer
                    if !buf.is_empty() {
                        spans.push(Span::styled(buf.clone(), base_style));
                        buf.clear();
                    }
                    chars.next();
                    chars.next();
                    // Collect until **
                    let mut bold_text = String::new();
                    while let Some(&(j, ch)) = chars.peek() {
                        if text[j..].starts_with("**") {
                            chars.next();
                            chars.next();
                            break;
                        }
                        bold_text.push(ch);
                        chars.next();
                    }
                    spans.push(Span::styled(
                        bold_text,
                        base_style.add_modifier(Modifier::BOLD),
                    ));
                } else {
                    // Single * = italic
                    if !buf.is_empty() {
                        spans.push(Span::styled(buf.clone(), base_style));
                        buf.clear();
                    }
                    chars.next();
                    let mut italic_text = String::new();
                    while let Some(&(_, ch)) = chars.peek() {
                        if ch == '*' {
                            chars.next();
                            break;
                        }
                        italic_text.push(ch);
                        chars.next();
                    }
                    spans.push(Span::styled(
                        italic_text,
                        base_style.add_modifier(Modifier::ITALIC),
                    ));
                }
            }
            _ => {
                buf.push(c);
                chars.next();
            }
        }
    }

    if !buf.is_empty() {
        spans.push(Span::styled(buf, base_style));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }

    spans
}

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

    // Render spec name input popup on top
    if app.spec_name_input.is_some() {
        render_spec_name_input(app, frame, frame.area());
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

    // Render pipeline new dialog on top
    if app.pipeline_new.is_some() {
        render_pipeline_new(app, frame, frame.area());
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
        View::Pipeline => {
            if app.pipeline_detail.is_some() {
                render_pipeline_detail(app, frame, area);
            } else {
                render_pipeline_list(app, frame, area);
            }
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
    // If viewing decomposition output, render that instead
    if app.viewing_decomposition_output
        && (app.is_decomposition_active() || !app.decomposition_output.lines.is_empty())
    {
        render_decomposition_output(app, frame, area);
        return;
    }

    let tree = &app.plan_tree;

    if tree.nodes.is_empty() {
        // When the tree is empty, render based on decomposition phase
        match &app.decomposition_phase {
            DecompositionPhase::Starting
            | DecompositionPhase::Analyzing
            | DecompositionPhase::BuildingItems => {
                render_decomposition_progress(app, frame, area);
                return;
            }
            DecompositionPhase::Completed(summary) => {
                let block = Block::default().borders(Borders::ALL).title("Plan");
                let text = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!(
                            "  \u{2713} Plan generation complete! {} epics, {} stories, {} tasks",
                            summary.epic_count, summary.story_count, summary.task_count
                        ),
                        Style::default().fg(Color::Green),
                    )),
                ];
                let para = Paragraph::new(text).block(block);
                frame.render_widget(para, area);
                return;
            }
            DecompositionPhase::Failed(msg) => {
                let block = Block::default().borders(Borders::ALL).title("Plan");
                let text = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  \u{2717} Plan generation failed: {}", msg),
                        Style::default().fg(Color::Red),
                    )),
                ];
                let para = Paragraph::new(text).block(block);
                frame.render_widget(para, area);
                return;
            }
            DecompositionPhase::Idle => {
                let block = Block::default().borders(Borders::ALL).title("Plan");
                let text = vec![
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
                ];
                let empty = Paragraph::new(text).block(block);
                frame.render_widget(empty, area);
                return;
            }
        }
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
    let title = format!(
        "Tasks — j/k:nav Space:collapse {} Enter:select",
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

/// Render the decomposition output view (full-screen streaming logs).
fn render_decomposition_output(app: &App, frame: &mut Frame, area: Rect) {
    let output = &app.decomposition_output;

    let specs_label = if !app.plan_tree.spec_names.is_empty() {
        format!(" [{}]", app.plan_tree.spec_names.join(", "))
    } else {
        String::new()
    };

    let title = if output.is_streaming {
        format!(
            " Plan Generation{} (live) — Esc:back j/k:scroll ",
            specs_label
        )
    } else {
        format!(" Plan Generation{} — Esc:back j/k:scroll ", specs_label)
    };

    let border_color = if output.is_streaming {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    let inner_height = inner.height as u16;

    let content_lines: Vec<Line> = if output.lines.is_empty() && output.is_streaming {
        vec![Line::from(Span::styled(
            "  Waiting for Claude output...",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ))]
    } else {
        output
            .lines
            .iter()
            .map(|l| format_decomposition_line(l))
            .collect()
    };

    let total = content_lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = max_scroll.saturating_sub(output.scroll_offset);

    let paragraph = Paragraph::new(content_lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

/// Render an inline progress panel for active decomposition (when tree is empty).
fn render_decomposition_progress(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Plan");
    let inner = block.inner(area);

    let spinner = spinner_char();

    let (phase_text, phase_color) = match &app.decomposition_phase {
        DecompositionPhase::Starting => ("Starting decomposition...", Color::Yellow),
        DecompositionPhase::Analyzing => ("Claude is analyzing specs...", Color::Cyan),
        DecompositionPhase::BuildingItems => ("Building work items...", Color::Cyan),
        _ => ("Processing...", Color::DarkGray),
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(format!("  {} ", spinner), Style::default().fg(phase_color)),
        Span::styled(
            phase_text,
            Style::default()
                .fg(phase_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  (V: fullscreen, Esc: dismiss preview)",
        Style::default().fg(Color::DarkGray),
    )));

    // Show last few lines of streaming output as an inline preview with styling,
    // unless the user has dismissed the preview
    if !app.streaming_preview_dismissed {
        let output_lines = &app.decomposition_output.lines;
        // Skip empty lines in preview to save space
        let non_empty: Vec<&String> = output_lines.iter().filter(|l| !l.is_empty()).collect();
        let preview_count = 6.min(non_empty.len());
        if preview_count > 0 {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  --- streaming preview ---",
                Style::default().fg(Color::DarkGray),
            )));
            let start = non_empty.len().saturating_sub(preview_count);
            // Collect truncated strings so they live long enough for borrowing
            let max_width = (inner.width as usize).saturating_sub(4);
            let preview_strings: Vec<String> = non_empty[start..]
                .iter()
                .map(|line_text| {
                    if line_text.len() > max_width {
                        let cut = max_width.saturating_sub(3).min(line_text.len());
                        format!("{}...", &line_text[..cut])
                    } else {
                        line_text.to_string()
                    }
                })
                .collect();
            for s in &preview_strings {
                // Use format_decomposition_line for consistent styling
                lines.push(format_decomposition_line(s));
            }
        }
    }

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Format a decomposition log line with appropriate styling.
/// Takes ownership of the line string to produce an owned `Line<'static>`.
fn format_decomposition_line(line: &str) -> Line<'static> {
    let styled =
        |text: String, style: Style| -> Line<'static> { Line::from(Span::styled(text, style)) };

    if line.starts_with("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} Tool: ") {
        // Tool separator line: ───── Tool: bash ─────
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
    } else if line.starts_with("  \u{21B3} ") {
        // Result arrow: ↳ output...
        styled(line.to_string(), Style::default().fg(Color::Green))
    } else if line == "  \u{22EF} typing..." {
        // Typing indicator
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    } else if line.starts_with("    ... (") && line.ends_with(" more lines)") {
        // Truncation indicator for results
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    } else if line.starts_with("  ... (") && line.ends_with(" more params)") {
        // Truncation indicator for params
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    } else if line.starts_with("  \u{21B3} (no output)") {
        // Empty result
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )
    } else if line.starts_with("  ") && line.contains(": ") && !line.starts_with("    ") {
        // Indented tool params (2-space indent, key: value)
        // But not result continuation lines (4-space indent)
        if let Some(pos) = line.find(": ") {
            // Safety: `pos` is from str::find(": ") which returns valid byte positions,
            // and ": " is ASCII, so pos+1 is always a valid UTF-8 boundary.
            let key_part = line[..pos + 1].to_string(); // includes the colon
            let val_part = line[pos + 1..].to_string(); // includes leading space + value
            Line::from(vec![
                Span::styled(key_part, Style::default().fg(Color::Cyan)),
                Span::styled(val_part, Style::default().fg(Color::White)),
            ])
        } else {
            styled(line.to_string(), Style::default().fg(Color::Cyan))
        }
    } else if line.starts_with("    ") {
        // Result continuation lines (4-space indent)
        styled(line.to_string(), Style::default().fg(Color::Green))
    } else if line.starts_with("[Error:") || line.starts_with("[Parse error:") {
        styled(
            line.to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if line.starts_with("[Decomposition complete]") {
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else if line.starts_with("[Connection lost]") {
        styled(
            line.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if line.starts_with("[Tool: ") {
        // Legacy format (backwards compatibility)
        styled(line.to_string(), Style::default().fg(Color::Cyan))
    } else if line.starts_with("[Tool input: ") || line.starts_with("[Result]") {
        // Legacy format (backwards compatibility)
        styled(line.to_string(), Style::default().fg(Color::DarkGray))
    } else {
        // Claude's reasoning text — plain white
        styled(line.to_string(), Style::default().fg(Color::White))
    }
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

    // Build lines with markdown rendering
    let full_text = pager.lines.join("\n");
    let content_lines: Vec<Line> = markdown_to_lines(&full_text, Style::default().fg(Color::White));

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

        // Sender header
        chat_lines.push(Line::from(Span::styled(
            format!("{}:", msg.sender),
            prefix_style,
        )));

        if msg.text.is_empty() {
            // nothing
        } else if msg.sender == "Claude" {
            // Render Claude's messages as markdown
            let md_lines = markdown_to_lines(&msg.text, text_style);
            for line in md_lines {
                // Indent all content lines
                let mut indented = vec![Span::raw("  ")];
                indented.extend(line.spans.into_iter());
                chat_lines.push(Line::from(indented));
            }
        } else {
            // User messages: plain text, indented
            for text_line in msg.text.lines() {
                chat_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(text_line.to_string(), text_style),
                ]));
            }
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
    let inner_width = chat_area.width.saturating_sub(2).max(1) as usize; // borders
                                                                         // Account for line wrapping: each Line may occupy multiple visual rows
    let total_lines: u16 = chat_lines
        .iter()
        .map(|line| {
            let w = line.width();
            (((w + inner_width - 1) / inner_width) as u16).max(1)
        })
        .sum();
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

/// Render the pipeline list view.
fn render_pipeline_list(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Pipeline Runs ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    if app.pipeline_list.runs.is_empty() {
        let text = Paragraph::new("No pipeline runs. Press 'n' to create one.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        frame.render_widget(text, area);
        return;
    }

    let header = Row::new(vec!["Name", "Status", "Stage", "Iteration", "Created"]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = app
        .pipeline_list
        .runs
        .iter()
        .enumerate()
        .map(|(i, run)| {
            let (icon, _color) = pipeline_status_icon(&run.status);
            let stage = run.current_stage.as_deref().unwrap_or("-");
            let iter_str = format!("{}/{}", run.iteration, run.max_iterations);
            let question_count = app.pending_question_count_for_run(&run.id);
            let status_str = if question_count > 0 {
                format!("{} {} [{} ?]", icon, run.status, question_count)
            } else {
                format!("{} {}", icon, run.status)
            };
            let style = if i == app.pipeline_list.selected {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };
            Row::new(vec![
                run.name.clone(),
                status_str,
                stage.to_string(),
                iter_str,
                run.created_at.clone(),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Percentage(25),
        Constraint::Percentage(20),
        Constraint::Percentage(15),
        Constraint::Percentage(15),
        Constraint::Percentage(25),
    ];

    let table = Table::new(rows, widths).header(header).block(block);

    frame.render_widget(table, area);
}

/// Get an icon and color for a pipeline status.
fn pipeline_status_icon(status: &str) -> (&str, Color) {
    match status {
        "pending" => ("\u{25CB}", Color::DarkGray),
        "running" => ("\u{25B6}", Color::Cyan),
        "completed" => ("\u{2713}", Color::Green),
        "failed" => ("\u{2717}", Color::Red),
        "cancelled" => ("\u{2298}", Color::DarkGray),
        _ => (" ", Color::DarkGray),
    }
}

/// Get a stage status indicator: spinner for running, checkmark for completed, X for failed.
fn pipeline_stage_icon(status: &str) -> String {
    match status {
        "running" => format!("{}", spinner_char()),
        "completed" => "\u{2713}".to_string(),
        "failed" => "\u{2717}".to_string(),
        "pending" => "\u{25CB}".to_string(),
        "cancelled" => "\u{2298}".to_string(),
        _ => " ".to_string(),
    }
}

/// Render the pipeline detail view (stages + output).
fn render_pipeline_detail(app: &App, frame: &mut Frame, area: Rect) {
    let detail = match &app.pipeline_detail {
        Some(d) => d,
        None => return,
    };

    // Horizontal split: left = stages + live output, right = stage output
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    // Left pane: vertical split — stages on top, live output on bottom
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[0]);

    // Left top: stages
    let stages_title = if detail.is_streaming {
        format!(" {} \u{2014} Stages [STREAMING...] ", detail.run.name)
    } else {
        format!(" {} \u{2014} Stages ", detail.run.name)
    };
    let stages_border_color = if detail.is_streaming {
        Color::Yellow
    } else {
        Color::Blue
    };
    let stages_block = Block::default()
        .title(stages_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(stages_border_color));

    let stage_rows: Vec<Row> = detail
        .stages
        .iter()
        .enumerate()
        .map(|(i, stage)| {
            let icon = pipeline_stage_icon(&stage.status);
            let label = format!("{} {} (iter {})", icon, stage.stage_type, stage.iteration);
            let (_, status_color) = pipeline_status_icon(&stage.status);
            let style = if i == detail.selected_stage {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default().fg(status_color)
            };
            Row::new(vec![label]).style(style)
        })
        .collect();

    let stages_table = Table::new(stage_rows, [Constraint::Percentage(100)]).block(stages_block);

    frame.render_widget(stages_table, left_chunks[0]);

    // Left bottom: live output buffer (ring buffer)
    let live_title = if app.pipeline_auto_scroll {
        " Live Output [AUTO] "
    } else {
        " Live Output "
    };
    let live_border_color = if detail.is_streaming {
        Color::Yellow
    } else {
        Color::Blue
    };
    let live_block = Block::default()
        .title(live_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(live_border_color));

    let buffer_lines: Vec<Line> = app
        .pipeline_output_buffer
        .iter()
        .map(|l| Line::from(l.as_str().to_owned()))
        .collect();

    let total_lines = buffer_lines.len();
    let inner_height = live_block.inner(left_chunks[1]).height as usize;

    // Auto-scroll: clamp scroll to show the bottom
    let live_scroll = if app.pipeline_auto_scroll {
        total_lines.saturating_sub(inner_height) as u16
    } else {
        app.pipeline_output_scroll as u16
    };

    let live_output = Paragraph::new(buffer_lines)
        .block(live_block)
        .wrap(Wrap { trim: false })
        .scroll((live_scroll, 0));

    frame.render_widget(live_output, left_chunks[1]);

    // Right pane: per-stage output
    let output_title = if detail.is_streaming {
        " Output [STREAMING...] "
    } else {
        " Output "
    };
    let output_border_color = if detail.is_streaming {
        Color::Yellow
    } else {
        Color::Blue
    };
    let output_block = Block::default()
        .title(output_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(output_border_color));

    // Use stage-specific output instead of all output
    let output_lines = detail.current_stage_output();
    let output_text: Vec<Line> = output_lines
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();

    let scroll = detail.scroll_offset;

    let output = Paragraph::new(output_text)
        .block(output_block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(output, chunks[1]);
}

/// Render the new pipeline dialog popup.
fn render_pipeline_new(app: &App, frame: &mut Frame, area: Rect) {
    let new_state = match &app.pipeline_new {
        Some(s) => s,
        None => return,
    };

    // Center popup
    let popup_width = 60u16.min(area.width.saturating_sub(4));
    let popup_height = 10u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" New Pipeline ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Name label
            Constraint::Length(1), // Name input
            Constraint::Length(1), // Spacer
            Constraint::Length(1), // Goal label
            Constraint::Length(1), // Goal input
            Constraint::Min(0),    // Padding
            Constraint::Length(1), // Help
        ])
        .split(inner);

    let name_label_style = if new_state.focused_field == 0 {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let goal_label_style = if new_state.focused_field == 1 {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    frame.render_widget(Paragraph::new("Name:").style(name_label_style), chunks[0]);

    let name_display = if new_state.focused_field == 0 {
        format!("{}\u{258C}", &new_state.name_input)
    } else {
        new_state.name_input.clone()
    };
    frame.render_widget(
        Paragraph::new(name_display).style(Style::default().fg(Color::White)),
        chunks[1],
    );

    frame.render_widget(Paragraph::new("Goal:").style(goal_label_style), chunks[3]);

    let goal_display = if new_state.focused_field == 1 {
        format!("{}\u{258C}", &new_state.goal_input)
    } else {
        new_state.goal_input.clone()
    };
    frame.render_widget(
        Paragraph::new(goal_display).style(Style::default().fg(Color::White)),
        chunks[4],
    );

    frame.render_widget(
        Paragraph::new("Tab: switch field | Enter: start | Esc: cancel")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[6],
    );
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

    match &app.decomposition_phase {
        DecompositionPhase::Starting => {
            spans.push(Span::raw(" | "));
            let s = spinner_char();
            spans.push(Span::styled(
                format!("{} Starting...", s),
                Style::default().fg(Color::Yellow),
            ));
        }
        DecompositionPhase::Analyzing => {
            spans.push(Span::raw(" | "));
            let s = spinner_char();
            spans.push(Span::styled(
                format!("{} Analyzing...", s),
                Style::default().fg(Color::Cyan),
            ));
        }
        DecompositionPhase::BuildingItems => {
            spans.push(Span::raw(" | "));
            let s = spinner_char();
            spans.push(Span::styled(
                format!("{} Building items...", s),
                Style::default().fg(Color::Cyan),
            ));
        }
        DecompositionPhase::Completed(_) => {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "\u{2713} Complete",
                Style::default().fg(Color::Green),
            ));
        }
        DecompositionPhase::Failed(_) => {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "\u{2717} Failed",
                Style::default().fg(Color::Red),
            ));
        }
        DecompositionPhase::Idle => {}
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

    // View-specific keyboard hints
    spans.push(Span::raw(" | "));
    let view_hints = match app.current_view {
        View::Specs => "Enter:open n:new a:approve d:delete v:view r:resume",
        View::Plan => "Enter:detail Space:collapse g:gen a:approve",
        View::Execute => "Enter:log r:run s:stop e:escalate",
        View::Logs => "Enter:view Esc:back",
        View::Pipeline => "n:new Enter:detail c:cancel",
    };
    spans.push(Span::styled(
        view_hints,
        Style::default().fg(Color::DarkGray),
    ));

    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        "?:help q:quit",
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
        View::Pipeline => {
            let total = app.pipeline_list.runs.len();
            (total, total)
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
        Overlay::PipelineQuestion => render_pipeline_question_overlay(app, frame, area),
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
            help_key("Enter", "Open spec (resume draft / view approved)"),
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
        View::Pipeline => vec![
            help_key("j/k", "Navigate pipeline list / stages"),
            help_key("J/K", "Scroll output (detail view)"),
            help_key("n", "New pipeline"),
            help_key("Enter", "View pipeline detail"),
            help_key("c", "Cancel running pipeline"),
            help_key("Esc", "Exit detail view"),
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

/// Render the pipeline question overlay dialog.
fn render_pipeline_question_overlay(app: &App, frame: &mut Frame, area: Rect) {
    let overlay = match &app.pipeline_question_overlay {
        Some(o) => o,
        None => return,
    };

    let current = match overlay.current_question() {
        Some(q) => q,
        None => return,
    };

    // Dim background
    let dim_style = Style::default().fg(Color::DarkGray);
    let buf = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(dim_style);
            }
        }
    }

    let popup_area = centered_rect(60, 50, area);
    frame.render_widget(Clear, popup_area);

    let remaining = overlay.remaining();
    let title = if remaining > 1 {
        format!(" Pipeline Question ({} remaining) \u{2014} Enter:submit Esc:dismiss ", remaining)
    } else {
        " Pipeline Question \u{2014} Enter:submit Esc:dismiss ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Yellow));

    // Build content
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));

    // Question text
    lines.push(Line::from(Span::styled(
        "  Question:".to_string(),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    // Wrap question text across lines
    for line in current.question.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {}", line),
            Style::default().fg(Color::White),
        )));
    }

    // Context (if any)
    if let Some(ctx) = &current.context {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Context:".to_string(),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        )));
        for line in ctx.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Your answer:".to_string(),
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    )));

    // Input field with cursor
    let input_display = if overlay.answer_input.is_empty() {
        "  \u{2588}".to_string()
    } else {
        format!("  {}\u{2588}", overlay.answer_input)
    };
    lines.push(Line::from(Span::styled(
        input_display,
        Style::default().fg(Color::White),
    )));

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
fn render_spec_name_input(app: &App, frame: &mut Frame, area: Rect) {
    let input = match &app.spec_name_input {
        Some(s) => s,
        None => return,
    };

    let popup_area = centered_rect(50, 20, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("New Spec — Enter:create Esc:cancel")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));

    let lines = vec![
        Line::from(Span::styled(
            " Enter spec name:",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" > {}_", input.input),
            Style::default().fg(Color::Green),
        )),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup_area);
}

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

    if let Some(ref error_msg) = detail.error_message {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  Error: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(error_msg.as_str(), Style::default().fg(Color::Red)),
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
