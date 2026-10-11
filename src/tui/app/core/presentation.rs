//! Compose global notices, overlay hints and workspace hints in priority order.

use crate::tui::app::App;
use crate::tui::foundation::footer::{FooterModel, FooterRegistry};
use std::collections::HashSet;

impl App {
    pub(crate) fn footer_model(&self, removing_images: &HashSet<String>) -> FooterModel<'_> {
        // Messages report asynchronous operations, independent of the input
        // owner. A wizard or its nested dialog must not postpone their display.
        if let Some((text, level)) = &self.ui.status_message {
            return FooterModel::Message { text, level };
        }
        if let Some(footer) = self.ui.overlays.footer_model() {
            return footer;
        }
        let mut footer = FooterRegistry::new();
        self.workspace
            .register_footer_hints(&mut footer, &self.terminal, removing_images);
        FooterModel::Hints(footer)
    }
}
