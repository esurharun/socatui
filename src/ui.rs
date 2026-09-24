//! Rendering.

use crate::app::{App, ExportDialog, Form, Mode, FORM_FIELDS};
use crate::tunnel::{Status, Tunnel};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use std::time::Duration;

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn human_rate(bps: f64) -> String {
    if bps < 1.0 {
        return "0 B/s".into();
    }
    format!("{}/s", human_bytes(bps as u64))
}

pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn status_style(s: &Status) -> Style {
    match s {
        Status::Running => Style::default().fg(Color::Green).bold(),
        Status::Stopping => Style::default().fg(Color::Yellow),
        Status::Stopped => Style::default().fg(Color::DarkGray),
        Status::Exited(Some(0)) => Style::default().fg(Color::DarkGray),
        Status::Exited(_) => Style::default().fg(Color::Red),
        Status::Failed(_) => Style::default().fg(Color::Red).bold(),
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let log_height = if app.show_log { 9 } else { 0 };
    let [header, table_area, log_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(log_height),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, app, header);
    draw_table(f, app, table_area);
    if app.show_log {
        draw_log(f, app, log_area);
    }
    draw_footer(f, app, footer);

    match &app.mode {
        Mode::Normal => {}
        Mode::Form(form) => draw_form(f, form, area),
        Mode::ConfirmDelete => draw_confirm(f, app, area),
        Mode::Export(dialog) => draw_export(f, app, dialog, area),
        Mode::Help => draw_help(f, area),
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (ltr, rtl) = app.total_rates();
    let line = Line::from(vec![
        Span::styled(
            " socatui ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(format!(
            "  {} tunnels, {} running   ",
            app.tunnels.len(),
            app.running_count()
        )),
        Span::styled("→ ", Style::default().fg(Color::Cyan)),
        Span::raw(human_rate(ltr)),
        Span::raw("   "),
        Span::styled("← ", Style::default().fg(Color::Magenta)),
        Span::raw(human_rate(rtl)),
        Span::raw("   "),
        Span::styled(
            format!("config: {}", app.config_path.display()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let header = Row::new(
        [
            "#",
            "NAME",
            "STATUS",
            "PID",
            "SOURCE",
            "DESTINATION",
            "CONN",
            "→ BYTES",
            "← BYTES",
            "→ RATE",
            "← RATE",
            "UPTIME",
        ]
        .into_iter()
        .map(|h| Cell::from(h).style(Style::default().fg(Color::Yellow).bold())),
    )
    .height(1);

    let rows = app.tunnels.iter().enumerate().map(|(i, t)| row_for(i, t));

    let widths = [
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Fill(2),
        Constraint::Fill(2),
        Constraint::Length(4),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(9),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" tunnels ");

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 50, 70))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = TableState::default();
    if !app.tunnels.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(table, area, &mut state);

    if app.tunnels.is_empty() {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: 3.min(area.height.saturating_sub(3)),
        };
        let msg = Paragraph::new(vec![
            Line::from("No tunnels configured yet."),
            Line::from(
                "Press a to add one, e.g. TCP-LISTEN:8080,fork,reuseaddr → TCP:example.com:80",
            ),
        ])
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
        f.render_widget(msg, inner);
    }
}

fn row_for(i: usize, t: &Tunnel) -> Row<'static> {
    let pid = t.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
    let uptime = t.uptime().map(human_duration).unwrap_or_else(|| "-".into());
    let conn = if t.is_running() {
        t.connections.to_string()
    } else {
        "-".into()
    };
    let dim = if t.is_running() {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let rate_style = |r: f64, c: Color| {
        if r >= 1.0 {
            Style::default().fg(c)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };
    Row::new(vec![
        Cell::from(format!("{}", i + 1)).style(Style::default().fg(Color::DarkGray)),
        Cell::from(t.config.name.clone()).style(if t.is_running() {
            Style::default().bold()
        } else {
            dim
        }),
        Cell::from(t.status.label()).style(status_style(&t.status)),
        Cell::from(pid).style(dim),
        Cell::from(t.config.source.clone()).style(dim),
        Cell::from(t.config.destination.clone()).style(dim),
        Cell::from(conn).style(dim),
        Cell::from(human_bytes(t.total.ltr_bytes)),
        Cell::from(human_bytes(t.total.rtl_bytes)),
        Cell::from(human_rate(t.rate_ltr)).style(rate_style(t.rate_ltr, Color::Cyan)),
        Cell::from(human_rate(t.rate_rtl)).style(rate_style(t.rate_rtl, Color::Magenta)),
        Cell::from(uptime).style(dim),
    ])
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let (title, lines): (String, Vec<Line>) = match app.selected_tunnel() {
        Some(t) => {
            let inner_h = area.height.saturating_sub(2) as usize;
            let lines = t
                .log
                .iter()
                .rev()
                .take(inner_h)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|l| Line::from(l.as_str()))
                .collect();
            let detail = match &t.status {
                Status::Failed(e) => format!(" — {e}"),
                _ => String::new(),
            };
            (
                format!(
                    " {} — {} packets → / {} packets ←, {} sessions{} ",
                    t.config.name, t.total.ltr_packets, t.total.rtl_packets, t.sessions, detail
                ),
                lines,
            )
        }
        None => (" log ".into(), vec![]),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = match &app.mode {
        Mode::Form(_) => Line::from(vec![
            key("Tab/↑↓", "field"),
            key("Space", "toggle"),
            key("Enter", "save"),
            key("Esc", "cancel"),
        ]),
        Mode::ConfirmDelete => Line::from(vec![key("y", "confirm delete"), key("any", "cancel")]),
        Mode::Export(_) => Line::from(vec![
            key("Tab", "selected/all"),
            key("Ctrl-U", "clear path"),
            key("Enter", "write"),
            key("Esc", "cancel"),
        ]),
        Mode::Help => Line::from(vec![key("any key", "close")]),
        Mode::Normal => {
            if let Some(msg) = app.status_text() {
                Line::from(Span::styled(
                    format!(" {msg}"),
                    Style::default().fg(Color::Yellow),
                ))
            } else {
                Line::from(vec![
                    key("a", "add"),
                    key("e", "edit"),
                    key("d", "del"),
                    key("s", "start/stop"),
                    key("r", "restart"),
                    key("c", "clear stats"),
                    key("l", "log"),
                    key("w/W", "export log"),
                    key("S/X", "start/stop all"),
                    key("?", "help"),
                    key("q", "quit"),
                ])
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn key(k: &str, desc: &str) -> Span<'static> {
    Span::raw(format!(" [{k}] {desc} "))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_form(f: &mut Frame, form: &Form, area: Rect) {
    let popup = centered(area, 78, 15);
    f.render_widget(Clear, popup);
    let title = if form.editing.is_some() {
        " edit tunnel "
    } else {
        " add tunnel "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let labels = ["Name", "Source", "Destination", "Options", "Autostart"];
    let hints = [
        "a label for the list",
        "left address, e.g. TCP-LISTEN:8080,fork,reuseaddr",
        "right address, e.g. TCP:example.com:80",
        "extra socat flags, e.g. -d -d -T 30",
        "start this tunnel when socatui launches",
    ];
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .split(inner);

    for i in 0..FORM_FIELDS {
        let focused = form.focus == i;
        let label_style = if focused {
            Style::default().fg(Color::Cyan).bold()
        } else {
            Style::default().fg(Color::Gray)
        };
        let [label_area, value_area] =
            Layout::horizontal([Constraint::Length(13), Constraint::Min(1)]).areas(rows[i]);
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                format!("{:>11}: ", labels[i]),
                label_style,
            ))]),
            label_area,
        );
        if i == FORM_FIELDS - 1 {
            let v = if form.autostart { "[x] yes" } else { "[ ] no" };
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(v),
                    Line::from(Span::styled(hints[i], Style::default().fg(Color::DarkGray))),
                ]),
                value_area,
            );
        } else {
            let input = match i {
                0 => &form.name,
                1 => &form.source,
                2 => &form.destination,
                _ => &form.options,
            };
            // horizontal scroll so the cursor is always visible
            let w = value_area.width.max(1) as usize;
            let chars: Vec<char> = input.text.chars().collect();
            let start = input.cursor.saturating_sub(w.saturating_sub(1));
            let visible: String = chars.iter().skip(start).take(w).collect();
            let value_style = if focused {
                Style::default().add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
            };
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(visible, value_style)),
                    Line::from(Span::styled(hints[i], Style::default().fg(Color::DarkGray))),
                ]),
                value_area,
            );
            if focused {
                let cx = value_area.x + (input.cursor - start) as u16;
                f.set_cursor_position((cx.min(value_area.right().saturating_sub(1)), value_area.y));
            }
        }
    }

    let footer = match &form.error {
        Some(e) => Line::from(Span::styled(
            format!("✗ {e}"),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            "Enter saves, Esc cancels. Addresses are passed to socat verbatim (no shell quoting).",
            Style::default().fg(Color::DarkGray),
        )),
    };
    f.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), rows[5]);
}

fn draw_confirm(f: &mut Frame, app: &App, area: Rect) {
    let name = app
        .selected_tunnel()
        .map(|t| t.config.name.as_str())
        .unwrap_or("");
    let running = app
        .selected_tunnel()
        .map(|t| t.is_running())
        .unwrap_or(false);
    let popup = centered(area, 60, 6);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(" delete tunnel ");
    let mut lines = vec![Line::from(format!("Delete \"{name}\"?"))];
    if running {
        lines.push(Line::from(Span::styled(
            "It is running and will be stopped.",
            Style::default().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "y = yes, any other key = no",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        popup,
    );
}

fn draw_export(f: &mut Frame, app: &App, dialog: &ExportDialog, area: Rect) {
    let popup = centered(area, 78, 9);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" export log ");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let scope = if dialog.all {
        format!("all {} tunnels", app.tunnels.len())
    } else {
        app.selected_tunnel()
            .map(|t| format!("\"{}\" only", t.config.name))
            .unwrap_or_default()
    };
    let [scope_area, path_label, path_area, hint_area, msg_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Scope: ", Style::default().fg(Color::Gray)),
            Span::styled(scope, Style::default().bold()),
            Span::styled("   (Tab toggles)", Style::default().fg(Color::DarkGray)),
        ])),
        scope_area,
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "Path:",
            Style::default().fg(Color::Cyan).bold(),
        )),
        path_label,
    );
    let w = path_area.width.max(1) as usize;
    let chars: Vec<char> = dialog.path.text.chars().collect();
    let start = dialog.path.cursor.saturating_sub(w.saturating_sub(1));
    let visible: String = chars.iter().skip(start).take(w).collect();
    f.render_widget(
        Paragraph::new(Span::styled(
            visible,
            Style::default().add_modifier(Modifier::UNDERLINED),
        )),
        path_area,
    );
    let cx = path_area.x + (dialog.path.cursor - start) as u16;
    f.set_cursor_position((cx.min(path_area.right().saturating_sub(1)), path_area.y));
    f.render_widget(
        Paragraph::new(Span::styled(
            "Leave empty for a random file under /tmp. Existing files are overwritten.",
            Style::default().fg(Color::DarkGray),
        )),
        hint_area,
    );
    if let Some(e) = &dialog.error {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("✗ {e}"),
                Style::default().fg(Color::Red),
            ))
            .wrap(Wrap { trim: true }),
            msg_area,
        );
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let popup = centered(area, 72, 23);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" help ");
    let entries = [
        ("↑/k ↓/j", "select tunnel"),
        ("g / G", "first / last"),
        ("a", "add tunnel"),
        ("e / Enter", "edit selected"),
        ("d", "delete selected"),
        (
            "s / Space",
            "start / stop selected (press again while stopping = SIGKILL)",
        ),
        ("r", "restart selected"),
        ("K", "SIGKILL selected"),
        ("S / X", "start all / stop all"),
        ("c / C", "clear stats: selected / all"),
        ("J / U", "move selected down / up"),
        ("l", "toggle log pane"),
        ("w / W", "export log of selected / all tunnels to a file"),
        ("q / Ctrl-C", "quit (stops all tunnels)"),
    ];
    let mut lines: Vec<Line> = entries
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!("  {k:<12}"),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::raw(*d),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Each tunnel runs `socat --statistics [options] SOURCE DESTINATION`.",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "  Counters come from SIGUSR1 stat dumps once per second; with `fork`",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "  each connection is a socat child and CONN shows how many are alive.",
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}
