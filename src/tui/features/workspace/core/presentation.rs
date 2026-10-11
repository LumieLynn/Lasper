//! Draw the panel composition owned by the workspace manager.

use crate::tui::features::workspace::terminal_panel::{TerminalManager, TerminalPanel};
use crate::tui::features::workspace::Workspace;
use crate::tui::foundation::term::CursorStyle;
use ratatui::{layout::Rect, Frame};
use std::collections::HashSet;

impl Workspace {
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        terminal: &mut TerminalManager,
        removing_images: &HashSet<String>,
        overlay_visible: bool,
    ) -> Option<CursorStyle> {
        let focus = self.focus();
        let resize_mode = self.layout.is_resizing();
        let layout = self
            .layout
            .update(area, terminal.is_showing(), terminal.maximized);
        let cursor_style = layout.terminal.and_then(|terminal_area| {
            TerminalPanel.render(
                frame,
                terminal_area,
                terminal,
                focus.is_terminal() && !overlay_visible,
                resize_mode,
            )
        });
        self.resources.render_machines(
            frame,
            layout.machines,
            focus.is_machine_list(),
            resize_mode,
        );
        self.resources.render_images(
            frame,
            layout.images,
            removing_images,
            focus.is_image_list(),
            resize_mode,
        );
        if let Some(detail_area) = layout.detail {
            self.inspector.render(
                frame,
                detail_area,
                &self.resources.catalog,
                focus.is_inspector(),
                resize_mode,
            );
        }
        cursor_style
    }
}
