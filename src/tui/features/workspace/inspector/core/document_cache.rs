//! One replaceable layout for the currently visible document pane.

use crate::tui::foundation::widgets::display::text::TextLayoutCache;
use ratatui::text::Line;

use crate::tui::features::workspace::inspector::{DetailPane, DetailTarget};

#[derive(Default)]
pub(crate) struct DocumentCache {
    key: Option<DocumentKey>,
    layout: TextLayoutCache,
}

struct DocumentKey {
    target: DetailTarget,
    pane: DetailPane,
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
        let same_document = self
            .key
            .as_ref()
            .is_some_and(|key| key.target == *target && key.pane == pane);
        if dirty || !same_document {
            self.layout.invalidate();
        }
        self.layout
            .update(u16::try_from(width).unwrap_or(u16::MAX), build);
        if !same_document {
            self.key = Some(DocumentKey {
                target: target.clone(),
                pane,
            });
        }
    }

    pub(crate) fn clear(&mut self) {
        self.key = None;
        self.layout.invalidate();
    }

    pub(crate) fn lines(&self) -> &[Line<'static>] {
        self.layout.rows()
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
