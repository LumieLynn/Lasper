use ratatui::text::Line;

/// One width-dependent layout. Its owner invalidates it when source data changes.
#[derive(Default)]
pub(crate) struct TextLayoutCache {
    width: Option<u16>,
    rows: Vec<Line<'static>>,
}

impl TextLayoutCache {
    pub(crate) fn update(&mut self, width: u16, build: impl FnOnce() -> Vec<Line<'static>>) {
        if self.width == Some(width) {
            return;
        }
        self.invalidate();
        self.rows = if width == 0 { Vec::new() } else { build() };
        self.width = Some(width);
    }

    pub(crate) fn invalidate(&mut self) {
        self.width = None;
        self.rows = Vec::new();
    }

    pub(crate) fn needs_layout(&self, width: u16) -> bool {
        self.width != Some(width)
    }

    pub(crate) fn rows(&self) -> &[Line<'static>] {
        &self.rows
    }
}
