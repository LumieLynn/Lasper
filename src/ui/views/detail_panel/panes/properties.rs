use ratatui::{
    layout::Rect,
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::app::AppData;
use super::super::core::utils::empty_block;
use super::super::core::style::property_style;
use crate::render_column_layout;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" Properties "), area);
        return;
    }

    let props = match &data.properties {
        Ok(p) => p,
        Err(e) => {
            let error_text = vec![
                ratatui::text::Line::from(""),
                ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(
                        "  Error: ",
                        ratatui::style::Style::default()
                            .fg(ratatui::style::Color::Red)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ),
                    ratatui::text::Span::styled(e.clone(), ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
                ]),
            ];
            f.render_widget(
                Paragraph::new(error_text)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
    };

    let pairs = props.get_summary();

    let key_width = if area.width > 6 {
        (area.width as f32 * 0.35) as usize
    } else {
        10
    };
    let val_width = area.width as usize - key_width;
    let val_width = val_width.saturating_sub(3).max(10); // buffer

    let mut lines = Vec::new();

    for (k, v) in pairs {
        let style = property_style(k.as_str(), v.as_str());
        render_column_layout!(lines, k, v, key_width, val_width, style);
    }

    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0)),
        area,
    );
}
