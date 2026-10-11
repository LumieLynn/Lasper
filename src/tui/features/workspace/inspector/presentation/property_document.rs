//! Pure presentation of sectioned fields and complete inspection snapshots.
//!
//! Pages supply overview content or a complete property snapshot. This module
//! owns display order, empty-value handling, semantic colors and wrapping; it
//! does not select targets, query backends or coordinate page state.

use std::borrow::Cow;

use ratatui::{style::Style, text::Line};

use crate::domain::inspection::{
    MachineProperties, PropertyGroup, GROUP_DEPENDENCIES, GROUP_MACHINE, GROUP_SYSTEMD_SERVICE,
    GROUP_SYSTEMD_UNIT,
};
use crate::tui::features::workspace::inspector::presentation::{
    layout::{section_heading, value_lines},
    style::property_style,
};
use crate::tui::foundation::theme;

pub(crate) struct PropertyField<'a> {
    label: &'a str,
    style_key: &'a str,
    value: Cow<'a, str>,
}

impl<'a> PropertyField<'a> {
    pub(crate) fn new(label: &'a str, value: impl Into<Cow<'a, str>>) -> Self {
        Self::property(label, label, value)
    }

    pub(crate) fn property(
        label: &'a str,
        style_key: &'a str,
        value: impl Into<Cow<'a, str>>,
    ) -> Self {
        Self {
            label,
            style_key,
            value: value.into(),
        }
    }

    /// An absent display value cannot satisfy a page's preferred-field
    /// fallback. The document also uses this rule when omitting empty rows.
    pub(crate) fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

pub(crate) struct PropertySection<'a> {
    pub title: &'a str,
    pub fields: Vec<PropertyField<'a>>,
}

/// Explicit sections retain the page's content order. Both overview fields
/// and inspection groups use this same empty-value, style and layout policy.
pub(crate) fn section_lines(sections: &[PropertySection<'_>], width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for section in sections {
        // Sentinel text such as [not set] and [unprintable] is meaningful.
        // Only actually empty display values are absent from the document.
        let mut fields = section
            .fields
            .iter()
            .filter(|field| !field.is_empty())
            .peekable();
        if fields.peek().is_none() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(section_heading(section.title, width));
        for field in fields {
            lines.extend(field_lines(
                field.label,
                field.style_key,
                &field.value,
                width,
            ));
        }
    }
    lines
}

/// A complete inspection becomes the same sectioned document as an overview.
/// Keep unknown groups and keys, and apply deterministic diagnostic ordering.
pub(crate) fn inspection_lines(properties: &MachineProperties, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut groups: Vec<_> = properties.groups.iter().collect();
    groups.sort_by_key(|group| (group_display_priority(group), group.name.as_str()));

    let sections: Vec<_> = groups
        .into_iter()
        .map(|group| {
            let mut pairs: Vec<_> = group.properties.iter().collect();
            pairs.sort_by_key(|(key, _)| key.as_str());
            PropertySection {
                title: &group.name,
                fields: pairs
                    .into_iter()
                    .map(|(key, entry)| PropertyField::new(key.as_str(), entry.display.as_str()))
                    .collect(),
            }
        })
        .collect();
    section_lines(&sections, width)
}

/// A display label may differ from the underlying property's semantic key.
pub(crate) fn field_lines(
    label: &str,
    style_key: &str,
    value: &str,
    width: usize,
) -> Vec<Line<'static>> {
    value_lines(
        label,
        value,
        width,
        Style::default().fg(theme::theme().accent),
        property_style(style_key, value),
    )
}

fn group_display_priority(group: &PropertyGroup) -> u8 {
    match group.name.as_str() {
        GROUP_MACHINE => 0,
        GROUP_SYSTEMD_UNIT => 1,
        GROUP_SYSTEMD_SERVICE => 2,
        GROUP_DEPENDENCIES => 10,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Paragraph, Widget},
    };

    #[test]
    fn empty_display_values_are_hidden_but_sentinels_are_kept() {
        theme::init_theme(theme::Theme::dark());
        let mut properties = MachineProperties::default();
        properties.insert(GROUP_MACHINE, "Empty".into(), String::new());
        properties.insert(GROUP_MACHINE, "Sentinel".into(), "[not set]".into());

        let inspection = inspection_lines(&properties, 48);
        let explicit = section_lines(
            &[
                PropertySection {
                    title: GROUP_MACHINE,
                    fields: vec![
                        PropertyField::new("Empty", ""),
                        PropertyField::new("Sentinel", "[not set]"),
                    ],
                },
                PropertySection {
                    title: "Empty section",
                    fields: vec![PropertyField::new("Empty", "")],
                },
            ],
            48,
        );
        assert_eq!(inspection, explicit);
        let text = inspection.iter().map(Line::to_string).collect::<String>();
        assert!(!text.contains("Empty"));
        assert!(text.contains("Sentinel"));
    }

    #[test]
    fn unknown_groups_and_keys_are_kept_between_service_and_dependencies() {
        theme::init_theme(theme::Theme::dark());
        let mut properties = MachineProperties::default();
        for group in [
            GROUP_DEPENDENCIES,
            "ProviderSpecific",
            GROUP_SYSTEMD_SERVICE,
            GROUP_MACHINE,
        ] {
            properties.insert(group, "UnknownKey".into(), "observed".into());
        }
        let text: Vec<_> = inspection_lines(&properties, 80)
            .iter()
            .map(Line::to_string)
            .collect();
        let headings: Vec<_> = text
            .iter()
            .filter(|line| !line.is_empty() && !line.contains(':'))
            .map(String::as_str)
            .collect();
        assert_eq!(
            headings,
            [
                GROUP_MACHINE,
                GROUP_SYSTEMD_SERVICE,
                "ProviderSpecific",
                GROUP_DEPENDENCIES,
            ]
        );
        assert_eq!(
            text.iter()
                .filter(|line| line.as_str() == "UnknownKey: observed")
                .count(),
            4
        );
    }

    #[test]
    fn grouped_properties_reflow_and_scroll_to_the_last_value() {
        theme::init_theme(theme::Theme::dark());
        let mut properties = MachineProperties::default();
        properties.insert(GROUP_SYSTEMD_SERVICE, "MainPID".into(), "1234".into());
        properties.insert(
            GROUP_SYSTEMD_SERVICE,
            "ExecStart".into(),
            "/usr/bin/systemd-nspawn --boot --machine=archlinux".into(),
        );
        properties.insert(
            GROUP_SYSTEMD_UNIT,
            "Description".into(),
            "容器 archlinux".into(),
        );
        properties.insert(GROUP_MACHINE, "Name".into(), "archlinux".into());

        for width in [12, 18, 40, 80] {
            let lines = inspection_lines(&properties, width);
            assert!(lines.iter().all(|line| line.width() <= width));
            assert_eq!(lines[0].to_string(), "Machine");

            let area = Rect::new(0, 0, width as u16, 3);
            let scroll = lines.len() - usize::from(area.height);
            let expected: Vec<_> = lines[scroll..].iter().map(Line::to_string).collect();
            let mut buffer = Buffer::empty(area);
            Paragraph::new(lines)
                .scroll((scroll as u16, 0))
                .render(area, &mut buffer);
            let actual: Vec<_> = buffer
                .content
                .chunks(width)
                .map(|row| {
                    row.iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();

            assert_eq!(actual, expected);
            assert!(actual.last().unwrap().ends_with("1234"));
        }
    }
}
