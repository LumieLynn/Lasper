use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::super::core::utils::empty_block;
use super::super::core::style::property_style;
use crate::render_column_layout;
use crate::app::AppData;

pub fn render(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.entries.is_empty() {
        f.render_widget(empty_block(" All Details "), area);
        return;
    }

    let props = match &data.properties {
        Ok(p) => p,
        Err(e) => {
            let error_text = vec![
                Line::from(""),
                Line::from(vec![
                    ratatui::text::Span::styled(
                        "  Error: ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    ratatui::text::Span::styled(e.clone(), Style::default().fg(Color::Red)),
                ]),
            ];
            f.render_widget(Paragraph::new(error_text).wrap(Wrap { trim: false }), area);
            return;
        }
    };

    let key_width = if area.width > 6 {
        (area.width as f32 * 0.35) as usize
    } else {
        10
    };
    let val_width = area.width as usize - key_width;
    let val_width = val_width.saturating_sub(3).max(10); // buffer

    let mut lines = Vec::new();

    // Sort groups for consistent display order
    let mut sorted_groups = props.groups.clone();
    sorted_groups.sort_by_key(|g| g.display_priority());

    for group in &sorted_groups {
        if group.properties.is_empty() {
            continue;
        }

        // Add Header Row
        lines.push(Line::from(vec![
            ratatui::text::Span::styled(
                format!("[ {} ]", group.name.to_uppercase()),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]));

        let mut pairs: Vec<(&String, &String)> = group.properties.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());

        for (k, v) in pairs {
            let style = property_style(k, v);
            render_column_layout!(lines, k, v, key_width, val_width, style);
        }

        // Add Spacer
        lines.push(Line::from(""));
    }

    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0)),
        area,
    );
}
