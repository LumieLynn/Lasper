pub mod builder;
pub mod context;
pub mod manager;
pub mod steps;

pub use self::manager::Wizard;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WizardStep {
    Source,
    CopySelect,
    Basic,
    Storage,
    User,
    Network,
    Passthrough,
    Devices,
    Review,
    Deploy,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepAction {
    None,
    Next,
    Prev,
    Close,
    CloseRefresh,
    /// Display a status message in the application status bar.
    Status(String, crate::nspawn::StatusLevel),
}

impl WizardStep {
    pub fn title(&self) -> &str {
        match self {
            WizardStep::Source => "Source Selection",
            WizardStep::CopySelect => "Select Image to Clone",
            WizardStep::Basic => "Basic Configuration",
            WizardStep::Storage => "Storage Settings",
            WizardStep::User => "User Management",
            WizardStep::Network => "Network Configuration",
            WizardStep::Passthrough => "Hardware Passthrough",
            WizardStep::Devices => "Device & Mounts",
            WizardStep::Review => "Final Review",
            WizardStep::Deploy => "Deployment Progress",
        }
    }

}

pub fn render_hint(f: &mut ratatui::Frame, area: ratatui::layout::Rect, hints: &[&str]) {
    use ratatui::{
        layout::Alignment,
        style::{Color, Style},
        widgets::Paragraph,
    };
    let text = hints.join(" | ");
    let p = Paragraph::new(text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    f.render_widget(p, area);
}
