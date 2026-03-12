use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::app::{App, Mode, Panel};

const BRAND: Color = Color::Rgb(212, 146, 125); // terracotta
const DIM: Color = Color::DarkGray;
const HIGHLIGHT: Color = Color::Rgb(229, 196, 145); // gold

/// Parse a string containing ANSI SGR escape codes into ratatui Spans.
///
/// Non-SGR CSI sequences (cursor movement, screen clear, etc.) are silently
/// dropped so they do not appear as raw `\x1b` garbage in the output pane.
/// Carriage returns (\r) are handled by keeping only content after the last
/// one on a line, which matches the in-place-update pattern used by tools
/// like `dx serve`.
fn ansi_to_spans(s: &str) -> Vec<Span<'_>> {
    // In-place update lines use \r to overwrite content. Keep only what
    // comes after the last \r — that is the "final" content of the line.
    // The PTY reader already applies this for interactive tasks, but piped
    // output may still contain \r.
    let s = s.rfind('\r').map_or(s, |pos| &s[pos + 1..]);

    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut rest = s;

    while !rest.is_empty() {
        if let Some(esc_pos) = rest.find('\x1b') {
            // Text before the escape.
            if esc_pos > 0 {
                spans.push(Span::styled(&rest[..esc_pos], style));
            }
            rest = &rest[esc_pos..];

            if rest.starts_with("\x1b[") {
                // CSI sequence: \x1b[ <params> <terminator>
                // The terminator is the first byte in 0x40–0x7E (e.g. 'm', 'J', 'H', 'h').
                // We must NOT use find('m') on the whole string — that would misparse
                // sequences like \x1b[2J by matching an 'm' elsewhere in the text.
                let term = rest[2..].find(|c: char| c.is_ascii() && matches!(c as u8, 0x40..=0x7E));
                if let Some(rel_pos) = term {
                    let term_pos = 2 + rel_pos;
                    if rest.as_bytes().get(term_pos) == Some(&b'm') {
                        // SGR — apply color/style.
                        let params = &rest[2..term_pos];
                        style = apply_sgr(style, params);
                    }
                    // Skip the entire CSI sequence regardless of terminator.
                    rest = &rest[term_pos + 1..];
                } else {
                    // Unterminated CSI — skip just the \x1b and continue.
                    rest = &rest[1..];
                }
            } else if rest.len() >= 2 {
                // Other escape (SS3, keypad, etc.) — skip \x1b + the next byte.
                rest = &rest[2..];
            } else {
                break; // \x1b at end of string
            }
        } else {
            // No more escapes — remainder is plain text.
            spans.push(Span::styled(rest, style));
            break;
        }
    }

    spans
}

fn apply_sgr(base: Style, params: &str) -> Style {
    let mut style = base;
    let mut codes = params.split(';').peekable();

    while let Some(code) = codes.next() {
        match code {
            "0" | "" => style = Style::default(),
            "1" => style = style.add_modifier(Modifier::BOLD),
            "2" => style = style.add_modifier(Modifier::DIM),
            "3" => style = style.add_modifier(Modifier::ITALIC),
            "4" => style = style.add_modifier(Modifier::UNDERLINED),
            "7" => style = style.add_modifier(Modifier::REVERSED),
            "22" => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            "23" => style = style.remove_modifier(Modifier::ITALIC),
            "24" => style = style.remove_modifier(Modifier::UNDERLINED),
            "27" => style = style.remove_modifier(Modifier::REVERSED),
            // Standard foreground colors.
            "30" => style = style.fg(Color::Black),
            "31" => style = style.fg(Color::Red),
            "32" => style = style.fg(Color::Green),
            "33" => style = style.fg(Color::Yellow),
            "34" => style = style.fg(Color::Blue),
            "35" => style = style.fg(Color::Magenta),
            "36" => style = style.fg(Color::Cyan),
            "37" => style = style.fg(Color::White),
            "39" => style = style.fg(Color::Reset),
            // Bright foreground colors.
            "90" => style = style.fg(Color::DarkGray),
            "91" => style = style.fg(Color::LightRed),
            "92" => style = style.fg(Color::LightGreen),
            "93" => style = style.fg(Color::LightYellow),
            "94" => style = style.fg(Color::LightBlue),
            "95" => style = style.fg(Color::LightMagenta),
            "96" => style = style.fg(Color::LightCyan),
            "97" => style = style.fg(Color::White),
            // Extended foreground: 38;5;N or 38;2;R;G;B
            "38" => {
                if let Some(mode) = codes.next() {
                    if mode == "5" {
                        if let Some(n) = codes.next().and_then(|v| v.parse::<u8>().ok()) {
                            style = style.fg(Color::Indexed(n));
                        }
                    } else if mode == "2" {
                        let r = codes.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
                        let g = codes.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
                        let b = codes.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
                        style = style.fg(Color::Rgb(r, g, b));
                    }
                }
            }
            // Standard background colors.
            "40" => style = style.bg(Color::Black),
            "41" => style = style.bg(Color::Red),
            "42" => style = style.bg(Color::Green),
            "43" => style = style.bg(Color::Yellow),
            "44" => style = style.bg(Color::Blue),
            "45" => style = style.bg(Color::Magenta),
            "46" => style = style.bg(Color::Cyan),
            "47" => style = style.bg(Color::White),
            "49" => style = style.bg(Color::Reset),
            // Extended background: 48;5;N or 48;2;R;G;B
            "48" => {
                if let Some(mode) = codes.next() {
                    if mode == "5" {
                        if let Some(n) = codes.next().and_then(|v| v.parse::<u8>().ok()) {
                            style = style.bg(Color::Indexed(n));
                        }
                    } else if mode == "2" {
                        let r = codes.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
                        let g = codes.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
                        let b = codes.next().and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
                        style = style.bg(Color::Rgb(r, g, b));
                    }
                }
            }
            _ => {} // Ignore unknown codes.
        }
    }

    style
}

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // main content
            Constraint::Length(1), // status bar
        ])
        .split(f.area());

    render_title_bar(f, chunks[0]);

    match app.mode {
        Mode::Output => render_output(f, app, chunks[1]),
        _ => render_panels(f, app, chunks[1]),
    }

    render_status_bar(f, app, chunks[2]);

    if app.show_help {
        render_help(f, f.area());
    }

    if app.show_task_info {
        render_task_info(f, app, f.area());
    }
}

fn render_title_bar(f: &mut Frame, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            " moon-tui ",
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
        ),
        Span::styled("— workspace task runner", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title), area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(text) = app.status_text() {
        text.to_string()
    } else {
        let panel = match app.panel {
            Panel::Projects => "Projects",
            Panel::Tasks => "Tasks",
        };
        let bg = if app.is_running_task() {
            if app.stdin_tx.is_some() {
                " | ● interactive (o: resume)"
            } else {
                " | ● running (o: view)"
            }
        } else {
            ""
        };
        format!(" {panel}{bg} | ? help | q quit")
    };
    let bar = Paragraph::new(Line::from(vec![Span::styled(
        status,
        Style::default().fg(DIM),
    )]));
    f.render_widget(bar, area);
}

fn render_panels(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    render_project_list(f, app, cols[0]);
    render_task_list(f, app, cols[1]);
}

fn render_project_list(f: &mut Frame, app: &App, area: Rect) {
    let is_active = app.panel == Panel::Projects;
    let border_style = if is_active {
        Style::default().fg(BRAND)
    } else {
        Style::default().fg(DIM)
    };

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let style = if i == app.project_index {
                if is_active {
                    Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                }
            } else {
                Style::default().fg(Color::Gray)
            };

            let marker = if i == app.project_index { "▸ " } else { "  " };
            let tag_str = if p.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", p.tags.join(", "))
            };

            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(&p.id, style),
                Span::styled(tag_str, Style::default().fg(DIM)),
            ]))
        })
        .collect();

    let block = Block::default()
        .title(format!(" Projects ({}) ", app.projects.len()))
        .borders(Borders::ALL)
        .border_style(border_style);

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn render_task_list(f: &mut Frame, app: &App, area: Rect) {
    let is_active = app.panel == Panel::Tasks;
    let border_style = if is_active {
        Style::default().fg(BRAND)
    } else {
        Style::default().fg(DIM)
    };

    let project_name = app
        .selected_project()
        .map(|p| p.id.as_str())
        .unwrap_or("none");

    let items: Vec<ListItem> = app
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let style = if i == app.task_index && is_active {
                Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };

            let marker = if i == app.task_index && is_active {
                "▸ "
            } else {
                "  "
            };

            let full_cmd = t.full_command();
            let cmd_str = if full_cmd.is_empty() {
                String::new()
            } else {
                format!("  ({full_cmd})")
            };

            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(&t.id, style),
                Span::styled(cmd_str, Style::default().fg(DIM)),
            ]))
        })
        .collect();

    let block = Block::default()
        .title(format!(" Tasks — {project_name} ({}) ", app.tasks.len()))
        .borders(Borders::ALL)
        .border_style(border_style);

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn render_output(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.output_finished {
        let code = app.output_exit_code.unwrap_or(-1);
        let status = if code == 0 { "done" } else { "FAILED" };
        format!(" Output — {} ({status}, exit {code}) ", app.output_target)
    } else if app.stdin_tx.is_some() {
        format!(" Interactive — {} (Esc: detach) ", app.output_target)
    } else {
        format!(" Output — {} (running...) ", app.output_target)
    };

    let border_color = if app.output_finished {
        if app.output_exit_code == Some(0) {
            Color::Green
        } else {
            Color::Red
        }
    } else {
        BRAND
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Record how many rows are visible so key handlers can clamp scroll correctly.
    app.output_visible_height = inner.height as usize;

    if app.output_lines.is_empty() {
        let msg = if app.output_finished {
            "No output."
        } else {
            "Waiting for output..."
        };
        f.render_widget(Paragraph::new(msg).style(Style::default().fg(DIM)), inner);
        return;
    }

    let visible_height = inner.height as usize;
    let total = app.output_lines.len();
    let start = if app.output_follow {
        total.saturating_sub(visible_height)
    } else {
        app.output_scroll.min(total.saturating_sub(visible_height))
    };
    let end = (start + visible_height).min(total);

    let lines: Vec<Line> = app.output_lines[start..end]
        .iter()
        .map(|l| Line::from(ansi_to_spans(l)))
        .collect();

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);
}

fn render_help(f: &mut Frame, area: Rect) {
    let help_width = 52u16;
    let help_height = 27u16;
    let x = area.width.saturating_sub(help_width) / 2;
    let y = area.height.saturating_sub(help_height) / 2;
    let popup = Rect::new(
        x,
        y,
        help_width.min(area.width),
        help_height.min(area.height),
    );

    f.render_widget(Clear, popup);

    let lines = vec![
        Line::from(Span::styled(
            " Keybindings ",
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(" Navigation"),
        Line::from(vec![
            Span::styled("  h/l  ", Style::default().fg(HIGHLIGHT)),
            Span::raw("Switch panel (projects/tasks)"),
        ]),
        Line::from(vec![
            Span::styled("  j/k  ", Style::default().fg(HIGHLIGHT)),
            Span::raw("Move down/up"),
        ]),
        Line::from(vec![
            Span::styled("  gg/G ", Style::default().fg(HIGHLIGHT)),
            Span::raw("Jump to top/bottom"),
        ]),
        Line::from(vec![
            Span::styled("  C-d/u", Style::default().fg(HIGHLIGHT)),
            Span::raw("Half page down/up"),
        ]),
        Line::from(""),
        Line::from(" Actions"),
        Line::from(vec![
            Span::styled("  Enter", Style::default().fg(HIGHLIGHT)),
            Span::raw("  Projects: show tasks / Tasks: run task"),
        ]),
        Line::from(vec![
            Span::styled("  C     ", Style::default().fg(HIGHLIGHT)),
            Span::raw(" Run CI for selected project"),
        ]),
        Line::from(vec![
            Span::styled("  i     ", Style::default().fg(HIGHLIGHT)),
            Span::raw(" Show task info"),
        ]),
        Line::from(vec![
            Span::styled("  I     ", Style::default().fg(HIGHLIGHT)),
            Span::raw(" Run interactive (Esc detach)"),
        ]),
        Line::from(vec![
            Span::styled("  o     ", Style::default().fg(HIGHLIGHT)),
            Span::raw(" Reattach to running task"),
        ]),
        Line::from(vec![
            Span::styled("  R     ", Style::default().fg(HIGHLIGHT)),
            Span::raw(" Force run (skip cache)"),
        ]),
        Line::from(vec![
            Span::styled("  r     ", Style::default().fg(HIGHLIGHT)),
            Span::raw(" Refresh project/task list"),
        ]),
        Line::from(""),
        Line::from(" Output view"),
        Line::from(vec![
            Span::styled("  j/k  ", Style::default().fg(HIGHLIGHT)),
            Span::raw("Scroll output"),
        ]),
        Line::from(vec![
            Span::styled("  f    ", Style::default().fg(HIGHLIGHT)),
            Span::raw("Toggle follow mode"),
        ]),
        Line::from(vec![
            Span::styled("  Esc/q", Style::default().fg(HIGHLIGHT)),
            Span::raw("Back to panels"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ?    ", Style::default().fg(HIGHLIGHT)),
            Span::raw("Toggle this help"),
        ]),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BRAND));

    let help = Paragraph::new(lines).block(block);
    f.render_widget(help, popup);
}

fn render_task_info(f: &mut Frame, app: &App, area: Rect) {
    let task = match app.selected_task() {
        Some(t) => t,
        None => return,
    };

    let project_name = app.selected_project().map(|p| p.id.as_str()).unwrap_or("?");

    let toolchains = task.toolchains.join(", ");
    let full_cmd = task.full_command();
    let mut lines = vec![
        Line::from(Span::styled(
            format!(" {} ", task.target),
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        info_line("Task", &task.id),
        info_line("Project", project_name),
        info_line("Command", &full_cmd),
        info_line("Type", &task.task_type),
        info_line("Toolchains", &toolchains),
        info_line("Interactive", if task.interactive { "yes" } else { "no" }),
    ];

    if !task.deps.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Dependencies",
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
        )));
        for dep in &task.deps {
            lines.push(Line::from(vec![
                Span::styled("    → ", Style::default().fg(DIM)),
                Span::styled(dep.as_str(), Style::default().fg(HIGHLIGHT)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  i/Esc ", Style::default().fg(HIGHLIGHT)),
        Span::raw("Close"),
    ]));

    let info_width = 70u16;
    let info_height = (lines.len() as u16 + 2).min(area.height);
    let x = area.width.saturating_sub(info_width) / 2;
    let y = area.height.saturating_sub(info_height) / 2;
    let popup = Rect::new(x, y, info_width.min(area.width), info_height);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Task Info ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BRAND));

    let info = Paragraph::new(lines).block(block);
    f.render_widget(info, popup);
}

fn info_line<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<14}"), Style::default().fg(HIGHLIGHT)),
        Span::raw(if value.is_empty() { "—" } else { value }),
    ])
}
