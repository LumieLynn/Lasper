use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::App;
use crate::ui::core::Component;
use crate::ui::theme;

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
        crate::ui::widgets::help::HelpOverlay::new().render(f, area);
    }
    if let Some(dialog) = &mut app.ui.quit_dialog {
        dialog.render(f, area);
    }
    if let Some(dialog) = &mut app.ui.delete_dialog {
        dialog.render(f, area);
    }
    if let Some(dialog) = &mut app.ui.active_dialog {
        dialog.render(f, area);
    }
}

// Title

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::theme();
    let badge = match app.permissions.level() {
        crate::nspawn::ops::PermissionLevel::Root => Span::styled(
            " [ ROOT ] ",
            Style::default()
                .fg(t.badge_root)
                .add_modifier(Modifier::BOLD),
        ),
        crate::nspawn::ops::PermissionLevel::Elevated => Span::styled(
            " [ SUDO ] ",
            Style::default()
                .fg(t.badge_cli)
                .add_modifier(Modifier::BOLD),
        ),
        crate::nspawn::ops::PermissionLevel::User => {
            Span::styled(" [ USER ] ", Style::default().fg(t.badge_readonly))
        }
    };

    let mut spans = vec![
        Span::styled(
            " Lasper ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        badge,
    ];

    if !app.data.dbus_active {
        spans.push(Span::styled(
            " [ ⚡ CLI ] ",
            Style::default()
                .fg(t.badge_cli)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        format!("  {} container(s)", app.data.entries.len()),
        Style::default().fg(t.text_secondary),
    ));

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line).style(Style::default()), area);
}

// Content

fn render_content(f: &mut Frame, app: &mut App, area: Rect) {
    let list_focused = app.ui.focus.active_idx == 0;
    let detail_focused = app.ui.focus.active_idx == 1;
    let terminal_focused = app.ui.focus.active_idx == 2;
    let resize_mode = app.ui.resize_mode == crate::app::ResizeMode::Active;

    app.ui.detail_panel.set_focus(detail_focused);

    let list_pct = app.ui.container_list_pct;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(list_pct),
            Constraint::Percentage(100u16.saturating_sub(list_pct)),
        ])
        .split(area);

    let list_area = cols[0];
    let right_area = cols[1];

    let maximized = app.data.terminal.is_showing() && app.data.terminal.maximized;
    let detail_pct = app.ui.detail_pct;
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if maximized {
            vec![Constraint::Percentage(100)]
        } else if app.data.terminal.is_showing() {
            vec![
                Constraint::Percentage(detail_pct),
                Constraint::Percentage(100u16.saturating_sub(detail_pct)),
            ]
        } else {
            vec![Constraint::Percentage(100)]
        })
        .split(right_area);

    let detail_area = right_chunks[0];

    // Store panel rects for mouse hit-testing.
    app.ui.panel_layout.list = list_area;
    app.ui.panel_layout.detail = detail_area;

    let terminal_area = if app.data.terminal.is_showing() {
        let ta = if maximized {
            right_chunks[0]
        } else {
            right_chunks[1]
        };
        app.ui.panel_layout.terminal = Some(ta);
        Some(ta)
    } else {
        app.ui.panel_layout.terminal = None;
        None
    };

    app.ui.pane_height = list_area.height.saturating_sub(2);
    app.ui.detail_panel.pane_height = detail_area.height.saturating_sub(2);

    if let Some(terminal_area) = terminal_area {
        let terminal_panel = crate::ui::views::terminal_panel::TerminalPanel;
        terminal_panel.render(
            f,
            terminal_area,
            &mut app.data.terminal,
            terminal_focused,
            resize_mode,
        );
    }

    app.ui.container_list.render_with_data(
        f,
        list_area,
        &app.data.entries,
        app.data.selected,
        list_focused,
        resize_mode,
    );
    if !maximized {
        app.ui
            .detail_panel
            .render_with_data(f, detail_area, &mut app.data, resize_mode);
    }
}

// Status bar

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::theme();
    let line = if let Some((msg, level)) = &app.ui.status_message {
        let color = t.status_color(level);
        Line::from(vec![
            Span::raw("  "),
            Span::styled(msg.as_str(), Style::default().fg(color)),
        ])
    } else if app.ui.resize_mode == crate::app::ResizeMode::Active {
        Line::from(vec![
            kspan("[Esc/R/q]"),
            hspan(" exit resize "),
            kspan("[←/h →/l]"),
            hspan(" list width "),
            kspan("[↓/j ↑/k]"),
            hspan(" detail/terminal height"),
        ])
    } else {
        match app.ui.focus.active_idx {
            0 => Line::from(vec![
                kspan("[j/k]"),
                hspan(" nav "),
                kspan("[Tab/⇧Tab]"),
                hspan(" panels "),
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
                kspan("[D]"),
                hspan(" delete "),
                kspan("[?]"),
                hspan(" help "),
                kspan("[q]"),
                hspan(" quit"),
            ]),
            1 => Line::from(vec![
                kspan("[Alt+1..5]"),
                hspan(" panes "),
                kspan("[[/]]"),
                hspan(" cycle "),
                kspan("[↑/↓ | j/k]"),
                hspan(" scroll "),
                kspan("[PgUp/Dn]"),
                hspan(" page "),
                kspan("[Tab/⇧Tab]"),
                hspan(" panels "),
                kspan("[t]"),
                hspan(" terminal "),
                kspan("[?]"),
                hspan(" help "),
                kspan("[q]"),
                hspan(" quit"),
            ]),
            2 => {
                let insert_mode = app
                    .data
                    .terminal
                    .active_session()
                    .map(|s| s.insert_mode)
                    .unwrap_or(false);
                if insert_mode {
                    Line::from(vec![
                        kspan("[Alt+x]"),
                        hspan(" exit insert mode "),
                        kspan("[Alt+1..9]"),
                        hspan(" switch tabs"),
                    ])
                } else {
                    let t_label = if app.data.terminal.maximized {
                        " restore split "
                    } else {
                        " maximize "
                    };
                    Line::from(vec![
                        kspan("[i/⏎/Alt+x]"),
                        hspan(" insert mode "),
                        kspan("[Alt+1..9 / [/]]"),
                        hspan(" switch tabs "),
                        kspan("[Tab/⇧Tab]"),
                        hspan(" panels "),
                        kspan("[T]"),
                        hspan(t_label),
                        kspan("[t]"),
                        hspan(" hide "),
                        kspan("[x]"),
                        hspan(" close tab "),
                        kspan("[y]"),
                        hspan(" yank "),
                        kspan("[q]"),
                        hspan(" quit"),
                    ])
                }
            }
            _ => Line::from(vec![]),
        }
    };

    f.render_widget(Paragraph::new(line).style(Style::default()), area);
}

fn kspan(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(theme::theme().key_hint_fg))
}
fn hspan(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(theme::theme().hint_fg))
}
