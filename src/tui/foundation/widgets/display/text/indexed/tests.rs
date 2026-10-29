use super::*;
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::Widget,
    Terminal,
};

fn samples() -> VecDeque<String> {
    VecDeque::from([
        "中".repeat(16),
        "x".repeat(40),
        format!("{}  ", "x".repeat(19)),
        "prefix\tx\ty\tz\tend".into(),
        "👩‍💻".repeat(16),
        "".into(),
        "first physical line\nsecond physical line".into(),
        "last entry".into(),
    ])
}

#[test]
fn entry_heights_match_the_rows_painted_by_ratatui() {
    let source = samples();
    for width in [1, 12, 20, 32, 80] {
        let mut lines = VecDeque::new();
        let mut index = TextEntryIndex::default();
        let mut height = 0;
        for line in &source {
            let area = Rect::new(0, 0, width, 256);
            let mut actual = Buffer::empty(area);
            let mut text = Text::from(line.as_str());
            text.lines.push(Line::from(Span::styled(
                "§",
                Style::default().fg(Color::Red),
            )));
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .render(area, &mut actual);
            height += (0..area.height)
                .find(|&row| (0..width).any(|column| actual[(column, row)].symbol() == "§"))
                .expect("marker follows all source rows") as usize;
            lines.push_back(line.clone());
            let layout = WrappedEntries::new(&lines, 0, &mut index).layout(width, 0);
            assert_eq!(layout.height, height);
        }
        index.update(&lines, 0, 0);
        assert_eq!(index.height(), 0);
    }
}

#[test]
fn indexed_viewport_matches_a_full_paragraph_at_every_scroll_offset() {
    let source = samples();
    let lines = source
        .iter()
        .flat_map(|line| Text::from(line.as_str()).lines)
        .collect::<Vec<_>>();
    for width in [1, 12, 20, 32, 80] {
        let mut index = TextEntryIndex::default();
        let mut content = WrappedEntries::new(&source, 0, &mut index);
        let height = content.layout(width, 0).height;
        let area = Rect::new(0, 0, width, 3);
        let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
        for offset in 0..height {
            let mut expected = Buffer::empty(area);
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((offset as u16, 0))
                .render(area, &mut expected);
            let actual = terminal
                .draw(|frame| content.render(frame, area, offset))
                .unwrap();
            assert_eq!(*actual.buffer, expected, "width={width}, scroll={offset}");
        }
    }
}

#[test]
fn append_eviction_and_resize_reuse_the_index_and_keep_the_viewed_entry() {
    let mut source = VecDeque::from([
        "first entry".into(),
        "second entry".into(),
        "third entry".into(),
    ]);
    let mut index = TextEntryIndex::default();
    index.update(&source, 0, 6);
    assert_eq!(index.measurements, 3);
    for _ in 0..10 {
        index.update(&source, 0, 6);
    }
    assert_eq!(index.measurements, 3);
    let second = paragraph("first entry").line_count(6);
    source.pop_front();
    source.push_back("fourth entry".into());
    let layout = WrappedEntries::new(&source, 1, &mut index).layout(6, second);
    assert_eq!(layout.top_row, 0);
    assert_eq!(index.measurements, 4);
    let layout = WrappedEntries::new(&source, 1, &mut index).layout(30, 0);
    assert_eq!(index.measurements, 7);
    assert_eq!(layout.height, 3);
}
