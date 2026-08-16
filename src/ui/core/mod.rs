#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AppMessage {
    Wizard(WizardMessage),
    Container(ContainerMessage),
    List(ListMessage),
    Backend(crate::nspawn::ops::BackendResponse),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WizardMessage {
    Submit,
    Close,
    DialogSubmit,
    DialogCancel,
    AcceptUnsafeRemoteTar,
    DeclineUnsafeRemoteTar,

    // Dialog open requests (add = blank, edit = pre-filled)
    OpenUserDialog,
    OpenUserEditDialog(usize, crate::nspawn::models::CreateUser),
    OpenPortDialog,
    OpenPortEditDialog(usize, crate::nspawn::models::PortForward),
    OpenBindDialog,
    OpenBindEditDialog(usize, crate::nspawn::models::BindMount),
    OpenNvidiaConfigDialog,
    NvidiaConfigSaved(crate::ui::widgets::dialogs::nvidia_config::NvidiaConfigResult),
    OpenUnclassifiedEditDialog(usize, crate::ui::wizard::core::context::UnclassifiedFile),

    // Macro-events for atomic data changes
    UserAdded(crate::nspawn::models::CreateUser),
    UserUpdated(usize, crate::nspawn::models::CreateUser),
    UserRemoved(usize),
    PortForwardAdded(crate::nspawn::models::PortForward),
    PortForwardUpdated(usize, crate::nspawn::models::PortForward),
    PortForwardRemoved(usize),
    BindMountAdded(crate::nspawn::models::BindMount),
    BindMountUpdated(usize, crate::nspawn::models::BindMount),
    BindMountRemoved(usize),
    UnclassifiedFileUpdated(usize, crate::ui::wizard::core::context::UnclassifiedFile),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContainerMessage {
    PaneChanged(crate::ui::views::detail_panel::DetailPane),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ListMessage {
    Next,
    Prev,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum EventResult {
    Ignored,             // Not handled, bubble up
    Consumed,            // Handled, no further action needed
    FocusNext,           // Request parent to move focus forward
    FocusPrev,           // Request parent to move focus backward
    Message(AppMessage), // Handled, produced a business message
}

pub trait Component {
    fn render(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect);
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult;

    /// Handle a mouse event in the component's last rendered area.  Widgets
    /// that do not expose mouse interaction keep the default no-op; parents
    /// still consume the event when they are modal so it cannot bubble into
    /// the background UI.
    fn handle_mouse(&mut self, _mouse: crossterm::event::MouseEvent) -> EventResult {
        EventResult::Ignored
    }

    fn set_focus(&mut self, _focused: bool) {}
    fn is_focused(&self) -> bool {
        false
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn is_focusable(&self) -> bool {
        self.is_enabled()
    }
    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

pub struct FocusTracker {
    pub active_idx: usize,
}

impl FocusTracker {
    pub fn new() -> Self {
        Self { active_idx: 0 }
    }

    pub fn next<T: std::ops::Deref<Target = dyn Component>>(&mut self, components: &[T]) {
        if components.is_empty() {
            return;
        }
        let len = components.len();
        let start = self.active_idx;
        loop {
            self.active_idx = (self.active_idx + 1) % len;
            if components[self.active_idx].is_focusable() || self.active_idx == start {
                break;
            }
        }
    }

    pub fn prev<T: std::ops::Deref<Target = dyn Component>>(&mut self, components: &[T]) {
        if components.is_empty() {
            return;
        }
        let len = components.len();
        let start = self.active_idx;
        loop {
            self.active_idx = (self.active_idx + len - 1) % len;
            if components[self.active_idx].is_focusable() || self.active_idx == start {
                break;
            }
        }
    }

    pub fn update_focus(&self, components: &mut [&mut dyn Component], parent_focused: bool) {
        for (i, child) in components.iter_mut().enumerate() {
            child.set_focus(parent_focused && i == self.active_idx && child.is_focusable());
        }
    }
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
                // Advance past disabled components so focus lands on the first focusable one.
                if len > 0 {
                    let start = self.focus.active_idx;
                    while !comps[self.focus.active_idx].is_focusable() {
                        self.focus.active_idx = (self.focus.active_idx + 1) % len;
                        if self.focus.active_idx == start {
                            break;
                        }
                    }
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
macro_rules! wizard_set_focus {
    ($self:ident, $focused:ident, $comps_macro:ident) => {{
        if $focused {
            $self.update_focus();
        } else {
            for comp in $comps_macro!($self) {
                comp.set_focus(false);
            }
        }
    }};
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
