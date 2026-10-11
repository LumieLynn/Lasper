//! Application foundations: owners, service composition, input priority and
//! frame/event scheduling. Concrete feature coordination lives in integrations.

mod driver;
pub(crate) mod events;
mod input;
mod launcher;
pub(in crate::tui::app) mod layout;
mod lifecycle;
pub(in crate::tui::app) mod overlay;
mod presentation;
mod services;
mod state;

pub(crate) use launcher::run;
#[cfg(test)]
pub use overlay::ModalLayer;
pub(in crate::tui::app) use overlay::OverlayCommand;
pub use state::App;
#[cfg(test)]
pub use state::AppUi;
