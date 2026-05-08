/// Generates the shared form-dialog boilerplate: focus management, render,
/// key handling, and focus propagation.  The struct must provide:
///   - `active_comps!(&mut self)` → `Vec<&mut dyn Component>`
///   - `self.focus: FocusTracker`
///   - `self.btn_ok: Button` and `self.btn_cancel: Button`
///   - `fn try_submit(&mut self) -> Option<AppMessage>`
///
/// Usage:
/// ```ignore
/// form_dialog!(BindMountBox, " Add Bind Mount ", (45, 55), [source_path, target_path, readonly, suffix]);
/// ```
macro_rules! form_dialog {
    ($name:ident, $title:expr, ($hp:expr, $wp:expr), [ $( $field:ident ),+ $(,)? ]) => {
        impl $name {
            fn update_focus(&mut self) {
                let mut comps = active_comps!(self);
                self.focus.update_focus(&mut comps, true);
            }

            fn next(&mut self) {
                let comps = active_comps!(self);
                self.focus.next(&comps);
                self.update_focus();
            }

            fn prev(&mut self) {
                let comps = active_comps!(self);
                self.focus.prev(&comps);
                self.update_focus();
            }
        }

        impl crate::ui::core::Component for $name {
            fn render(
                &mut self,
                f: &mut ratatui::Frame,
                area: ratatui::layout::Rect,
            ) {
                let dialog_area = crate::ui::centered_rect($hp, $wp, area);
                f.render_widget(ratatui::widgets::Clear, dialog_area);

                let block = ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .title($title)
                    .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::Cyan));
                let inner = block.inner(dialog_area);
                f.render_widget(block, dialog_area);

                let widget_count: usize = {
                    let mut c = 0usize;
                    $( let _ = &self.$field; c += 1; )+
                    c
                };

                let mut constraints: Vec<ratatui::layout::Constraint> =
                    vec![ratatui::layout::Constraint::Length(3); widget_count];
                constraints.push(ratatui::layout::Constraint::Min(0));
                constraints.push(ratatui::layout::Constraint::Length(3));

                let chunks = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints(constraints)
                    .split(inner);

                let mut _idx = 0usize;
                $(
                    self.$field.render(f, chunks[_idx]);
                    _idx += 1;
                )+

                let btn_row = widget_count + 1;
                let btn_chunks = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Horizontal)
                    .constraints([
                        ratatui::layout::Constraint::Percentage(50),
                        ratatui::layout::Constraint::Percentage(50),
                    ])
                    .split(chunks[btn_row]);

                let ok_area = crate::ui::centered_rect(60, 100, btn_chunks[0]);
                let cancel_area = crate::ui::centered_rect(60, 100, btn_chunks[1]);
                self.btn_ok.render(f, ok_area);
                self.btn_cancel.render(f, cancel_area);
            }

            fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> crate::ui::core::EventResult {
                use crate::ui::core::{AppMessage, EventResult, WizardMessage};
                use crossterm::event::KeyCode;

                match key.code {
                    KeyCode::Tab => {
                        self.next();
                        return EventResult::Consumed;
                    }
                    KeyCode::BackTab => {
                        self.prev();
                        return EventResult::Consumed;
                    }
                    KeyCode::Enter if !self.btn_ok.is_focused() && !self.btn_cancel.is_focused() => {
                        return if let Some(msg) = self.try_submit() {
                            EventResult::Message(msg)
                        } else {
                            EventResult::Consumed
                        };
                    }
                    _ => {}
                }

                let mut comps = active_comps!(self);
                let res = comps[self.focus.active_idx].handle_key(key);
                match res {
                    EventResult::Message(AppMessage::Wizard(WizardMessage::DialogSubmit)) => {
                        if let Some(msg) = self.try_submit() {
                            EventResult::Message(msg)
                        } else {
                            EventResult::Consumed
                        }
                    }
                    EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel)) => res,
                    EventResult::FocusNext => {
                        self.next();
                        EventResult::Consumed
                    }
                    EventResult::FocusPrev => {
                        self.prev();
                        EventResult::Consumed
                    }
                    _ => res,
                }
            }

            fn set_focus(&mut self, focused: bool) {
                if focused {
                    self.update_focus();
                } else {
                    $(
                        self.$field.set_focus(false);
                    )+
                    self.btn_ok.set_focus(false);
                    self.btn_cancel.set_focus(false);
                }
            }

            fn is_focused(&self) -> bool {
                false
                    $( || self.$field.is_focused() )+
                    || self.btn_ok.is_focused()
                    || self.btn_cancel.is_focused()
            }
        }
    };
}

pub mod bind_mount;
pub mod confirmation;
pub mod nvidia_config;
pub mod port_mapping;
pub mod unclassified_file;
pub mod user_editor;
