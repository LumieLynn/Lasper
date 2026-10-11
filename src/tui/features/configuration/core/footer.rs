//! The workspace contributes navigation; each page owns its editing and
//! nested-dialog hints. The same registry supplies drawing and pointer targets.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{ConfigurationPane, ConfigurationView, DraftPreviewState, InspectionState};
use crate::application::configuration::ConfigurationPreview;
use crate::tui::features::configuration::page::PageInteractionContext;
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::scrolling::register_scroll_hints;

impl ConfigurationView {
    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        let context = PageInteractionContext {
            target: &self.target,
            snapshot: match &self.state {
                InspectionState::Ready(snapshot) => Some(snapshot.as_ref()),
                _ => None,
            },
            draft: &self.draft,
        };
        let page = self.navigation.current_page();
        if self.pages.modal_open(page) {
            self.pages.register_footer_hints(page, footer, context);
            return;
        }
        if self.restart_confirmation.is_some() || self.discard.is_some() {
            return;
        }
        if self.saving {
            footer.show_notice("Saving configuration...");
            return;
        }
        footer.register(FooterHint::new("r", "Refresh").on_key(KeyCode::Char('r')));
        footer.register(
            FooterHint::new("Esc", "Close")
                .primary()
                .on_key(KeyCode::Esc),
        );
        footer.register(FooterHint::new("Tab/⇧Tab", "Pane").on_key(KeyCode::Tab));
        match self.pane {
            ConfigurationPane::Navigation => self.navigation.register_footer_hints(footer),
            ConfigurationPane::Content => self.pages.register_footer_hints(page, footer, context),
            ConfigurationPane::Preview => {
                register_scroll_hints(footer);
            }
        }
        let ready_to_save = !self.apply_requires_refresh
            && matches!(
                &self.draft_preview,
                DraftPreviewState::Ready { generation, preview: ConfigurationPreview::Ready { .. } }
                    if *generation == self.draft_generation
            );
        if ready_to_save {
            footer.register(
                FooterHint::new("Ctrl+S", "Save")
                    .on_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            );
        }
        footer.register(FooterHint::new("[/]", "Tabs"));
        footer.register(
            FooterHint::new("?", "Help")
                .primary()
                .on_key(KeyCode::Char('?')),
        );
    }
}
