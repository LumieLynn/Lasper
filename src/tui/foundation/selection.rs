//! Selection navigation shares scroll actions without confusing an item's
//! focus with the viewport's row offset.

use super::scrolling::ScrollAction;
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};

pub(crate) fn navigate_selection(
    action: ScrollAction,
    selected: Option<usize>,
    length: usize,
    viewport_rows: usize,
    wrap_lines: bool,
    enabled: impl Fn(usize) -> bool,
) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let current = selected.unwrap_or(0).min(length - 1);
    let backwards = matches!(
        action,
        ScrollAction::Up(_) | ScrollAction::PageUp | ScrollAction::First
    );
    if wrap_lines && matches!(action, ScrollAction::Up(1) | ScrollAction::Down(1)) {
        for step in 1..=length {
            let index = if backwards {
                (current + length - step) % length
            } else {
                (current + step) % length
            };
            if enabled(index) {
                return Some(index);
            }
        }
        return None;
    }
    let destination = action.destination(current, length - 1, viewport_rows);
    if backwards {
        (0..=destination)
            .rev()
            .find(|&index| enabled(index))
            .or_else(|| (destination..length).find(|&index| enabled(index)))
    } else {
        (destination..length)
            .find(|&index| enabled(index))
            .or_else(|| (0..=destination).rev().find(|&index| enabled(index)))
    }
}

pub(crate) fn register_selection_hints(footer: &mut FooterRegistry) {
    footer.register(FooterHint::new("↑/↓ / j/k", "select"));
    footer.register(FooterHint::new("PgUp/PgDn", "page").secondary());
    footer.register(FooterHint::new("Home/End", "first/last").secondary());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_skips_disabled_items_and_wraps_only_line_actions() {
        let enabled = |index| matches!(index, 1 | 3 | 4);
        assert_eq!(
            navigate_selection(ScrollAction::First, Some(4), 6, 3, true, enabled),
            Some(1)
        );
        assert_eq!(
            navigate_selection(ScrollAction::Last, Some(1), 6, 3, true, enabled),
            Some(4)
        );
        assert_eq!(
            navigate_selection(ScrollAction::Down(1), Some(4), 6, 3, true, enabled),
            Some(1)
        );
        assert_eq!(
            navigate_selection(ScrollAction::Down(1), Some(4), 6, 3, false, enabled),
            Some(4)
        );
        assert_eq!(
            navigate_selection(ScrollAction::PageDown, Some(1), 6, 3, true, enabled),
            Some(3)
        );
        assert_eq!(
            navigate_selection(ScrollAction::First, Some(1), 6, 3, true, |_| false),
            None
        );
    }
}
