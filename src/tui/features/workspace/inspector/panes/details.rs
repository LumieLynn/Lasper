//! Shared diagnostic surface for machine details and image unit inspection.

use ratatui::text::Line;

use crate::application::inspection::SystemdDropInInspection;
use crate::domain::inspection::MachineProperties;
use crate::tui::features::workspace::inspector::presentation::configuration::configuration_file_lines;
use crate::tui::features::workspace::inspector::presentation::empty_lines;
use crate::tui::features::workspace::inspector::presentation::layout::{error_lines, notice_lines};
use crate::tui::features::workspace::inspector::presentation::property_document::{
    field_lines, inspection_lines,
};
use crate::tui::features::workspace::inspector::DetailRenderContext;

pub(crate) fn lines(data: &DetailRenderContext<'_>, width: usize) -> Vec<Line<'static>> {
    if data.target.name().is_none() {
        return empty_lines();
    }
    let mut lines = details_lines(data.unit_name, data.properties, data.unit_drop_ins, width);
    if data.target.is_image() && data.unit_name.is_none() && width > 0 {
        let mut notice = notice_lines(
            "No corresponding systemd-nspawn unit for this image.",
            width,
        );
        if !lines.is_empty() {
            notice.push(Line::from(""));
        }
        notice.append(&mut lines);
        lines = notice;
    }
    lines
}

/// Unit metadata and configuration files are optional additions to the same
/// property document, rather than a separate page with its own layout.
fn details_lines(
    unit: Option<&str>,
    properties: Result<&MachineProperties, &str>,
    drop_ins: &[SystemdDropInInspection],
    width: usize,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = unit
        .map(|unit| field_lines("Unit", "Unit", unit, width))
        .unwrap_or_default();

    let properties = match properties {
        Ok(properties) => inspection_lines(properties, width),
        Err(error) => error_lines(error, width),
    };
    if !properties.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(properties);
    }
    for drop_in in drop_ins {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(configuration_file_lines(
            Some(&drop_in.path),
            &drop_in.content,
            width,
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inspection::{GROUP_SYSTEMD_SERVICE, GROUP_SYSTEMD_UNIT};
    use crate::tui::foundation::theme;
    use ratatui::{buffer::Buffer, widgets::Widget};
    use ratatui::{layout::Rect, widgets::Paragraph};

    #[test]
    fn unit_properties_and_drop_ins_keep_shared_styles_and_scroll_to_the_last_file() {
        theme::init_theme(theme::Theme::dark());
        let mut properties = MachineProperties::default();
        properties.insert(GROUP_SYSTEMD_UNIT, "ActiveState".into(), "active".into());
        properties.insert(GROUP_SYSTEMD_SERVICE, "MainPID".into(), "1234".into());
        let drop_ins = [
            SystemdDropInInspection {
                path: "/etc/systemd/system/systemd-nspawn@archlinux.service.d/90-lasper.conf"
                    .into(),
                content: "[Service]\nEnvironment=  A=one B=two\n".into(),
            },
            SystemdDropInInspection {
                path: "/etc/systemd/system/systemd-nspawn@archlinux.service.d/99-admin.conf".into(),
                content: "[Service]\n# administrator settings\nCPUWeight=100\n".into(),
            },
        ];
        let wide = details_lines(
            Some("systemd-nspawn@archlinux.service"),
            Ok(&properties),
            &drop_ins,
            100,
        );
        let property_heading = wide
            .iter()
            .find(|line| line.to_string() == "Systemd Unit")
            .unwrap();
        let file_heading = wide
            .iter()
            .find(|line| line.to_string() == drop_ins[0].path)
            .unwrap();
        assert_eq!(file_heading.style, property_heading.style);
        let environment = wide
            .iter()
            .find(|line| line.to_string().starts_with("Environment="))
            .unwrap();
        assert_eq!(
            environment.spans[0].style.fg,
            Some(theme::theme().config_key)
        );
        assert_eq!(
            environment.spans[1].style.fg,
            Some(theme::theme().config_value)
        );
        assert_eq!(environment.to_string(), "Environment=  A=one B=two");

        let rows = details_lines(
            Some("systemd-nspawn@archlinux.service"),
            Ok(&properties),
            &drop_ins,
            16,
        );
        assert!(rows.iter().all(|line| line.width() <= 16));
        let area = Rect::new(0, 0, 16, 3);
        let scroll = rows.len() - usize::from(area.height);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(rows)
            .scroll((scroll as u16, 0))
            .render(area, &mut buffer);
        let last_row = buffer.content[32..]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(last_row.trim_end(), "CPUWeight=100");

        let properties_only = details_lines(None, Ok(&properties), &[], 100);
        assert_eq!(properties_only[0].to_string(), "Systemd Unit");
        assert!(properties_only
            .iter()
            .all(|line| !line.to_string().contains("No corresponding")));
    }
}
