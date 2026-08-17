use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::super::core::utils::empty_block;
use crate::app::AppData;
use crate::nspawn::models::ImageEntry;

fn selected_image(data: &AppData) -> Option<&ImageEntry> {
    data.detail_target.name().and_then(|name| {
        data.images
            .iter()
            .chain(data.internal_images.iter())
            .find(|image| image.name == name)
    })
}

pub fn render_overview(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    let Some(image) = selected_image(data) else {
        f.render_widget(empty_block(" Image Overview "), area);
        return;
    };
    let mut lines: Vec<_> = overview_fields(image)
        .into_iter()
        .map(|(label, value)| field(label, value))
        .collect();
    if image.image_type == "mstack" {
        lines.push(Line::from(""));
        lines.push(
            Line::from("OCI application image; storage ownership remains with systemd.")
                .style(Style::default().fg(crate::ui::theme::theme().text_secondary)),
        );
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

pub fn overview_line_count(data: &AppData, width: usize) -> usize {
    let Some(image) = selected_image(data) else {
        return 0;
    };
    let field_lines = overview_fields(image)
        .into_iter()
        .map(|(label, value)| crate::ui::soft_wrap_text(&format!("{label}: {value}"), width).len())
        .sum::<usize>();
    if image.image_type == "mstack" {
        field_lines.saturating_add(1).saturating_add(
            crate::ui::soft_wrap_text(
                "OCI application image; storage ownership remains with systemd.",
                width,
            )
            .len(),
        )
    } else {
        field_lines
    }
}

fn overview_fields(image: &ImageEntry) -> [(&'static str, &str); 7] {
    [
        ("Name", &image.name),
        ("Type", &image.image_type),
        ("Visibility", image.visibility().label()),
        ("Removal", image.removal_label()),
        ("Read-only", if image.readonly { "yes" } else { "no" }),
        ("Usage", image.usage.as_deref().unwrap_or("unknown")),
        (
            "D-Bus object path",
            image.dbus_object_path.as_deref().unwrap_or("unavailable"),
        ),
    ]
}

pub fn render_unit(f: &mut Frame, data: &AppData, area: Rect, scroll: u16) {
    if data.detail_target.name().is_none() {
        f.render_widget(empty_block(" Systemd Unit "), area);
        return;
    }
    let t = crate::ui::theme::theme();
    let mut lines = Vec::new();
    if let Some(unit) = &data.unit_name {
        lines.push(field("Unit", unit));
        lines.push(Line::from(""));
    } else {
        lines.push(
            Line::from("No corresponding systemd-nspawn unit for this image.")
                .style(Style::default().fg(t.text_secondary)),
        );
    }
    if let Ok(properties) = &data.properties {
        for group in &properties.groups {
            if group.properties.is_empty() {
                continue;
            }
            lines.push(Line::from(vec![Span::styled(
                format!("[ {} ]", group.name.to_uppercase()),
                Style::default()
                    .fg(t.highlight)
                    .add_modifier(Modifier::BOLD),
            )]));
            let mut pairs: Vec<_> = group.properties.iter().collect();
            pairs.sort_by_key(|(key, _)| key.as_str());
            for (key, value) in pairs {
                lines.push(Line::from(vec![
                    Span::styled(format!("{} = ", key), Style::default().fg(t.config_key)),
                    Span::styled(value.clone(), Style::default().fg(t.config_value)),
                ]));
            }
            lines.push(Line::from(""));
        }
    } else {
        lines.push(Line::from("Unit properties unavailable.").style(Style::default().fg(t.error)));
    }
    for drop_in in &data.unit_drop_ins {
        lines.push(Line::from(vec![Span::styled(
            format!("--- {} ---", drop_in.path),
            Style::default()
                .fg(t.config_section)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.extend(
            drop_in
                .content
                .lines()
                .map(|line| Line::from(line.to_string())),
        );
        lines.push(Line::from(""));
    }
    if lines.is_empty() {
        lines.push(
            Line::from("No unit drop-ins found.").style(Style::default().fg(t.text_secondary)),
        );
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn field(label: &str, value: &str) -> Line<'static> {
    let t = crate::ui::theme::theme();
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(t.text_secondary)),
        Span::styled(value.to_string(), Style::default().fg(t.text_primary)),
    ])
}
