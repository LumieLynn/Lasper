#[derive(Debug, Clone, PartialEq)]
pub enum AppMessage {
    StepNext,
    StepPrev,
    Submit,
    Close,

    // Source State
    SourceUrlUpdated(String),
    SourceKindUpdated(crate::ui::wizard::context::SourceKind),
    SourceMirrorUpdated(String),
    SourceSuiteUpdated(String),
    SourcePkgsUpdated(String),
    SourceDiskPathUpdated(String),
    SourceCloneIdxUpdated(usize),

    // Basic State
    BaseNameUpdated(String),
    BaseHostnameUpdated(String),

    // Storage State
    StorageTypeUpdated(usize),
    StorageSizeUpdated(String),
    StorageFsUpdated(String),
    StoragePartitionUpdated(bool),

    // User State
    RootPasswordUpdated(String),
    UserAdded(crate::nspawn::models::CreateUser),
    UserRemoved(usize),

    // Network State
    NetworkModeUpdated(usize),
    NetworkBridgeUpdated(String),
    PortForwardAdded(crate::nspawn::models::PortForward),
    PortForwardRemoved(usize),
    DialogSubmit,
    DialogCancel,

    // Passthrough State
    GenericGpuUpdated(bool),
    WaylandSocketUpdated(bool),
    NvidiaGpuUpdated(bool),
    BindMountAdded(crate::nspawn::models::BindMount),
    BindMountRemoved(usize),

    // Main UI — container list navigation
    ListNext,
    ListPrev,
    DetailPaneChanged(crate::app::DetailPane),

    // Backend communication
    BackendResult(BackendResponse),
}

#[derive(Debug, Clone)]
pub enum BackendCommand {
    SubmitConfig(Box<crate::ui::wizard::context::WizardContext>),
    ValidateBridge(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackendResponse {
    ValidationSuccess,
    ValidationError(String),
    DeployStarted,
    DeployFailed(String),
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

    pub fn next(&mut self, components: &[&dyn Component]) {
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

    pub fn prev(&mut self, components: &[&dyn Component]) {
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
