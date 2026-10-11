//! Draw already wrapped rows by reference, restricted to the visible viewport.

use ratatui::{
    layout::{Alignment, Rect},
    text::Line,
    Frame,
};

pub(crate) fn render(f: &mut Frame, lines: &[Line<'_>], area: Rect, scroll: usize) {
    let area = area.intersection(f.area());
    if area.is_empty() {
        return;
    }
    for (offset, line) in lines
        .iter()
        .skip(scroll)
        .take(usize::from(area.height))
        .enumerate()
    {
        // Paragraph clips overflowing text before applying alignment. Drawing
        // a Line widget directly would instead keep its right-hand suffix.
        let remaining = area
            .width
            .saturating_sub(line.width().min(usize::from(area.width)) as u16);
        let indent = match line.alignment {
            Some(Alignment::Center) => remaining / 2,
            Some(Alignment::Right) => remaining,
            Some(Alignment::Left) | None => 0,
        };
        f.buffer_mut().set_line(
            area.x + indent,
            area.y + offset as u16,
            line,
            area.width - indent,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::TestBackend,
        style::{Color, Style},
        text::Span,
        widgets::Paragraph,
        Terminal,
    };

    #[test]
    fn borrowed_viewport_keeps_paragraph_styles_clipping_and_scroll() {
        let rows = vec![
            Line::styled("Machine", Style::default().fg(Color::Blue)),
            Line::from(vec![
                Span::raw("Name: "),
                Span::styled("容器", Style::default().fg(Color::Green)),
            ]),
            Line::from(""),
            Line::from("continuation"),
            Line::from("last row").right_aligned(),
        ];
        for width in [6, 14] {
            for scroll in [0, 2, 4, 7] {
                let mut actual = Terminal::new(TestBackend::new(width, 3)).unwrap();
                let mut expected = Terminal::new(TestBackend::new(width, 3)).unwrap();
                actual
                    .draw(|frame| render(frame, &rows, frame.area(), scroll))
                    .unwrap();
                expected
                    .draw(|frame| {
                        frame.render_widget(
                            Paragraph::new(rows.clone()).scroll((scroll as u16, 0)),
                            frame.area(),
                        );
                    })
                    .unwrap();
                assert_eq!(actual.backend().buffer(), expected.backend().buffer());
            }
        }
    }
}
