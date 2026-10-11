//! Own workspace proportions and shared geometry for drawing and hit-testing.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::tui::features::workspace::WorkspaceFocus;

const CONTAINER_LIST_PCT_MIN: u16 = 15;
const CONTAINER_LIST_PCT_MAX: u16 = 50;
const DETAIL_PCT_MIN: u16 = 30;
const DETAIL_PCT_MAX: u16 = 85;
const LEFT_MACHINES_PCT_MIN: u16 = 20;
const LEFT_MACHINES_PCT_MAX: u16 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkspacePanel {
    Machines,
    Images,
    Inspector,
    Terminal,
}

pub(super) enum ResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Panel areas from the latest frame. Input routing uses these same bounds,
/// so the visual layout and pointer behavior cannot drift apart.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WorkspaceAreas {
    pub(crate) machines: Rect,
    pub(crate) images: Rect,
    pub(crate) detail: Option<Rect>,
    pub(crate) terminal: Option<Rect>,
}

/// The workspace owns its proportions and the areas from its latest frame.
/// Input changes this object; drawing updates its shared hit-test geometry.
pub(super) struct WorkspaceLayout {
    pub(super) areas: WorkspaceAreas,
    machine_list_pct: u16,
    left_machines_pct: u16,
    detail_pct: u16,
    right_pane_height: u16,
    resizing: bool,
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self {
            areas: WorkspaceAreas::default(),
            machine_list_pct: 30,
            left_machines_pct: 50,
            detail_pct: 45,
            right_pane_height: 10,
            resizing: false,
        }
    }
}

impl WorkspaceLayout {
    pub(super) fn is_resizing(&self) -> bool {
        self.resizing
    }

    pub(super) fn toggle_resize(&mut self) {
        self.resizing = !self.resizing;
    }

    pub(super) fn stop_resize(&mut self) {
        self.resizing = false;
    }

    pub(super) fn resize(
        &mut self,
        direction: ResizeDirection,
        focus: WorkspaceFocus,
        terminal_showing: bool,
    ) {
        const STEP: u16 = 5;
        let (value, increase, min, max) = match direction {
            ResizeDirection::Left | ResizeDirection::Right => (
                &mut self.machine_list_pct,
                matches!(direction, ResizeDirection::Right),
                CONTAINER_LIST_PCT_MIN,
                CONTAINER_LIST_PCT_MAX,
            ),
            ResizeDirection::Up | ResizeDirection::Down
                if focus.is_machine_list() || focus.is_image_list() =>
            {
                (
                    &mut self.left_machines_pct,
                    matches!(direction, ResizeDirection::Down),
                    LEFT_MACHINES_PCT_MIN,
                    LEFT_MACHINES_PCT_MAX,
                )
            }
            ResizeDirection::Up | ResizeDirection::Down if terminal_showing => (
                &mut self.detail_pct,
                matches!(direction, ResizeDirection::Down),
                DETAIL_PCT_MIN,
                DETAIL_PCT_MAX,
            ),
            _ => return,
        };
        *value = if increase {
            value.saturating_add(STEP).min(max)
        } else {
            value.saturating_sub(STEP).max(min)
        };
    }

    pub(super) fn terminal_rows(&self) -> u16 {
        self.right_pane_height.max(10)
    }

    pub(super) fn panel_at(&self, column: u16, row: u16) -> Option<WorkspacePanel> {
        let areas = self.areas;
        if in_rect(column, row, areas.machines) {
            Some(WorkspacePanel::Machines)
        } else if in_rect(column, row, areas.images) {
            Some(WorkspacePanel::Images)
        } else if areas.detail.is_some_and(|area| in_rect(column, row, area)) {
            Some(WorkspacePanel::Inspector)
        } else if areas
            .terminal
            .is_some_and(|area| in_rect(column, row, area))
        {
            Some(WorkspacePanel::Terminal)
        } else {
            None
        }
    }

    pub(super) fn update(
        &mut self,
        area: Rect,
        terminal_showing: bool,
        terminal_maximized: bool,
    ) -> WorkspaceAreas {
        let maximized = terminal_showing && terminal_maximized;
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(self.machine_list_pct),
                Constraint::Percentage(100u16.saturating_sub(self.machine_list_pct)),
            ])
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(self.left_machines_pct),
                Constraint::Percentage(100u16.saturating_sub(self.left_machines_pct)),
            ])
            .split(columns[0]);
        let right = if terminal_showing && !maximized {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(self.detail_pct),
                    Constraint::Percentage(100u16.saturating_sub(self.detail_pct)),
                ])
                .split(columns[1])
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(100)])
                .split(columns[1])
        };

        let detail = (!maximized).then_some(right[0]);
        let terminal = terminal_showing.then(|| if maximized { right[0] } else { right[1] });
        let areas = WorkspaceAreas {
            machines: left[0],
            images: left[1],
            detail,
            terminal,
        };

        self.areas = areas;
        self.right_pane_height = columns[1].height.saturating_sub(2);
        areas
    }
}

fn in_rect(column: u16, row: u16, area: Rect) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_layout_partitions_columns_and_stacks_lists() {
        let mut workspace = WorkspaceLayout::default();
        let area = Rect::new(2, 3, 100, 40);

        let layout = workspace.update(area, false, false);

        assert_eq!(layout.machines.x, area.x);
        assert_eq!(layout.images.x, area.x);
        assert_eq!(layout.images.y + layout.images.height, area.y + area.height);
        assert_eq!(layout.detail.unwrap().x, area.x + 30);
        assert_eq!(layout.machines.height + layout.images.height, area.height);
        assert_eq!(layout.terminal, None);
        assert_eq!(workspace.areas.machines, layout.machines);
    }

    #[test]
    fn workspace_layout_tracks_split_and_maximized_terminal_bounds() {
        let mut workspace = WorkspaceLayout::default();
        let area = Rect::new(0, 0, 100, 40);

        let split = workspace.update(area, true, false);
        let terminal = split.terminal.expect("visible terminal has an area");
        let detail = split.detail.expect("split terminal keeps the inspector");
        assert_eq!(detail.x, terminal.x);
        assert_eq!(detail.y, area.y);
        assert_eq!(terminal.y + terminal.height, area.y + area.height);
        assert_eq!(workspace.areas.terminal, Some(terminal));

        let maximized = workspace.update(area, true, true);
        assert_eq!(maximized.detail, None);
        assert_eq!(maximized.terminal.unwrap().height, area.height);

        let closed = workspace.update(area, false, true);
        assert_eq!(closed.terminal, None);
        assert_eq!(closed.detail.unwrap().height, area.height);
    }

    #[test]
    fn workspace_layout_keeps_panels_inside_a_small_terminal() {
        let mut workspace = WorkspaceLayout::default();
        let area = Rect::new(0, 0, 20, 12);

        let layout = workspace.update(area, true, false);
        let panels = [
            layout.machines,
            layout.images,
            layout.detail.unwrap(),
            layout.terminal.unwrap(),
        ];

        assert!(panels
            .iter()
            .all(|panel| panel.width > 0 && panel.height > 0));
        assert!(panels.iter().all(|panel| {
            panel.x >= area.x
                && panel.y >= area.y
                && panel.x + panel.width <= area.x + area.width
                && panel.y + panel.height <= area.y + area.height
        }));
    }

    #[test]
    fn resize_rules_keep_workspace_percentages_in_bounds() {
        let mut layout = WorkspaceLayout::default();
        for _ in 0..20 {
            layout.resize(ResizeDirection::Left, WorkspaceFocus::Machines, false);
            layout.resize(ResizeDirection::Up, WorkspaceFocus::Machines, false);
        }
        assert_eq!(layout.machine_list_pct, CONTAINER_LIST_PCT_MIN);
        assert_eq!(layout.left_machines_pct, LEFT_MACHINES_PCT_MIN);
    }

    #[test]
    fn terminal_is_taller_than_detail_by_default() {
        let layout = WorkspaceLayout::default();
        assert!(layout.detail_pct < 50);
        assert_eq!(layout.detail_pct, 45);
    }
}
