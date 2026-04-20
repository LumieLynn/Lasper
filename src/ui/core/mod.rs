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

    // Macro-events for atomic data changes
    UserAdded(crate::nspawn::models::CreateUser),
    UserUpdated(usize, crate::nspawn::models::CreateUser),
    UserRemoved(usize),
    PortForwardAdded(crate::nspawn::models::PortForward),
    PortForwardRemoved(usize),
    BindMountAdded(crate::nspawn::models::BindMount),
    BindMountRemoved(usize),
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
macro_rules! focus_action {
    // Process list with custom initialization (not strictly necessary but flexible)
    ($self:ident, $action:ident, { $($init:stmt)* }, $comps:expr $(, $args:expr)*) => {{
        $($init)*
        let mut comps: Vec<&mut dyn Component> = $comps;
        $self.focus.$action(&mut comps $(, $args)*)
    }};
    // Process standard list
    ($self:ident, $action:ident, $comps:expr $(, $args:expr)*) => {{
        let mut comps: Vec<&mut dyn Component> = $comps;
        $self.focus.$action(&mut comps $(, $args)*)
    }};
}
