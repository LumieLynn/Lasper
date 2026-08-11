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
        format!(
            "  {} machine(s)  {} image(s)  {} internal",
            app.data.entries.len(),
            app.data.images.len(),
            app.data.internal_images.len()
        ),
        Style::default().fg(t.text_secondary),
    ));

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line).style(Style::default()), area);
}

// Content

fn render_content(f: &mut Frame, app: &mut App, area: Rect) {
    let machines_focused = app.ui.focus.active_idx == 0;
    let images_focused = app.ui.focus.active_idx == 1;
    let detail_focused = app.ui.focus.active_idx == 2;
    let terminal_focused = app.ui.focus.active_idx == 3;
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

    let left_area = cols[0];
    let right_area = cols[1];
    let left_chunks = split_left_column(
        left_area,
        app.ui.left_machines_pct,
        app.ui.image_info_height,
    );
    let machines_area = left_chunks[0];
    let images_area = left_chunks[1];
    let image_info_area = left_chunks[2];

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
    app.ui.panel_layout.machines = machines_area;
    app.ui.panel_layout.images = images_area;
    app.ui.panel_layout.image_info = image_info_area;
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

    app.ui.pane_height = right_area.height.saturating_sub(2);
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
        machines_area,
        &app.data.entries,
        app.data.selected,
        machines_focused,
        resize_mode,
    );
    app.ui.image_list.set_focus(images_focused);
    let (images, image_selected) = if app.ui.image_list.shows_internal() {
        (
            app.data.internal_images.as_slice(),
            app.data.internal_image_selected,
        )
    } else {
        (app.data.images.as_slice(), app.data.image_selected)
    };
    app.ui
        .image_list
        .render(f, images_area, images, image_selected, resize_mode);
    render_image_detail(f, app, image_info_area, resize_mode);

    if !maximized {
        app.ui
            .detail_panel
            .render_with_data(f, detail_area, &mut app.data, resize_mode);
    }
}

fn split_left_column(
    area: Rect,
    machines_pct: u16,
    requested_info_height: u16,
) -> std::rc::Rc<[Rect]> {
    let constraints = if area.height >= 15 {
        let info_height = requested_info_height.clamp(5, area.height.saturating_sub(6));
        let flexible_height = area.height.saturating_sub(info_height);
        let machines_height = ((u32::from(flexible_height) * u32::from(machines_pct)) / 100)
            .clamp(3, u32::from(flexible_height.saturating_sub(3)))
            as u16;
        vec![
            Constraint::Length(machines_height),
            Constraint::Length(flexible_height.saturating_sub(machines_height)),
            Constraint::Length(info_height),
        ]
    } else {
        vec![
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ]
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
}

fn render_image_detail(f: &mut Frame, app: &mut App, area: Rect, resize_mode: bool) {
    let border_color =
        crate::ui::panel_border_color(resize_mode, app.ui.focus.active_idx == 1, false);
    let block = ratatui::widgets::Block::default()
        .title(" Image Information ")
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(image) = app.selected_image() else {
        app.ui.image_info_scroll = 0;
        app.ui.image_info_max_scroll = 0;
        f.render_widget(
            Paragraph::new("  No image selected")
                .style(Style::default().fg(theme::theme().text_secondary)),
            inner,
        );
        return;
    };
    let lines = image_information_lines(image, inner.width);
    let content_height = lines.len();
    let viewport_height = usize::from(inner.height);
    let max_scroll = content_height.saturating_sub(viewport_height);
    app.ui.image_info_max_scroll = max_scroll.min(u16::MAX as usize) as u16;
    app.ui.image_info_scroll = app.ui.image_info_scroll.min(app.ui.image_info_max_scroll);
    f.render_widget(
        Paragraph::new(lines).scroll((app.ui.image_info_scroll, 0)),
        inner,
    );

    if max_scroll > 0 {
        // Ratatui maps positions from 0 through content_length - 1. Our
        // position is a viewport offset, so the state length is the number of
        // valid offsets rather than the number of rendered content rows.
        let mut state = ratatui::widgets::ScrollbarState::new(max_scroll.saturating_add(1))
            .position(usize::from(app.ui.image_info_scroll))
            .viewport_content_length(viewport_height);
        let scrollbar = ratatui::widgets::Scrollbar::default()
            .orientation(ratatui::widgets::ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let scrollbar_area = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }
}

fn image_information_lines(image: &crate::nspawn::ImageEntry, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (label, value) in [
        ("Name", image.name.as_str()),
        ("Type", image.image_type.as_str()),
        ("Visibility", image.visibility().label()),
        ("Removal", image.removal_label()),
        ("Read-only", if image.readonly { "yes" } else { "no" }),
        ("Usage", image.usage.as_deref().unwrap_or("unknown")),
        (
            "D-Bus path",
            image.object_path.as_deref().unwrap_or("unavailable"),
        ),
    ] {
        push_information_field(&mut lines, label, value, width);
    }
    lines
}

fn push_information_field(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    value: &str,
    width: u16,
) {
    const LABEL_WIDTH: usize = 10;
    const GAP: usize = 2;
    let width = usize::from(width);
    let label_style = Style::default().fg(theme::theme().text_secondary);

    if width > LABEL_WIDTH + GAP {
        let value_width = width - LABEL_WIDTH - GAP;
        for (index, value_line) in crate::ui::soft_wrap_text(value, value_width)
            .into_iter()
            .enumerate()
        {
            let label = if index == 0 {
                format!("{label:<width$}", width = LABEL_WIDTH)
            } else {
                " ".repeat(LABEL_WIDTH)
            };
            lines.push(Line::from(vec![
                Span::styled(label, label_style),
                Span::raw(" ".repeat(GAP)),
                Span::raw(value_line),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(label, label_style)));
        let value_width = width.saturating_sub(2).max(1);
        for value_line in crate::ui::soft_wrap_text(value, value_width) {
            lines.push(Line::from(format!("  {value_line}")));
        }
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
        let vertical_hint = match app.ui.focus.active_idx {
            0 => " machines taller/shorter",
            1 => " image info shorter/taller",
            _ if app.data.terminal.is_showing() => " detail taller/shorter",
            _ => " no vertical split",
        };
        Line::from(vec![
            kspan("[Esc/R/q]"),
            hspan(" exit resize "),
            kspan("[←/h →/l]"),
            hspan(" list width "),
            kspan("[↓/j ↑/k]"),
            hspan(vertical_hint),
        ])
    } else {
        match app.ui.focus.active_idx {
            0 => Line::from(vec![
                kspan("[j/k]"),
                hspan(" nav "),
                kspan("[S]"),
                hspan(" poweroff "),
                kspan("[x/⏎]"),
                hspan(" actions "),
                kspan("[t]"),
                hspan(" terminal "),
                kspan("[n/a]"),
                hspan(" new "),
                kspan("[Tab/⇧Tab]"),
                hspan(" panels "),
                kspan("[?]"),
                hspan(" help"),
            ]),
            1 if app.ui.image_list.shows_internal() => {
                let mut spans = vec![kspan("[j/k]"), hspan(" nav ")];
                if app.selected_image().is_some_and(|image| {
                    !crate::nspawn::models::ImageEntry::is_protected_name(&image.name)
                }) {
                    spans.extend([kspan("[D]"), hspan(" delete internal ")]);
                }
                spans.extend([
                    kspan("[[/]]"),
                    hspan(" image tabs "),
                    kspan("[PgUp/Dn]"),
                    hspan(" info "),
                    kspan("[r]"),
                    hspan(" refresh "),
                    kspan("[Tab/⇧Tab]"),
                    hspan(" panels "),
                    kspan("[?]"),
                    hspan(" help"),
                ]);
                Line::from(spans)
            }
            1 => Line::from(vec![
                kspan("[j/k]"),
                hspan(" nav "),
                kspan("[s]"),
                hspan(" start "),
                kspan("[x/⏎]"),
                hspan(" actions "),
                kspan("[D]"),
                hspan(" delete "),
                kspan("[r]"),
                hspan(" refresh "),
                kspan("[[/]]"),
                hspan(" image tabs "),
                kspan("[PgUp/Dn]"),
                hspan(" info "),
                kspan("[Tab/⇧Tab]"),
                hspan(" panels"),
            ]),
            2 => Line::from(vec![
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
            3 => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_column_keeps_compact_image_information_panel() {
        let area = Rect::new(2, 3, 40, 30);
        let chunks = split_left_column(area, 50, 9);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2].height, 9);
        assert_eq!(chunks[0].x, area.x);
        assert_eq!(chunks[2].y + chunks[2].height, area.y + area.height);
        assert_eq!(
            chunks.iter().map(|chunk| chunk.height).sum::<u16>(),
            area.height
        );
    }

    #[test]
    fn left_column_shrinks_all_panels_in_small_terminals() {
        let area = Rect::new(0, 0, 20, 12);
        let chunks = split_left_column(area, 50, 9);

        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|chunk| chunk.height > 0));
        assert_eq!(
            chunks.iter().map(|chunk| chunk.height).sum::<u16>(),
            area.height
        );
    }

    #[test]
    fn long_values_wrap_without_losing_content() {
        use unicode_width::UnicodeWidthStr;

        let value = "/org/freedesktop/machine1/image/_2eoci_2dsha256_3averylongdigest";
        let wrapped = crate::ui::soft_wrap_text(value, 12);

        assert!(wrapped.len() > 1);
        assert!(wrapped
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 12));
        assert_eq!(wrapped.concat(), value);
    }

    #[test]
    fn unicode_wrapping_uses_terminal_cell_width() {
        use unicode_width::UnicodeWidthStr;

        let value = "路径/very-long-value";
        let wrapped = crate::ui::soft_wrap_text(value, 8);

        assert!(wrapped
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 8));
        assert_eq!(wrapped.concat(), value);
    }

    #[test]
    fn image_information_rows_include_soft_wrapped_dbus_path() {
        crate::ui::theme::init_theme(crate::ui::theme::Theme::dark());
        let image = crate::nspawn::ImageEntry {
            name: "fedora-44".into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            object_path: Some(
                "/org/freedesktop/machine1/image/_2eoci_2dsha256_3averylongdigest".into(),
            ),
        };

        let lines = image_information_lines(&image, 24);

        // Seven fields are rendered at minimum; the long object path must add
        // physical rows instead of remaining one clipped logical line.
        assert!(lines.len() > 7);
    }
}
