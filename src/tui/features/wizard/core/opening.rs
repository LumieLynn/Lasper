//! The temporary view shown while the wizard's host snapshot is loading.

use crate::tui::features::wizard::workflow::WizardOpening;
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::protocol::{Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

impl Component for WizardOpening {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        footer.register(FooterHint::new("Esc", "cancel").on_key(KeyCode::Esc));
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_layer(frame, area, true);
    }

    fn render_layer(&mut self, frame: &mut Frame, area: Rect, foreground: bool) {
        let width = area.width.min(60);
        let height = area.height.min(5);
        let dialog = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block =
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Create Container ")
                .border_style(Style::default().fg(
                    crate::tui::foundation::theme::theme().panel_border(false, foreground, true),
                ));
        let content = block.inner(dialog);
        frame.render_widget(Clear, dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(
            Paragraph::new("Inspecting provisioning capabilities..."),
            content,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.code == KeyCode::Esc {
            EventResult::Ignored
        } else {
            EventResult::Consumed
        }
    }
}
