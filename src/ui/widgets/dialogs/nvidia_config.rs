use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;
use crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode;
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};

use crate::ui::widgets::inputs::button::Button;
use crate::ui::widgets::inputs::text_box::TextBox;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::widgets::selectors::radio_group::RadioGroup;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        let mode_idx = $self.mode.selected_idx();
        let mut comps: Vec<&mut dyn Component> = vec![
            &mut $self.gpu_device,
            &mut $self.mode,
        ];
        if mode_idx == 1 {
            for (_, tb) in &mut $self.dest_inputs {
                comps.push(tb);
            }
        }
        comps.push(&mut $self.inject_env);
        comps.push(&mut $self.btn_ok);
        comps.push(&mut $self.btn_cancel);
        comps
    }};
}

#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaConfigResult {
    pub gpu_device: String,
    pub mode: NvidiaPassthroughMode,
    pub category_destinations: Vec<(NvidiaFileCategory, String)>,
    pub inject_env: bool,
}

pub struct NvidiaConfigDialog {
    gpu_device: RadioGroup,
    mode: RadioGroup,
    dest_inputs: Vec<(NvidiaFileCategory, TextBox)>,
    inject_env: Checkbox,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    scroll_offset: u16,
    on_submit: Box<dyn Fn(NvidiaConfigResult) -> AppMessage>,
}

impl NvidiaConfigDialog {
    pub fn new(
        gpu_devices: Vec<String>,
        active_categories: Vec<NvidiaFileCategory>,
        on_submit: impl Fn(NvidiaConfigResult) -> AppMessage + 'static,
    ) -> Self {
        let device_options = if gpu_devices.is_empty() {
            vec!["all".to_string()]
        } else {
            gpu_devices
        };

        let mut display = NvidiaFileCategory::all_static();
        for cat in active_categories {
            if !display.contains(&cat) {
                display.push(cat);
            }
        }
        display.sort_by_key(|c| format!("{:?}", c));

        let dest_inputs = display
            .into_iter()
            .map(|cat| {
                let default_dest = match cat {
                    NvidiaFileCategory::Lib64 => "/usr/lib",
                    NvidiaFileCategory::Lib32 => "/usr/lib32",
                    NvidiaFileCategory::Bin => "/usr/bin",
                    NvidiaFileCategory::Firmware => "/lib/firmware/nvidia",
                    NvidiaFileCategory::Config => "/usr/share",
                    NvidiaFileCategory::Xorg => "/usr/lib/xorg/modules",
                    NvidiaFileCategory::Vdpau => "/usr/lib/vdpau",
                    NvidiaFileCategory::Gbm => "/usr/lib/gbm",
                    NvidiaFileCategory::Other => "",
                };
                let label = cat.label().to_string();
                (cat, TextBox::new(&label, default_dest.to_string()))
            })
            .collect();

        Self {
            gpu_device: RadioGroup::new("GPU Device", device_options, 0),
            mode: RadioGroup::new(
                "Passthrough Mode",
                vec!["Mirror Host".to_string(), "Categorized".to_string()],
                0,
            ),
            dest_inputs,
            inject_env: Checkbox::new("Inject environment (/etc/environment)", false),
            btn_ok: Button::new("OK", AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new(
                "Cancel",
                AppMessage::Wizard(WizardMessage::DialogCancel),
            ),
            focus: FocusTracker::new(),
            scroll_offset: 0,
            on_submit: Box::new(on_submit),
        }
    }

    pub fn with_profile(
        mut self,
        gpu_device: &str,
        mode: &NvidiaPassthroughMode,
        saved_dests: &[(NvidiaFileCategory, String)],
        inject_env: bool,
        active_categories: Vec<NvidiaFileCategory>,
    ) -> Self {
        if let Some(idx) = self
            .gpu_device
            .options()
            .iter()
            .position(|o| o == gpu_device)
        {
            self.gpu_device.set_selected_idx(idx);
        }
        self.mode.set_selected_idx(match mode {
            NvidiaPassthroughMode::Mirror => 0,
            NvidiaPassthroughMode::Categorized => 1,
        });

        let mut display = NvidiaFileCategory::all_static();
        for cat in active_categories {
            if !display.contains(&cat) {
                display.push(cat);
            }
        }
        display.sort_by_key(|c| format!("{:?}", c));

        self.dest_inputs = display
            .into_iter()
            .map(|cat| {
                let default_dest = match cat {
                    NvidiaFileCategory::Lib64 => "/usr/lib",
                    NvidiaFileCategory::Lib32 => "/usr/lib32",
                    NvidiaFileCategory::Bin => "/usr/bin",
                    NvidiaFileCategory::Firmware => "/lib/firmware/nvidia",
                    NvidiaFileCategory::Config => "/usr/share",
                    NvidiaFileCategory::Xorg => "/usr/lib/xorg/modules",
                    NvidiaFileCategory::Vdpau => "/usr/lib/vdpau",
                    NvidiaFileCategory::Gbm => "/usr/lib/gbm",
                    NvidiaFileCategory::Other => "",
                };
                let label = cat.label().to_string();
                let dest = saved_dests
                    .iter()
                    .find(|(c, _)| c == &cat)
                    .map(|(_, d)| d.clone())
                    .unwrap_or(default_dest.to_string());
                (cat, TextBox::new(&label, dest))
            })
            .collect();

        self.inject_env = Checkbox::new("Inject environment (/etc/environment)", inject_env);
        self.update_focus();
        self
    }

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

    fn try_submit(&mut self) -> Option<AppMessage> {
        let gpu_device = self
            .gpu_device
            .options()
            .get(self.gpu_device.selected_idx())
            .cloned()
            .unwrap_or_else(|| "all".to_string());
        let mode = match self.mode.selected_idx() {
            0 => NvidiaPassthroughMode::Mirror,
            _ => NvidiaPassthroughMode::Categorized,
        };
        if matches!(mode, NvidiaPassthroughMode::Categorized) {
            for (_, tb) in &self.dest_inputs {
                let text = tb.value().trim().to_string();
                if text.is_empty() || !text.starts_with('/') {
                    return None;
                }
            }
        }
        let category_destinations: Vec<_> = self
            .dest_inputs
            .iter()
            .map(|(cat, tb)| (cat.clone(), tb.value().trim().to_string()))
            .collect();
        let inject_env = self.inject_env.checked();
        Some((self.on_submit)(NvidiaConfigResult {
            gpu_device,
            mode,
            category_destinations,
            inject_env,
        }))
    }
}

impl Component for NvidiaConfigDialog {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let dialog_area = crate::ui::centered_rect(50, 70, area);
        f.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" NVIDIA Passthrough ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(dialog_area);
        f.render_widget(block, dialog_area);

        let mode_idx = self.mode.selected_idx();
        let dest_count = if mode_idx == 1 {
            self.dest_inputs.len()
        } else {
            0
        };

        // Layout: gpu(3) | mode(3) | dest_area(Min(0)) | inject_env(3) | buttons(3)
        // Matches active_comps! focus order: gpu -> mode -> [dest_inputs...] -> inject -> buttons
        let constraints = vec![
            Constraint::Length(3), // gpu_device
            Constraint::Length(3), // mode
            Constraint::Min(0),    // scrollable dest area / spacer
            Constraint::Length(3), // inject_env
            Constraint::Length(3), // buttons
        ];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        self.gpu_device.render(f, chunks[0]);
        self.mode.render(f, chunks[1]);

        // Scrollable dest_inputs (chunks[2])
        if dest_count > 0 {
            let dest_area = chunks[2];
            let total_height = dest_count as u16 * 3;
            let max_scroll = total_height.saturating_sub(dest_area.height);

            // Auto-scroll to keep focused dest_input visible
            // dest_inputs occupy focus indices 2..2+dest_count
            let active_idx = self.focus.active_idx;
            if active_idx >= 2 && active_idx < 2 + dest_count {
                let focused_dest = (active_idx - 2) as u16;
                let focused_y = focused_dest * 3;
                if focused_y < self.scroll_offset {
                    self.scroll_offset = focused_y;
                } else if focused_y + 3 > self.scroll_offset + dest_area.height {
                    self.scroll_offset = (focused_y + 3).saturating_sub(dest_area.height);
                }
                self.scroll_offset = self.scroll_offset.min(max_scroll);
            }

            let inner_width = if max_scroll > 0 {
                dest_area.width.saturating_sub(1)
            } else {
                dest_area.width
            };

            let vis_top = self.scroll_offset;
            let vis_bottom = self.scroll_offset + dest_area.height;

            for (i, (_, tb)) in self.dest_inputs.iter_mut().enumerate() {
                let item_top = i as u16 * 3;
                let item_bottom = item_top + 3;

                // Intersection of item bounds with visible viewport
                let visible_top = item_top.max(vis_top);
                let visible_bottom = item_bottom.min(vis_bottom);
                if visible_top >= visible_bottom {
                    continue;
                }

                let screen_y = dest_area.y + visible_top - vis_top;
                let draw_height = visible_bottom - visible_top;
                let item_rect = Rect::new(dest_area.x, screen_y, inner_width, draw_height);
                tb.render(f, item_rect);
            }

            if max_scroll > 0 {
                use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
                let mut state = ScrollbarState::new(max_scroll as usize)
                    .position(self.scroll_offset as usize);
                let scrollbar = Scrollbar::default()
                    .orientation(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("▲"))
                    .end_symbol(Some("▼"));
                f.render_stateful_widget(
                    scrollbar,
                    Rect {
                        x: dest_area.x + dest_area.width - 1,
                        y: dest_area.y,
                        width: 1,
                        height: dest_area.height,
                    },
                    &mut state,
                );
            }
        }

        // inject_env always visible at chunks[3], buttons at chunks[4]
        self.inject_env.render(f, chunks[3]);

        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[4]);

        let ok_area = crate::ui::centered_rect(60, 100, btn_chunks[0]);
        let cancel_area = crate::ui::centered_rect(60, 100, btn_chunks[1]);
        self.btn_ok.render(f, ok_area);
        self.btn_cancel.render(f, cancel_area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
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
            self.gpu_device.set_focus(false);
            self.mode.set_focus(false);
            for (_, tb) in &mut self.dest_inputs {
                tb.set_focus(false);
            }
            self.inject_env.set_focus(false);
            self.btn_ok.set_focus(false);
            self.btn_cancel.set_focus(false);
        }
    }

    fn is_focused(&self) -> bool {
        self.gpu_device.is_focused()
            || self.mode.is_focused()
            || self.inject_env.is_focused()
            || self.dest_inputs.iter().any(|(_, tb)| tb.is_focused())
            || self.btn_ok.is_focused()
            || self.btn_cancel.is_focused()
    }
}
