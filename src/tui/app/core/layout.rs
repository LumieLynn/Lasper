use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::tui::app::App;
use crate::tui::foundation::term::CursorStyle;
use crate::tui::foundation::theme;
use std::collections::HashSet;

pub(crate) fn render(f: &mut Frame, app: &mut App) -> Option<CursorStyle> {
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
    let removing_images = app.services.image_lifecycle.active_images();
    let cursor_style = render_content(f, app, rows[1], &removing_images);
    let overlay_area = Rect {
        height: rows[2].y.saturating_sub(area.y),
        ..area
    };
    app.ui.overlays.render(f, rows[1], overlay_area);
    app.ui.footer_hits = app.footer_model(&removing_images).render(f, rows[2]);
    cursor_style
}

// Title

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::theme();
    let badge = match app.permissions.level() {
        crate::composition::PermissionLevel::Root => Span::styled(
            " [ ROOT ] ",
            Style::default()
                .fg(t.badge_root)
                .add_modifier(Modifier::BOLD),
        ),
        crate::composition::PermissionLevel::Elevated => Span::styled(
            " [ SUDO ] ",
            Style::default()
                .fg(t.badge_systemd_tools)
                .add_modifier(Modifier::BOLD),
        ),
        crate::composition::PermissionLevel::User => {
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

    if !app.workspace.resources.catalog.dbus_active {
        spans.push(Span::styled(
            " [ ⚡ Systemd Tools ] ",
            Style::default()
                .fg(t.badge_systemd_tools)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled(
        format!(
            "  {} machine(s)  {} image(s)  {} internal",
            app.workspace.resources.catalog.entries.len(),
            app.workspace.resources.catalog.images.len(),
            app.workspace.resources.catalog.internal_images.len()
        ),
        Style::default().fg(t.text_secondary),
    ));

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line).style(Style::default()), area);
}

// Content

fn render_content(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    removing_images: &HashSet<String>,
) -> Option<CursorStyle> {
    let overlay_visible = app.ui.modal_layer().is_some();
    app.workspace
        .render(f, area, &mut app.terminal, removing_images, overlay_visible)
}
