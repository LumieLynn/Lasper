use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::App;
use crate::ui::StatusLevel;
use crate::ui::{centered_rect, core::Component};

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // content
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_title(f, app, rows[0]);
    render_content(f, app, rows[1]);
    render_status(f, app, rows[2]);

    // Overlays (highest priority last so they render on top)
    if let Some(pm) = &mut app.ui.power_menu {
        pm.render(f, area);
    }
    if app.ui.show_wizard {
        if let Some(w) = &mut app.ui.wizard {
            w.render(f, area);
        }
    }
    if app.ui.show_help {
        render_help(f);
    }
    if let Some(dialog) = &mut app.ui.quit_dialog {
        dialog.render(f, area);
    }
}

// ── Title ─────────────────────────────────────────────────────────────────────

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let badge = if app.is_root {
        Span::styled(
            " [ ⚡ ROOT ] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            " [ ⚠  READ-ONLY — run with sudo for full control ] ",
            Style::default().fg(Color::Yellow),
        )
    };

    let mut spans = vec![
        Span::styled(
            " Lasper ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        badge,
    ];

    if !app.data.dbus_active {
        spans.push(Span::styled(
            " [ ⚡ CMD-MODE ] ",
            Style::default()
                .fg(Color::Rgb(255, 140, 0))
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        format!("  {} container(s)", app.data.entries.len()),
        Style::default().fg(Color::DarkGray),
    ));

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line).style(Style::default()), area);
}

// ── Content ───────────────────────────────────────────────────────────────────

fn render_content(f: &mut Frame, app: &mut App, area: Rect) {
    let list_focused = app.ui.active_panel == crate::app::ActivePanel::ContainerList;
    let detail_focused = app.ui.active_panel == crate::app::ActivePanel::DetailPanel;
    let terminal_focused = app.ui.active_panel == crate::app::ActivePanel::TerminalPanel;

    app.ui.detail_panel.set_focus(detail_focused);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    let list_area = cols[0];
    let right_area = cols[1];

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if app.ui.show_terminal {
            vec![Constraint::Percentage(60), Constraint::Percentage(40)]
        } else {
            vec![Constraint::Percentage(100)]
        })
        .split(right_area);

    let detail_area = right_chunks[0];

    app.ui.pane_height = list_area.height.saturating_sub(2);
    app.ui.detail_panel.pane_height = detail_area.height.saturating_sub(2);

    if app.ui.show_terminal {
        let terminal_area = right_chunks[1];
        let terminal_panel = crate::ui::views::terminal_panel::TerminalPanel;
        terminal_panel.render(
            f,
            terminal_area,
            &app.data.terminal_sessions,
            app.data.active_terminal_idx,
            terminal_focused,
        );
    }

    app.ui.container_list.render_with_data(
        f,
        list_area,
        &app.data.entries,
        app.data.selected,
        app.is_root,
        list_focused,
    );
    app.ui
        .detail_panel
        .render_with_data(f, detail_area, &mut app.data);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some((msg, level)) = &app.ui.status_message {
        let color = match level {
            StatusLevel::Info => Color::White,
            StatusLevel::Success => Color::Green,
            StatusLevel::Warn => Color::Rgb(255, 140, 0),
            StatusLevel::Error => Color::Red,
        };
        Line::from(vec![
            Span::raw("  "),
            Span::styled(msg.as_str(), Style::default().fg(color)),
        ])
    } else {
        match app.ui.active_panel {
            crate::app::ActivePanel::ContainerList => Line::from(vec![
                kspan("[j/k]"),
                hspan(" nav "),
                kspan("[Tab]"),
                hspan(" → detail "),
                kspan("[s]"),
                hspan(" start "),
                kspan("[S]"),
                hspan(" poweroff "),
                kspan("[x/⏎]"),
                hspan(" actions "),
                kspan("[n/a]"),
                hspan(" new "),
                kspan("[r]"),
                hspan(" refresh "),
                kspan("[t]"),
                hspan(" terminal "),
                kspan("[?]"),
                hspan(" help "),
                kspan("[q]"),
                hspan(" quit"),
            ]),
            crate::app::ActivePanel::DetailPanel => Line::from(vec![
                kspan("[Alt+1..5]"),
                hspan(" panes "),
                kspan("[[/]]"),
                hspan(" cycle "),
                kspan("[↑/↓ | j/k]"),
                hspan(" scroll "),
                kspan("[PgUp/Dn]"),
                hspan(" page "),
                kspan("[Tab]"),
                hspan(" → list "),
                kspan("[t]"),
                hspan(" terminal "),
                kspan("[?]"),
                hspan(" help "),
                kspan("[q]"),
                hspan(" quit"),
            ]),
            crate::app::ActivePanel::TerminalPanel => {
                let insert_mode = if let Some(session) =
                    app.data.terminal_sessions.get(app.data.active_terminal_idx)
                {
                    session.insert_mode
                } else {
                    false
                };
                if insert_mode {
                    Line::from(vec![
                        kspan("[Alt+x]"),
                        hspan(" exit insert mode "),
                        kspan("[Alt+1..9 / [/]]"),
                        hspan(" switch tabs"),
                    ])
                } else {
                    Line::from(vec![
                        kspan("[i/⏎/Alt+x]"),
                        hspan(" insert mode "),
                        kspan("[Alt+1..9 / [/]]"),
                        hspan(" switch tabs "),
                        kspan("[t]"),
                        hspan(" hide "),
                        kspan("[x]"),
                        hspan(" close tab "),
                        kspan("[q]"),
                        hspan(" quit"),
                    ])
                }
            }
        }
    };

    f.render_widget(Paragraph::new(line).style(Style::default()), area);
}

fn kspan(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(Color::Cyan))
}
fn hspan(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(Color::DarkGray))
}

// ── Help overlay ──────────────────────────────────────────────────────────────

fn render_help(f: &mut Frame) {
    let area = centered_rect(50, 85, f.area());
    f.render_widget(Clear, area);
    let rows: Vec<Line> = vec![
        Line::from(Span::styled(
            "  Keybindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        hrow("j / ↓", "Select next container"),
        hrow("k / ↑", "Select previous container"),
        Line::from(""),
        hrow("Alt+1..5", "Switch detail pane"),
        hrow("[ / ]  ", "Cycle detail panes"),
        hrow("↑/↓ | j/k", "Scroll / navigate in detail pane"),
        Line::from(""),
        hrow("Alt+1..9", "Switch terminal tab"),
        hrow("[ / ]  ", "Cycle terminal tabs"),
        Line::from(""),
        hrow("s    ", "Start container  [root]"),
        hrow("S    ", "Poweroff container [root]"),
        hrow("x / ⏎", "Actions / Power menu  [root]"),
        Line::from(""),
        hrow("n    ", "New container / Import wizard  [root]"),
        Line::from(""),
        hrow("Tab  ", "Toggle focus: list ↔ detail panel"),
        hrow("r    ", "Refresh list"),
        hrow("?    ", "Toggle help"),
        hrow("q    ", "Quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}

fn hrow(k: &'static str, d: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(k, Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::raw(d),
    ])
}
