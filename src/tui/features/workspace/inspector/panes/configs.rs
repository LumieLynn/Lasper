use ratatui::text::Line;

use crate::tui::features::workspace::inspector::presentation::configuration::configuration_file_lines;
use crate::tui::features::workspace::inspector::presentation::empty_lines;
use crate::tui::features::workspace::inspector::presentation::layout::{error_lines, notice_lines};
use crate::tui::features::workspace::inspector::DetailRenderContext;

pub(crate) fn lines(data: &DetailRenderContext<'_>, width: usize) -> Vec<Line<'static>> {
    if data.target.name().is_none() {
        return empty_lines();
    }
    if width == 0 {
        return Vec::new();
    }
    let path = data.config_path.map(|path| path.display().to_string());
    let mut lines = configuration_file_lines(
        path.as_deref(),
        data.config_content.unwrap_or_default(),
        width,
    );
    if data.config_content.is_some() {
        return lines;
    }
    if let Some(error) = data.config_error {
        lines.extend(error_lines(
            &format!("Configuration unavailable: {error}"),
            width,
        ));
    } else {
        let name = data.target.name().unwrap_or("?");
        lines.extend(notice_lines(
            &format!("No .nspawn config file found for machine '{name}'."),
            width,
        ));
    }
    lines
}
