use crate::tui::foundation::widgets::display::text::{TextContent, TextLayout};
use ratatui::{
    layout::Rect,
    widgets::{Paragraph, Wrap},
    Frame,
};
use std::collections::VecDeque;

struct EntryLayout {
    id: usize,
    row: usize,
    height: usize,
}

/// The top row is attached to a source entry rather than to the current wrapping.
struct TextAnchor {
    id: usize,
    row: usize,
}

#[derive(Default)]
pub(crate) struct TextEntryIndex {
    entries: VecDeque<EntryLayout>,
    width: Option<u16>,
    #[cfg(test)]
    pub(super) measurements: usize,
}

impl TextEntryIndex {
    fn update(&mut self, lines: &VecDeque<String>, first: usize, width: u16) {
        if self.width != Some(width) {
            self.entries.clear();
            self.width = Some(width);
        }
        if width == 0 {
            return;
        }
        if lines.is_empty() {
            self.entries.clear();
            return;
        }
        while self.entries.front().is_some_and(|entry| entry.id < first) {
            self.entries.pop_front();
        }
        let next = self.entries.back().map_or(first, |entry| entry.id + 1);
        let mut row = self
            .entries
            .back()
            .map_or(0, |entry| entry.row + entry.height);
        for (index, line) in lines.range(next - first..).enumerate() {
            let height = paragraph(line).line_count(width).max(1);
            self.entries.push_back(EntryLayout {
                id: next + index,
                row,
                height,
            });
            row += height;
            #[cfg(test)]
            {
                self.measurements += 1;
            }
        }
    }

    pub(crate) fn height(&self) -> usize {
        self.entries
            .back()
            .map_or(0, |last| last.row + last.height - self.entries[0].row)
    }

    fn row(&self, index: usize) -> usize {
        self.entries[index].row - self.entries[0].row
    }

    fn first_visible(&self, offset: usize) -> usize {
        self.entries
            .partition_point(|entry| entry.row - self.entries[0].row <= offset)
            .saturating_sub(1)
    }

    fn visible_entries(&self, offset: usize) -> impl Iterator<Item = (usize, &EntryLayout)> {
        let first = self.first_visible(offset);
        self.entries.iter().enumerate().skip(first)
    }

    fn anchor(&self, offset: usize) -> Option<TextAnchor> {
        let index = self.first_visible(offset);
        self.entries.get(index).map(|entry| TextAnchor {
            id: entry.id,
            row: offset - self.row(index),
        })
    }

    fn offset(&self, anchor: TextAnchor) -> usize {
        let index = self.entries.partition_point(|entry| entry.id < anchor.id);
        self.entries.get(index).map_or(0, |entry| {
            if entry.id == anchor.id {
                self.row(index) + anchor.row.min(entry.height - 1)
            } else {
                // The viewed entry was evicted: keep the earliest retained entry visible.
                self.row(index)
            }
        })
    }

    /// Borrow and wrap only the entries intersecting the viewport. The cache
    /// retains row offsets, not a second copy of the wrapped log text.
    fn render(&self, frame: &mut Frame, area: Rect, lines: &VecDeque<String>, offset: usize) {
        if area.is_empty() {
            return;
        }
        let mut y = area.y;
        for (index, layout) in self.visible_entries(offset) {
            let skip = offset.saturating_sub(self.row(index));
            let height = (layout.height - skip).min(usize::from(area.bottom() - y)) as u16;
            let entry = &lines[index];
            frame.render_widget(
                paragraph(entry).scroll((skip.min(usize::from(u16::MAX)) as u16, 0)),
                Rect::new(area.x, y, area.width, height),
            );
            y += height;
            if y == area.bottom() {
                break;
            }
        }
    }
}

/// Append-only source entries with a moving retention window. The feature owns
/// both the entries and index, so replacing the source also replaces its layout.
pub(crate) struct WrappedEntries<'a> {
    lines: &'a VecDeque<String>,
    first: usize,
    index: &'a mut TextEntryIndex,
}

impl<'a> WrappedEntries<'a> {
    pub(crate) fn new(
        lines: &'a VecDeque<String>,
        first: usize,
        index: &'a mut TextEntryIndex,
    ) -> Self {
        Self {
            lines,
            first,
            index,
        }
    }
}

impl TextContent for WrappedEntries<'_> {
    fn layout(&mut self, width: u16, top_row: usize) -> TextLayout {
        let anchor = self.index.anchor(top_row);
        self.index.update(self.lines, self.first, width);
        TextLayout {
            height: self.index.height(),
            top_row: anchor.map_or(top_row, |anchor| self.index.offset(anchor)),
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, top_row: usize) {
        self.index.render(frame, area, self.lines, top_row);
    }
}

// Height measurement and painting use exactly the same wrapping and whitespace policy.
pub(super) fn paragraph(text: &str) -> Paragraph<'_> {
    Paragraph::new(text).wrap(Wrap { trim: false })
}

#[cfg(test)]
mod tests;
