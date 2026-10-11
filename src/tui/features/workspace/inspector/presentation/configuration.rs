//! File presentation shared by the Config pane and unit drop-in inspection.

use ratatui::text::Line;

use crate::tui::features::workspace::inspector::presentation::layout::section_heading;
use crate::tui::foundation::widgets::display::config_text;

pub(crate) fn configuration_file_lines(
    path: Option<&str>,
    content: &str,
    width: usize,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = path
        .map(|path| section_heading(path, width))
        .unwrap_or_default();
    lines.extend(config_text::wrapped_lines(
        content,
        u16::try_from(width).unwrap_or(u16::MAX),
    ));
    lines
}
