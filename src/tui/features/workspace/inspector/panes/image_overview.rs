use ratatui::text::Line;

use crate::domain::runtime::ImageEntry;
use crate::tui::features::workspace::inspector::presentation::empty_lines;
use crate::tui::features::workspace::inspector::presentation::layout::notice_lines;
use crate::tui::features::workspace::inspector::presentation::property_document::{
    section_lines, PropertyField, PropertySection,
};
use crate::tui::features::workspace::inspector::DetailRenderContext;

pub(crate) fn lines(data: &DetailRenderContext<'_>, width: usize) -> Vec<Line<'static>> {
    data.image
        .map(|image| image_lines(image, width))
        .unwrap_or_else(empty_lines)
}

fn image_lines(image: &ImageEntry, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut management = vec![
        PropertyField::new("Visibility", image.visibility().label()),
        PropertyField::new("Removal", image.removal_label()),
    ];
    if let Some(path) = &image.dbus_object_path {
        management.push(PropertyField::new("D-Bus object", path.as_str()));
    }
    let mut lines = section_lines(
        &[
            PropertySection {
                title: "Image",
                fields: vec![
                    PropertyField::new("Name", image.name.as_str()),
                    PropertyField::new("Type", image.image_type.as_str()),
                ],
            },
            PropertySection {
                title: "Storage",
                fields: vec![
                    PropertyField::property(
                        "Read only",
                        "ReadOnly",
                        if image.readonly { "yes" } else { "no" },
                    ),
                    PropertyField::property(
                        "Usage",
                        "Usage",
                        image.usage.as_deref().unwrap_or("unknown"),
                    ),
                ],
            },
            PropertySection {
                title: "Management",
                fields: management,
            },
        ],
        width,
    );
    if image.image_type == "mstack" {
        lines.push(Line::from(""));
        lines.extend(notice_lines(
            "OCI application image; storage ownership remains with systemd.",
            width,
        ));
    }
    lines
}
