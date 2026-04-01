use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::App;
use crate::nspawn::StatusLevel;
use crate::ui::{
    views::container_list, views::detail_panel, centered_rect, 
    widgets::power_menu::PowerMenu, core::Component
};

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
    if app.ui.show_power_menu { PowerMenu::new(app.ui.power_menu_selected).render(f, area); }
    if app.ui.show_wizard  { 
        if let Some(w) = &mut app.ui.wizard {
            w.render(f, area);
        }
    }
    if app.ui.show_help    { render_help(f); }
}


// ── Title ─────────────────────────────────────────────────────────────────────

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let badge = if app.is_root {
        Span::styled(" ⚡ ROOT ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" ⚠  READ-ONLY — run with sudo for full control ",
            Style::default().fg(Color::Black).bg(Color::Yellow))
    };

    let mut spans = vec![
        Span::styled(" Lasper ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        badge,
    ];

    if !app.data.dbus_active {
        spans.push(Span::styled(" ⚡ CMD-MODE ",
            Style::default().fg(Color::Black).bg(Color::Rgb(255, 140, 0)).add_modifier(Modifier::BOLD)));
    }

    spans.push(Span::styled(format!("  {} container(s)", app.data.entries.len()), Style::default().fg(Color::DarkGray)));

    let line = Line::from(spans);
    f.render_widget(
        Paragraph::new(line).style(Style::default()),
        area,
    );
}

// ── Content ───────────────────────────────────────────────────────────────────

fn render_content(f: &mut Frame, app: &mut App, area: Rect) {
    app.ui.pane_height = area.height.saturating_sub(2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);
    container_list::render(f, app, cols[0]);
    detail_panel::render(f, app, cols[1]);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Some((msg, level)) = &app.ui.status_message {
        let color = match level {
            StatusLevel::Info    => Color::White,
            StatusLevel::Success => Color::Green,
            StatusLevel::Warn    => Color::Rgb(255, 140, 0),
            StatusLevel::Error   => Color::Red,
        };
        Line::from(vec![Span::raw("  "), Span::styled(msg.as_str(), Style::default().fg(color))])
    } else {
        Line::from(vec![
            kspan("[j/k]"), hspan(" nav "),
            kspan("[p]"),   hspan(" prop "),
            kspan("[d]"),   hspan(" det "),
            kspan("[s]"),   hspan(" start "),
            kspan("[S]"),   hspan(" poweroff "),
            kspan("[x/⏎]"), hspan(" actions "),
            kspan("[n/a]"), hspan(" new/import "),
            kspan("[r]"),   hspan(" refresh "),
            kspan("[?]"),   hspan(" help "),
            kspan("[q]"),   hspan(" quit"),
        ])
    };

    f.render_widget(
        Paragraph::new(line).style(Style::default()),
        area,
    );
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
        Line::from(Span::styled("  Keybindings", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
        Line::from(""),
        hrow("j / ↓", "Select next"),
        hrow("k / ↑", "Select previous"),
        Line::from(""),
        hrow("p    ", "Properties pane"),
        hrow("d    ", "Full details pane"),
        hrow("l    ", "Logs pane (running only)"),
        hrow("c    ", "Config pane (.nspawn file)"),
        Line::from(""),
        hrow("s    ", "Start container  [root]"),
        hrow("S    ", "Poweroff container [root]"),
        hrow("x / ⏎", "Actions / Power menu  [root]"),
        Line::from(""),
        hrow("n    ", "New container / Import wizard  [root]"),
        Line::from(""),
        hrow("r    ", "Refresh list"),

        hrow("?    ", "Toggle help"),
        hrow("q    ", "Quit"),
        Line::from(""),
        Line::from(Span::styled("  Press any key to close", Style::default().fg(Color::DarkGray))),
    ];
    f.render_widget(
        Paragraph::new(rows).block(
            Block::default().title(" Help ").borders(Borders::ALL)
                .style(Style::default().fg(Color::Cyan).bg(Color::Rgb(20, 20, 30))),
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

// ── Utility ───────────────────────────────────────────────────────────────────

