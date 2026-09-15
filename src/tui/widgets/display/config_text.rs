//! Shared configuration highlighting for the inspector and Configure's Raw
//! view. This is presentation only; it does not interpret configuration values.

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme;

pub(crate) fn highlighted_lines(text: &str) -> impl Iterator<Item = Line<'_>> {
    text.lines().map(highlight_line)
}

fn highlight_line(line: &str) -> Line<'_> {
    let t = theme::theme();
    let trimmed = line.trim();
    if trimmed.starts_with(['#', ';']) {
        Line::styled(line, Style::default().fg(t.text_secondary))
    } else if trimmed.starts_with('[') && trimmed.ends_with(']') {
        Line::styled(
            line,
            Style::default()
                .fg(t.config_section)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(separator) = line.find('=') {
        let (key, value) = line.split_at(separator);
        Line::from(vec![
            Span::styled(key, Style::default().fg(t.config_key)),
            Span::styled(value, Style::default().fg(t.config_value)),
        ])
    } else {
        Line::styled(line, Style::default().fg(t.text_secondary))
    }
}

/// Cache visual rows without losing the style of a wrapped value. Highlight
/// before wrapping: a continuation containing '=' is still part of the value.
pub(crate) fn wrapped_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    for line in highlighted_lines(text) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for grapheme in line.styled_graphemes(Style::default()) {
            let (symbol, cells) = if grapheme.symbol.width() > width {
                ("�", 1)
            } else {
                (grapheme.symbol, grapheme.symbol.width())
            };
            if used > 0 && used + cells > width {
                rows.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            if let Some(span) = spans.last_mut().filter(|span| span.style == grapheme.style) {
                span.content.to_mut().push_str(symbol);
            } else {
                spans.push(Span::styled(symbol.to_owned(), grapheme.style));
            }
            used += cells;
        }
        rows.push(Line::from(spans));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_highlighting_keeps_comments_and_indented_sections_intact() {
        theme::init_theme(theme::Theme::dark());
        let lines =
            highlighted_lines("  [Files]  \n# Bind=comment\n; Key=comment\nBind=/tmp/socket\n")
                .collect::<Vec<_>>();
        assert_eq!(lines[0].to_string(), "  [Files]  ");
        assert_eq!(lines[0].style.fg, Some(theme::theme().config_section));
        assert_eq!(lines[1].style.fg, Some(theme::theme().text_secondary));
        assert_eq!(lines[2].style.fg, Some(theme::theme().text_secondary));
        assert_eq!(lines[3].spans[0].style.fg, Some(theme::theme().config_key));
        assert_eq!(
            lines[3].spans[1].style.fg,
            Some(theme::theme().config_value)
        );
    }

    #[test]
    fn wrapping_preserves_value_styles_whitespace_and_wide_graphemes() {
        theme::init_theme(theme::Theme::dark());
        let text = "Key=  宿主/path=a=b";
        let rows = wrapped_lines(text, 8);
        assert_eq!(rows.iter().map(Line::to_string).collect::<String>(), text);
        assert!(rows.iter().all(|line| line.width() <= 8));
        assert!(rows
            .iter()
            .skip(1)
            .flat_map(|line| &line.spans)
            .all(|span| span.style.fg == Some(theme::theme().config_value)));
        assert_eq!(wrapped_lines("宿", 1)[0].to_string(), "�");
    }
}
