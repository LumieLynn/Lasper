use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::foundation::{text::soft_wrap_text, theme};

/// Property groups and configuration files use the same compact heading.
pub(crate) fn section_heading(label: &str, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let style = Style::default()
        .fg(theme::theme().highlight)
        .add_modifier(Modifier::BOLD);
    soft_wrap_text(label, width)
        .into_iter()
        .map(|line| Line::styled(line, style))
        .collect()
}

/// Keep short labeled values on one line. Longer values get the full available
/// width beneath their label instead of being squeezed into a fixed column.
pub(crate) fn value_lines(
    key: &str,
    value: &str,
    width: usize,
    key_style: Style,
    value_style: Style,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let label = format!("{key}:");
    if !value.contains('\n') && label.width() + 1 + value.width() <= width {
        return vec![Line::from(vec![
            Span::styled(label, key_style),
            Span::raw(" "),
            Span::styled(value.to_owned(), value_style),
        ])];
    }

    let mut lines: Vec<_> = soft_wrap_text(&label, width)
        .into_iter()
        .map(|line| Line::styled(line, key_style))
        .collect();
    let indent = if width >= 4 { "  " } else { "" };
    lines.extend(
        soft_wrap_text(value, width - indent.len())
            .into_iter()
            .map(|line| Line::from(vec![Span::raw(indent), Span::styled(line, value_style)])),
    );
    lines
}

pub(crate) fn error_lines(error: &str, width: usize) -> Vec<Line<'static>> {
    let style = Style::default().fg(theme::theme().error);
    value_lines(
        "Error",
        error,
        width,
        style.add_modifier(Modifier::BOLD),
        style,
    )
}

pub(crate) fn notice_lines(notice: &str, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    soft_wrap_text(notice, width)
        .into_iter()
        .map(|line| Line::styled(line, Style::default().fg(theme::theme().text_secondary)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn short_values_stay_with_their_label() {
        let lines = value_lines(
            "ActiveState",
            "active",
            24,
            Style::default(),
            Style::default(),
        );
        assert_eq!(text(&lines), ["ActiveState: active"]);
    }

    #[test]
    fn long_values_use_full_width_below_the_label_and_reflow_on_resize() {
        let value = "/usr/bin/systemd-nspawn --boot --machine=archlinux";
        let narrow = value_lines("ExecStart", value, 32, Style::default(), Style::default());
        assert_eq!(
            text(&narrow),
            [
                "ExecStart:",
                "  /usr/bin/systemd-nspawn --boot",
                "  --machine=archlinux"
            ]
        );
        assert!(narrow.iter().all(|line| line.width() <= 32));

        let wide = value_lines("ExecStart", value, 80, Style::default(), Style::default());
        assert_eq!(text(&wide), [format!("ExecStart: {value}")]);
    }

    #[test]
    fn multiline_and_unicode_values_keep_readable_continuations() {
        let lines = value_lines(
            "路径",
            "一二三四五\n/run",
            8,
            Style::default(),
            Style::default(),
        );
        assert_eq!(text(&lines), ["路径:", "  一二三", "  四五", "  /run"]);
        assert!(lines.iter().all(|line| line.width() <= 8));

        assert!(value_lines("Name", "machine", 0, Style::default(), Style::default()).is_empty());
    }
}
