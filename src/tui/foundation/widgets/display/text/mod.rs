//! Text layout and borrowed drawing, independent of feature data and input.

mod cache;
mod indexed;
mod rows;

pub(crate) use cache::TextLayoutCache;
pub(crate) use indexed::{TextEntryIndex, WrappedEntries};
pub(crate) use rows::{PlainText, TextRows};

use ratatui::{layout::Rect, Frame};

/// Layout reports the old top row's new position without changing scroll state.
pub(crate) struct TextLayout {
    pub(crate) height: usize,
    pub(crate) top_row: usize,
}

/// The viewport needs only layout and drawing. Providers keep their own source
/// storage, styles and invalidation rules; controls never read files or streams.
pub(crate) trait TextContent {
    fn layout(&mut self, width: u16, top_row: usize) -> TextLayout;
    fn render(&self, frame: &mut Frame, area: Rect, top_row: usize);
}

impl<T: TextContent + ?Sized> TextContent for &mut T {
    fn layout(&mut self, width: u16, top_row: usize) -> TextLayout {
        (**self).layout(width, top_row)
    }

    fn render(&self, frame: &mut Frame, area: Rect, top_row: usize) {
        (**self).render(frame, area, top_row);
    }
}
