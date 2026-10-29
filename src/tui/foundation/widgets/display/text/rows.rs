use crate::tui::foundation::text::soft_wrap_text;
use crate::tui::foundation::widgets::display::text::{TextContent, TextLayout, TextLayoutCache};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// Already laid-out styled rows, borrowed from the owning feature's cache.
pub(crate) struct TextRows<'a>(pub(crate) &'a [Line<'a>]);

impl TextContent for TextRows<'_> {
    fn layout(&mut self, _width: u16, top_row: usize) -> TextLayout {
        TextLayout {
            height: self.0.len(),
            top_row,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, top_row: usize) {
        let area = area.intersection(frame.area());
        if area.is_empty() {
            return;
        }
        // Paragraph owns the visible row descriptors; their text borrows the cache.
        let rows: Vec<_> = self
            .0
            .iter()
            .skip(top_row)
            .take(usize::from(area.height))
            .map(|line| Line {
                style: line.style,
                alignment: line.alignment,
                spans: line
                    .spans
                    .iter()
                    .map(|span| Span::styled(span.content.as_ref(), span.style))
                    .collect(),
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), area);
    }
}

/// Immutable form text retains one layout across measurement, drawing and input.
/// A new document replaces both the source and its derived rows together.
pub(crate) struct PlainText {
    source: String,
    layout: TextLayoutCache,
    #[cfg(test)]
    layout_builds: usize,
}

impl PlainText {
    pub(crate) fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            layout: TextLayoutCache::default(),
            #[cfg(test)]
            layout_builds: 0,
        }
    }
}

impl TextContent for PlainText {
    fn layout(&mut self, width: u16, top_row: usize) -> TextLayout {
        self.layout.update(width, || {
            #[cfg(test)]
            {
                self.layout_builds += 1;
            }
            soft_wrap_text(&self.source, usize::from(width))
                .into_iter()
                .map(Line::from)
                .collect()
        });
        TextRows(self.layout.rows()).layout(width, top_row)
    }

    fn render(&self, frame: &mut Frame, area: Rect, top_row: usize) {
        TextRows(self.layout.rows()).render(frame, area, top_row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::TestBackend,
        style::{Color, Style},
        Terminal,
    };

    #[test]
    fn borrowed_rows_keep_paragraph_styles_clipping_and_scroll() {
        let rows = vec![
            Line::styled("Machine", Style::default().fg(Color::Blue)),
            Line::from(vec![
                Span::raw("Name: "),
                Span::styled("容器", Style::default().fg(Color::Green)),
            ]),
            Line::from(""),
            Line::from("continuation"),
            Line::from("last row").right_aligned(),
            Line::from("a long centered row").centered(),
            Line::from(vec![
                Span::styled("highlight ", Style::default().bg(Color::Blue)),
                Span::raw("end 🪐"),
            ])
            .style(Style::default().fg(Color::Yellow)),
            Line::from("tab\tthen e\u{301}"),
        ];
        for width in [6, 14] {
            for scroll in 0..=rows.len() {
                let mut actual = Terminal::new(TestBackend::new(width, 3)).unwrap();
                let mut expected = Terminal::new(TestBackend::new(width, 3)).unwrap();
                actual
                    .draw(|frame| TextRows(&rows).render(frame, frame.area(), scroll))
                    .unwrap();
                expected
                    .draw(|frame| {
                        frame.render_widget(
                            Paragraph::new(rows.clone()).scroll((scroll as u16, 0)),
                            frame.area(),
                        )
                    })
                    .unwrap();
                assert_eq!(actual.backend().buffer(), expected.backend().buffer());
            }
        }
    }

    #[test]
    fn static_text_reuses_measured_rows_across_scrolling_and_frames() {
        let mut content = PlainText::new("words 中文 👩‍💻 ".repeat(20));
        let height = content.layout(40, 0).height;
        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        for offset in 0..height {
            let measured = content.layout(40, offset);
            assert_eq!(measured.height, height);
            terminal
                .draw(|frame| {
                    content.layout(frame.area().width, offset);
                    content.render(frame, frame.area(), offset);
                })
                .unwrap();
        }
        assert_eq!(content.layout_builds, 1);
        assert!(content.layout(20, 0).height > height);
        assert_eq!(content.layout_builds, 2);
        assert_eq!(content.layout(0, 0).height, 0);
    }
}
