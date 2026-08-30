use crate::application::provisioning::HostGpuDevice;
use crate::domain::provisioning::{NetworkMode, PrivateUsersMode};
use crate::tui::core::{Component, EventResult, FocusTracker};
use crate::tui::widgets::display::text_block::TextBlock;
use crate::tui::widgets::lists::checklist::Checklist;
use crate::tui::widgets::selectors::checkbox::Checkbox;
use crate::tui::widgets::selectors::radio_group::RadioGroup;
use crate::tui::wizard::draft::{PassthroughConfig, WizardDraft};
use crate::tui::wizard::steps::StepComponent;
use crate::{delegate_wizard_navigation, impl_wizard_nav, wizard_set_focus};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, widgets::ScrollbarState, Frame};

#[derive(Clone)]
enum GpuSelectionItem {
    AllDrm,
    Device(HostGpuDevice),
}

fn is_drm_gpu(gpu: &HostGpuDevice) -> bool {
    gpu.nodes.iter().any(|node| node.starts_with("/dev/dri/"))
}

fn scrollbar_state(scroll_offset: u16, scroll_max: u16, viewport_height: u16) -> ScrollbarState {
    // Ratatui models content_length as the number of valid scroll positions.
    ScrollbarState::new(usize::from(scroll_max) + 1)
        .position(usize::from(scroll_offset))
        .viewport_content_length(usize::from(viewport_height))
}

/// Single source of truth: returns (component, height, is_focusable) for every visible item.
macro_rules! layout_items {
    ($self:ident) => {{
        let is_accel = $self.graphics_acceleration.checked();
        let is_privileged = $self.privileged.checked();
        let has_gpus = !$self.gpu_list.items().is_empty();
        let gpu_count = $self.gpu_list.items().len() as u16;
        let privilege_warning_height = $self
            .privilege_warning
            .required_height($self.scroll_area.width.max(20));

        let mut items: Vec<(&mut dyn Component, u16, bool)> = Vec::new();

        if $self.hardware_scanning {
            items.push((&mut $self.scanning_indicator, 4, false));
        }

        items.push((&mut $self.graphics_acceleration, 3, true));

        if is_accel && has_gpus {
            let height = (gpu_count + 2).min(10);
            items.push((&mut $self.gpu_list, height, true));
        } else if is_accel && !has_gpus {
            items.push((&mut $self.gpu_empty, 3, false));
        }

        items.push((&mut $self.privileged, 3, true));
        items.push((&mut $self.private_users, 3, true));

        if is_privileged {
            items.push((
                &mut $self.privilege_warning,
                privilege_warning_height,
                false,
            ));
        }

        items
    }};
}

/// Extract only focusable components (for keyboard navigation).
macro_rules! active_comps {
    ($self:ident) => {{
        layout_items!($self)
            .into_iter()
            .filter(|(_, _, f)| *f)
            .map(|(c, _, _)| c)
            .collect::<Vec<&mut dyn Component>>()
    }};
}

impl_wizard_nav!(HostIntegrationStepView, active_comps);

pub struct HostIntegrationStepView {
    graphics_acceleration: Checkbox,
    gpu_list: Checklist<GpuSelectionItem>,
    gpu_all_index: Option<usize>,
    gpu_empty: TextBlock,

    privileged: Checkbox,
    private_users: RadioGroup,
    privilege_warning: TextBlock,
    scanning_indicator: TextBlock,
    focus: FocusTracker,
    scroll_offset: u16,
    scroll_area: Rect,
    scroll_max: u16,
    hardware_scanning: bool,
    private_network: bool,
    wayland_access_configured: bool,
}

impl HostIntegrationStepView {
    pub fn new(
        initial_data: &PassthroughConfig,
        nw_mode: Option<NetworkMode>,
        wayland_access_configured: bool,
        discovered_gpus: Vec<HostGpuDevice>,
        hardware_scanning: bool,
    ) -> Self {
        let has_drm_devices = discovered_gpus
            .iter()
            .flat_map(|gpu| &gpu.nodes)
            .any(|node| node.starts_with("/dev/dri/"));
        let gpu_all_index = has_drm_devices.then_some(0);
        let mut gpu_items =
            Vec::with_capacity(discovered_gpus.len() + usize::from(has_drm_devices));
        if has_drm_devices {
            gpu_items.push(GpuSelectionItem::AllDrm);
        }
        gpu_items.extend(
            discovered_gpus
                .iter()
                .cloned()
                .map(GpuSelectionItem::Device),
        );

        let mut checked_indices = Vec::new();
        for (i, gpu) in discovered_gpus.iter().enumerate() {
            if gpu
                .nodes
                .iter()
                .any(|node| initial_data.device_binds.contains(node))
            {
                checked_indices.push(i + usize::from(has_drm_devices));
            }
        }
        if initial_data.gpu_passthrough_all {
            if let Some(all_index) = gpu_all_index {
                checked_indices.push(all_index);
            }
            checked_indices.extend(gpu_items.iter().enumerate().filter_map(|(index, item)| {
                matches!(item, GpuSelectionItem::Device(gpu) if is_drm_gpu(gpu)).then_some(index)
            }));
        }

        let mut gpu_list = Checklist::new("Select Host GPU(s)", gpu_items, |item| match item {
            GpuSelectionItem::AllDrm => "All DRM devices (/dev/dri)".to_string(),
            GpuSelectionItem::Device(gpu) => format!(
                "{} ({})",
                gpu.display_name,
                gpu.nodes.first().cloned().unwrap_or_default()
            ),
        });
        gpu_list.set_checked(checked_indices);

        let warning_text = "Enabled setting: [Exec] Capability=all. This grants every Linux capability, including system and mount administration, device and raw I/O access, network administration, kernel/module controls, and process tracing. PrivateUsers and networking remain controlled by their separate settings. A compromised container may take over the host.";

        let mut view = Self {
            graphics_acceleration: Checkbox::new(
                "Hardware Graphics Acceleration",
                initial_data.graphics_acceleration,
            ),
            gpu_list,
            gpu_all_index,
            gpu_empty: TextBlock::new(
                " No GPUs Detected ",
                "No compatible GPU devices found on the host. Graphics acceleration may not work as expected.",
            ),
            privileged: Checkbox::new("Privileged Mode (NOT RECOMMENDED)", initial_data.privileged),
            private_users: RadioGroup::new(
                "PrivateUsers (User Namespace)",
                vec![
                    "Default".to_string(),
                    "pick".to_string(),
                    "managed".to_string(),
                    "yes".to_string(),
                    "no".to_string(),
                ],
                match initial_data.private_users {
                    None => 0,
                    Some(PrivateUsersMode::Pick) => 1,
                    Some(PrivateUsersMode::Managed) => 2,
                    Some(PrivateUsersMode::Yes) => 3,
                    Some(PrivateUsersMode::No) => 4,
                    _ => 0,
                },
            ),
            privilege_warning: TextBlock::new("SECURITY RISK", warning_text),
            scanning_indicator: TextBlock::new(
                " SCANNING ",
                " [~] Hardware discovery is running in the background. GPU and NVIDIA lists will populate automatically when finished...",
            ),
            focus: FocusTracker::new(),
            scroll_offset: 0,
            scroll_area: Rect::default(),
            scroll_max: 0,
            hardware_scanning,
            private_network: nw_mode.as_ref().is_some_and(NetworkMode::is_private),
            wayland_access_configured,
        };

        view.update_focus();
        view
    }

    fn gpu_all_selected(&self) -> bool {
        self.gpu_all_index
            .is_some_and(|index| self.gpu_list.checked_indices().contains(&index))
    }

    fn toggle_gpu_selection(&mut self) {
        let Some(selected) = self.gpu_list.selected_idx() else {
            return;
        };
        let mut checked = self.gpu_list.checked_indices().clone();

        if self.gpu_all_index == Some(selected) {
            if checked.contains(&selected) {
                checked.remove(&selected);
                for (index, item) in self.gpu_list.items().iter().enumerate() {
                    if matches!(item, GpuSelectionItem::Device(gpu) if is_drm_gpu(gpu)) {
                        checked.remove(&index);
                    }
                }
            } else {
                checked.insert(selected);
                checked.extend(self.gpu_list.items().iter().enumerate().filter_map(
                    |(index, item)| {
                        matches!(item, GpuSelectionItem::Device(gpu) if is_drm_gpu(gpu))
                            .then_some(index)
                    },
                ));
            }
        } else {
            let selected_is_drm = matches!(
                self.gpu_list.items().get(selected),
                Some(GpuSelectionItem::Device(gpu)) if is_drm_gpu(gpu)
            );
            if !checked.insert(selected) {
                checked.remove(&selected);
            }
            if selected_is_drm {
                if let Some(all_index) = self.gpu_all_index {
                    checked.remove(&all_index);
                }
            }
        }

        self.gpu_list.set_checked(checked.into_iter().collect());
    }

    fn selected_gpu_nodes(&self) -> Vec<String> {
        if !self.graphics_acceleration.checked() {
            return Vec::new();
        }

        let all_drm = self.gpu_all_selected();
        let mut selected_nodes = Vec::new();
        for &index in self.gpu_list.checked_indices() {
            if let Some(GpuSelectionItem::Device(gpu)) = self.gpu_list.items().get(index) {
                selected_nodes.extend(
                    gpu.nodes
                        .iter()
                        .filter(|node| !all_drm || !node.starts_with("/dev/dri/"))
                        .cloned(),
                );
            }
        }
        selected_nodes.sort();
        selected_nodes.dedup();
        selected_nodes
    }
}

impl Component for HostIntegrationStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // Manual margin(1) — shrink area to match other step views
        let area = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        self.scroll_area = area;

        let items = layout_items!(self);

        // Map focus index -> visual index for scroll-to-active
        let focus_to_visual: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, (_, _, f))| *f)
            .map(|(i, _)| i)
            .collect();

        let total_height: u16 = items.iter().map(|(_, h, _)| h).sum();

        let mut item_ys: Vec<u16> = Vec::new();
        let mut y: u16 = 0;
        for (_, h, _) in &items {
            item_ys.push(y);
            y += h;
        }

        // Scroll to keep active item visible
        let active_vis_idx = focus_to_visual
            .get(self.focus.active_idx)
            .copied()
            .unwrap_or(0);
        let active_y = item_ys[active_vis_idx];
        let active_h = items[active_vis_idx].1;

        if active_y < self.scroll_offset {
            self.scroll_offset = active_y;
        } else if active_y + active_h > self.scroll_offset + area.height {
            let target = (active_y + active_h).saturating_sub(area.height);
            self.scroll_offset = item_ys
                .iter()
                .copied()
                .find(|&y| y >= target)
                .unwrap_or(target);
        }

        let max_scroll = total_height.saturating_sub(area.height);
        self.scroll_max = max_scroll;
        self.scroll_offset = self.scroll_offset.min(max_scroll);

        let inner_width = if total_height > area.height {
            area.width.saturating_sub(1)
        } else {
            area.width
        };

        // Render visible items
        for (i, (comp, height, _)) in items.into_iter().enumerate() {
            let y = item_ys[i];
            if y + height <= self.scroll_offset || y >= self.scroll_offset + area.height {
                continue;
            }

            let screen_y = area.y + y - self.scroll_offset;
            let draw_height = height.min(area.y + area.height - screen_y);
            let draw_rect = Rect::new(area.x, screen_y, inner_width, draw_height);
            comp.render(f, draw_rect);
        }

        // Scrollbar
        if total_height > area.height {
            use ratatui::widgets::{Scrollbar, ScrollbarOrientation};
            let mut state = scrollbar_state(self.scroll_offset, max_scroll, area.height);
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));

            f.render_stateful_widget(
                scrollbar,
                Rect {
                    x: area.x + area.width - 1,
                    y: area.y,
                    width: 1,
                    height: area.height,
                },
                &mut state,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if key.code == KeyCode::Char(' ') && self.gpu_list.is_focused() {
            self.toggle_gpu_selection();
            self.update_focus();
            return EventResult::Consumed;
        }

        let res = delegate_wizard_navigation!(self, key, active_comps);

        if matches!(
            key.code,
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Left | KeyCode::Right
        ) {
            self.update_focus();
        }

        res
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        if mouse.column < self.scroll_area.x
            || mouse.column >= self.scroll_area.x.saturating_add(self.scroll_area.width)
            || mouse.row < self.scroll_area.y
            || mouse.row >= self.scroll_area.y.saturating_add(self.scroll_area.height)
        {
            return EventResult::Ignored;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                EventResult::Consumed
            }
            MouseEventKind::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(3).min(self.scroll_max);
                EventResult::Consumed
            }
            _ => EventResult::Ignored,
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        if self.private_users.selected_idx() == 2 && !self.private_network {
            return Err("PrivateUsers=managed requires a private network mode".into());
        }
        if self.private_users.selected_idx() == 2 && self.wayland_access_configured {
            return Err("Wayland access is not supported with PrivateUsers=managed".into());
        }
        Ok(())
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }
}

impl StepComponent for HostIntegrationStepView {
    fn commit_to_draft(&self, ctx: &mut WizardDraft) {
        ctx.passthrough.graphics_acceleration = self.graphics_acceleration.checked();
        ctx.passthrough.gpu_passthrough_all =
            self.graphics_acceleration.checked() && self.gpu_all_selected();
        ctx.passthrough.privileged = self.privileged.checked();
        ctx.passthrough.private_users = match self.private_users.selected_idx() {
            0 => None,
            1 => Some(PrivateUsersMode::Pick),
            2 => Some(PrivateUsersMode::Managed),
            3 => Some(PrivateUsersMode::Yes),
            4 => Some(PrivateUsersMode::No),
            _ => None,
        };

        ctx.passthrough.selected_gpu_nodes = self.selected_gpu_nodes();
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardDraft) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::TestBackend,
        buffer::Buffer,
        widgets::{Scrollbar, ScrollbarOrientation, StatefulWidget},
        Terminal,
    };

    #[test]
    fn scrollbar_thumb_uses_the_bounded_scroll_range() {
        let area = Rect::new(0, 0, 1, 8);
        let scrollbar = || {
            Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
        };

        let mut top_buffer = Buffer::empty(area);
        let mut top_state = scrollbar_state(0, 8, area.height);
        StatefulWidget::render(scrollbar(), area, &mut top_buffer, &mut top_state);
        assert_eq!(top_buffer[(0, 1)].symbol(), "█");
        assert_eq!(top_buffer[(0, 3)].symbol(), "█");
        assert_eq!(top_buffer[(0, 4)].symbol(), "║");

        let mut bottom_buffer = Buffer::empty(area);
        let mut bottom_state = scrollbar_state(8, 8, area.height);
        StatefulWidget::render(scrollbar(), area, &mut bottom_buffer, &mut bottom_state);
        assert_eq!(bottom_buffer[(0, 3)].symbol(), "║");
        assert_eq!(bottom_buffer[(0, 4)].symbol(), "█");
        assert_eq!(bottom_buffer[(0, 6)].symbol(), "█");
    }

    #[test]
    fn privileged_warning_names_the_effective_setting_and_independent_controls() {
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: Vec::new(),
            privileged: true,
            private_users: None,
            graphics_acceleration: false,
            gpu_passthrough_all: false,
            nvidia_gpu: false,
            nvidia_profile: None,
        };
        let mut view = HostIntegrationStepView::new(
            &config,
            Some(NetworkMode::Host),
            false,
            Vec::new(),
            false,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("[Exec] Capability=all"));
        assert!(rendered.contains("PrivateUsers and networking remain controlled"));
    }

    #[test]
    fn all_drm_list_item_selects_drm_devices_and_keeps_individual_control() {
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
        let config = PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: vec!["/dev/mali".into()],
            privileged: false,
            private_users: None,
            graphics_acceleration: true,
            gpu_passthrough_all: false,
            nvidia_gpu: false,
            nvidia_profile: None,
        };
        let first_gpu = HostGpuDevice {
            display_name: "First DRM GPU".into(),
            driver_type: "DRM/KMS".into(),
            nodes: vec!["/dev/dri/card0".into(), "/dev/dri/renderD128".into()],
        };
        let second_gpu = HostGpuDevice {
            display_name: "Second DRM GPU".into(),
            driver_type: "DRM/KMS".into(),
            nodes: vec!["/dev/dri/card1".into(), "/dev/dri/renderD129".into()],
        };
        let legacy_gpu = HostGpuDevice {
            display_name: "Legacy Mali GPU".into(),
            driver_type: "Mali/Proprietary".into(),
            nodes: vec!["/dev/mali".into()],
        };
        let gpus = vec![first_gpu, second_gpu, legacy_gpu];
        let mut view = HostIntegrationStepView::new(
            &config,
            Some(NetworkMode::Host),
            false,
            gpus.clone(),
            false,
        );

        assert!(!view.gpu_all_selected());
        assert_eq!(view.gpu_list.checked_indices().len(), 1);
        view.focus.active_idx = 1;
        view.update_focus();
        assert_eq!(view.gpu_list.selected_idx(), Some(0));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(view.gpu_all_selected());
        assert_eq!(view.gpu_list.checked_indices().len(), 4);
        assert!(view.gpu_list.is_enabled());
        assert_eq!(view.selected_gpu_nodes(), vec!["/dev/mali"]);

        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("All DRM devices (/dev/dri)"));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert_eq!(view.gpu_list.selected_idx(), Some(1));
        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(!view.gpu_all_selected());
        assert!(!view.gpu_list.checked_indices().contains(&1));
        assert!(view.gpu_list.checked_indices().contains(&2));
        assert!(view.gpu_list.checked_indices().contains(&3));
        assert_eq!(
            view.selected_gpu_nodes(),
            vec!["/dev/dri/card1", "/dev/dri/renderD129", "/dev/mali"]
        );

        assert_eq!(
            view.handle_key(KeyEvent::new(
                KeyCode::Char(' '),
                crossterm::event::KeyModifiers::NONE,
            )),
            EventResult::Consumed
        );
        assert!(!view.gpu_all_selected());
        assert_eq!(view.gpu_list.checked_indices().len(), 3);

        let mut restored_config = config;
        restored_config.gpu_passthrough_all = true;
        let restored_view = HostIntegrationStepView::new(
            &restored_config,
            Some(NetworkMode::Host),
            false,
            gpus,
            false,
        );
        assert!(restored_view.gpu_all_selected());
        assert_eq!(restored_view.gpu_list.checked_indices().len(), 4);
        assert_eq!(restored_view.selected_gpu_nodes(), vec!["/dev/mali"]);
    }
}
