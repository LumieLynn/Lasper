//! Top-level modal layer arbitration.
//!
//! Keyboard and mouse dispatch must agree about which overlay is on top.  A
//! single priority function keeps an accidentally stale flag from allowing
//! input to leak into the workspace underneath a modal.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalLayer {
    ResourceActionMenu,
    Leader,
    Wizard,
    Help,
    QuitConfirmation,
    DeleteConfirmation,
    Dialog,
}
