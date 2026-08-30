//! Shared title-tab hit testing for panel views.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TitleTabHitbox<T> {
    pub value: T,
    pub area: Rect,
}

/// Lay out clickable spans exactly where a bordered block renders one title line.
/// Tabs are separated visually, but the separator itself is not clickable.
pub(crate) fn bordered_title_tab_hitboxes<T: Copy>(
    area: Rect,
    alignment: Alignment,
    tabs: &[(T, usize)],
    separator_width: usize,
) -> Vec<TitleTabHitbox<T>> {
    let available = usize::from(area.width.saturating_sub(2));
    if available == 0 || tabs.is_empty() {
        return Vec::new();
    }

    let content_width = tabs
        .iter()
        .fold(0usize, |width, (_, tab_width)| {
            width.saturating_add(*tab_width)
        })
        .saturating_add(separator_width.saturating_mul(tabs.len().saturating_sub(1)));
    let (skip_width, indent) = if content_width <= available {
        let free = available - content_width;
        let indent = match alignment {
            Alignment::Left => 0,
            Alignment::Center => free / 2,
            Alignment::Right => free,
        };
        (0, indent)
    } else {
        let overflow = content_width - available;
        let skip = match alignment {
            Alignment::Left => 0,
            Alignment::Center => overflow / 2,
            Alignment::Right => overflow,
        };
        (skip, 0)
    };

    let visible_end = skip_width.saturating_add(available);
    let title_x = area.x.saturating_add(1);
    let mut offset = 0usize;
    let mut hitboxes = Vec::with_capacity(tabs.len());
    for (index, (value, tab_width)) in tabs.iter().enumerate() {
        let tab_start = offset;
        let tab_end = tab_start.saturating_add(*tab_width);
        let visible_start = tab_start.max(skip_width);
        let visible_tab_end = tab_end.min(visible_end);
        if visible_start < visible_tab_end {
            let relative_x = indent.saturating_add(visible_start - skip_width);
            let x = title_x.saturating_add(u16::try_from(relative_x).unwrap_or(u16::MAX));
            let width = u16::try_from(visible_tab_end - visible_start).unwrap_or(u16::MAX);
            hitboxes.push(TitleTabHitbox {
                value: *value,
                area: Rect::new(x, area.y, width, 1),
            });
        }
        offset = tab_end;
        if index + 1 < tabs.len() {
            offset = offset.saturating_add(separator_width);
        }
    }
    hitboxes
}

pub(crate) fn clicked_title_tab<T: Copy>(
    hitboxes: &[TitleTabHitbox<T>],
    mouse: MouseEvent,
) -> Option<T> {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    hitboxes
        .iter()
        .find(|hitbox| {
            mouse.column >= hitbox.area.x
                && mouse.column < hitbox.area.x.saturating_add(hitbox.area.width)
                && mouse.row == hitbox.area.y
        })
        .map(|hitbox| hitbox.value)
}

pub(crate) fn clip_title_tabs_before<T>(
    hitboxes: &mut Vec<TitleTabHitbox<T>>,
    first_visible_column: u16,
) {
    for hitbox in hitboxes.iter_mut() {
        let end = hitbox.area.x.saturating_add(hitbox.area.width);
        if hitbox.area.x < first_visible_column {
            hitbox.area.x = first_visible_column;
            hitbox.area.width = end.saturating_sub(first_visible_column);
        }
    }
    hitboxes.retain(|hitbox| hitbox.area.width > 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn title_tabs_follow_alignment_and_leave_separators_inert() {
        let area = Rect::new(10, 4, 22, 8);
        let tabs = bordered_title_tab_hitboxes(area, Alignment::Right, &[('a', 5), ('b', 8)], 1);

        assert_eq!(tabs[0].area, Rect::new(17, 4, 5, 1));
        assert_eq!(tabs[1].area, Rect::new(23, 4, 8, 1));
        assert_eq!(clicked_title_tab(&tabs, click(18, 4)), Some('a'));
        assert_eq!(clicked_title_tab(&tabs, click(22, 4)), None);
        assert_eq!(clicked_title_tab(&tabs, click(25, 4)), Some('b'));
    }

    #[test]
    fn right_aligned_overflow_keeps_only_visible_tab_fragments_clickable() {
        let tabs = bordered_title_tab_hitboxes(
            Rect::new(0, 0, 10, 4),
            Alignment::Right,
            &[('a', 5), ('b', 5)],
            1,
        );

        assert_eq!(tabs[0].area, Rect::new(1, 0, 2, 1));
        assert_eq!(tabs[1].area, Rect::new(4, 0, 5, 1));
    }

    #[test]
    fn clipping_removes_cells_obscured_by_an_earlier_title() {
        let mut tabs = vec![
            TitleTabHitbox {
                value: 'a',
                area: Rect::new(3, 0, 4, 1),
            },
            TitleTabHitbox {
                value: 'b',
                area: Rect::new(8, 0, 4, 1),
            },
        ];

        clip_title_tabs_before(&mut tabs, 7);

        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].value, 'b');
    }
}
