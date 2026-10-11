//! Pure presentation shared by detail-panel pages.
//!
//! Pages select and compose content; these components turn supplied content
//! into styled, wrapped lines. Target selection, backend requests and scroll
//! state belong to the panel coordinator and are not accessed here.

pub(crate) mod configuration;
mod execution;
pub(crate) mod layout;
pub(crate) mod property_document;
pub(crate) mod style;
pub(crate) mod viewport;

use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::tui::foundation::theme;
pub(crate) use execution::command_summary;

pub(crate) fn empty_block() -> Paragraph<'static> {
    Paragraph::new(empty_lines())
}

pub(crate) fn empty_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No container selected.",
            Style::default().fg(theme::theme().text_secondary),
        )),
    ]
}
