//! One replaceable layout for the currently visible document pane.

use ratatui::text::Line;

use crate::tui::features::workspace::inspector::{DetailPane, DetailTarget};

#[derive(Default)]
pub(crate) struct DocumentCache {
    key: Option<DocumentKey>,
    lines: Vec<Line<'static>>,
}

struct DocumentKey {
    target: DetailTarget,
    pane: DetailPane,
    width: usize,
}

impl DocumentCache {
    pub(crate) fn update(
        &mut self,
        target: &DetailTarget,
        pane: DetailPane,
        width: usize,
        dirty: bool,
        build: impl FnOnce() -> Vec<Line<'static>>,
    ) {
        if !dirty
            && self
                .key
                .as_ref()
                .is_some_and(|key| key.target == *target && key.pane == pane && key.width == width)
        {
            return;
        }
        // Release the previous document before allocating its replacement.
        self.clear();
        self.lines = build();
        self.key = Some(DocumentKey {
            target: target.clone(),
            pane,
            width,
        });
    }

    pub(crate) fn clear(&mut self) {
        self.key = None;
        self.lines = Vec::new();
    }

    pub(crate) fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn unchanged_document_is_reused_and_every_content_key_invalidates_it() {
        let mut cache = DocumentCache::default();
        let builds = Cell::new(0);
        let machine = DetailTarget::Machine("first".into());
        let image = DetailTarget::Image {
            name: "first".into(),
            internal: false,
        };
        let build = || {
            builds.set(builds.get() + 1);
            vec![Line::from(format!("build {}", builds.get()))]
        };

        for _ in 0..100 {
            cache.update(&machine, DetailPane::Details, 80, false, build);
        }
        assert_eq!(builds.get(), 1);

        cache.update(&machine, DetailPane::Details, 80, true, build);
        cache.update(&machine, DetailPane::Details, 30, false, build);
        cache.update(&image, DetailPane::Details, 30, false, build);
        cache.update(&image, DetailPane::ImageOverview, 30, false, build);
        assert_eq!(builds.get(), 5);
        assert_eq!(cache.lines().len(), 1);
        assert_eq!(cache.lines()[0].to_string(), "build 5");

        cache.clear();
        assert!(cache.lines().is_empty());
        cache.update(&image, DetailPane::ImageOverview, 30, false, build);
        assert_eq!(builds.get(), 6);
    }
}
