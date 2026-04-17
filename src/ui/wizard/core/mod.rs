pub mod context;
pub mod manager;

use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Frame;
use crate::ui::core::Component;

/// Renders an editor overlay centered on the screen.
pub fn render_editor_overlay(
    f: &mut Frame,
    title: &str,
    width_pct: u16,
    height_pct: u16,
    editor: &mut dyn Component,
) {
    let area = crate::ui::centered_rect(width_pct, height_pct, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title));
    let inner_area = block.inner(area);
    f.render_widget(block, area);
    editor.render(f, inner_area);
}

#[macro_export]
macro_rules! impl_wizard_nav {
    ($name:ident, $comps_macro:ident) => {
        impl $name {
            fn update_focus(&mut self) {
                let mut comps = $comps_macro!(self);
                let len = comps.len();
                if self.focus.active_idx >= len {
                    self.focus.active_idx = len.saturating_sub(1);
                }
                self.focus.update_focus(&mut comps, true);
            }

            fn next(&mut self) {
                let mut comps = $comps_macro!(self);
                self.focus.next(&mut comps);
                self.update_focus();
            }

            fn prev(&mut self) {
                let mut comps = $comps_macro!(self);
                self.focus.prev(&mut comps);
                self.update_focus();
            }
        }
    };
}

#[macro_export]
macro_rules! delegate_wizard_navigation {
    ($self:ident, $key:ident, $comps_macro:ident) => {{
        match $key.code {
            ::crossterm::event::KeyCode::Tab => {
                $self.next();
                return $crate::ui::core::EventResult::Consumed;
            }
            ::crossterm::event::KeyCode::BackTab => {
                $self.prev();
                return $crate::ui::core::EventResult::Consumed;
            }
            _ => {}
        }

        let mut comps = $comps_macro!($self);
        if $self.focus.active_idx < comps.len() {
            let res = comps[$self.focus.active_idx].handle_key($key);
            match res {
                $crate::ui::core::EventResult::FocusNext => {
                    $self.next();
                    $crate::ui::core::EventResult::Consumed
                }
                $crate::ui::core::EventResult::FocusPrev => {
                    $self.prev();
                    $crate::ui::core::EventResult::Consumed
                }
                _ => res,
            }
        } else {
            $crate::ui::core::EventResult::Ignored
        }
    }};
}
