use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::tui::features::workspace::inspector::presentation::empty_block;
use crate::tui::features::workspace::inspector::DetailRenderContext;
use crate::tui::foundation::theme;

/// Wrapped-line offsets belong to log presentation; the panel coordinator
/// retains this cache and uses its height for the shared scroll viewport.
#[derive(Default, Clone)]
pub(crate) struct LogRenderCache {
    pub offset_index: Vec<usize>,
    pub wrapped_height: usize,
}

impl LogRenderCache {
    pub(crate) fn compute(lines: &[&str], width: u16) -> Self {
        if width == 0 {
            return Self::default();
        }
        let mut offset_index = Vec::with_capacity(lines.len());
        let mut current_y = 0;
        for line in lines {
            offset_index.push(current_y);
            current_y += paragraph(Line::from(*line)).line_count(width);
        }
        Self {
            offset_index,
            wrapped_height: current_y,
        }
    }

    fn viewport<'a>(
        &self,
        lines: impl Iterator<Item = &'a str>,
        scroll: usize,
        height: u16,
    ) -> Paragraph<'a> {
        let first_line = match self.offset_index.binary_search(&scroll) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        };
        let first_row = self.offset_index.get(first_line).copied().unwrap_or(0);
        let skip = scroll.saturating_sub(first_row);
        // Every source line occupies at least one row. Borrow only the lines
        // needed for this viewport, including its partially visible first line.
        let visible = lines
            .skip(first_line)
            .take(usize::from(height))
            .map(Line::from)
            .collect::<Vec<_>>();
        paragraph(visible).scroll((skip as u16, 0))
    }
}

// Measuring and painting must use the same word wrapping and whitespace rules.
fn paragraph<'a>(text: impl Into<Text<'a>>) -> Paragraph<'a> {
    Paragraph::new(text).wrap(Wrap { trim: false })
}

pub(crate) fn render(
    f: &mut Frame,
    data: &DetailRenderContext<'_>,
    area: Rect,
    scroll: usize,
    cache: &LogRenderCache,
) {
    if data.machine.is_none() {
        f.render_widget(empty_block(), area);
        return;
    }

    let buffer = match data.logs.as_ref() {
        Some(b) => b,
        None => {
            f.render_widget(
                Paragraph::new(vec![Line::from(Span::styled(
                    "No log output.",
                    Style::default().fg(theme::theme().text_secondary),
                ))]),
                area,
            );
            return;
        }
    };

    if buffer.lines.is_empty() {
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "No log output.",
                Style::default().fg(theme::theme().text_secondary),
            ))]),
            area,
        );
        return;
    }

    f.render_widget(
        cache.viewport(buffer.lines.iter().map(String::as_str), scroll, area.height),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, style::Color, widgets::Widget};

    fn samples() -> Vec<String> {
        vec![
            "中".repeat(16),
            "x".repeat(40),
            format!("{}  ", "x".repeat(19)),
            "prefix\tx\ty\tz\tend".into(),
            "👩‍💻".repeat(16),
            "".into(),
            "last log entry".into(),
        ]
    }

    #[test]
    fn offsets_match_rows_painted_by_ratatui() {
        let samples = samples();
        let lines = samples.iter().map(String::as_str).collect::<Vec<_>>();
        for width in [1, 12, 20, 32, 80] {
            let cache = LogRenderCache::compute(&lines, width);
            let mut row = 0;
            for (index, line) in lines.iter().enumerate() {
                assert_eq!(cache.offset_index[index], row);
                let area = Rect::new(0, 0, width, 256);
                let mut buffer = Buffer::empty(area);
                let marker = Line::from(Span::styled("§", Style::default().fg(Color::Red)));
                paragraph(vec![Line::from(*line), marker]).render(area, &mut buffer);
                row += (0..area.height)
                    .find(|&y| (0..width).any(|x| buffer[(x, y)].symbol() == "§"))
                    .expect("the marker follows all wrapped source rows")
                    as usize;
            }
            assert_eq!(cache.wrapped_height, row);
        }
        assert_eq!(LogRenderCache::compute(&lines, 0).wrapped_height, 0);
    }

    #[test]
    fn indexed_viewport_matches_full_paragraph_at_every_scroll_offset() {
        let samples = samples();
        let lines = samples.iter().map(String::as_str).collect::<Vec<_>>();
        for width in [1, 12, 20, 32, 80] {
            let cache = LogRenderCache::compute(&lines, width);
            let area = Rect::new(0, 0, width, 3);
            for scroll in 0..cache.wrapped_height {
                let mut expected = Buffer::empty(area);
                paragraph(lines.iter().copied().map(Line::from).collect::<Vec<_>>())
                    .scroll((scroll as u16, 0))
                    .render(area, &mut expected);
                let mut actual = Buffer::empty(area);
                cache
                    .viewport(lines.iter().copied(), scroll, area.height)
                    .render(area, &mut actual);
                assert_eq!(actual, expected, "width={width}, scroll={scroll}");
            }
        }
    }
}
