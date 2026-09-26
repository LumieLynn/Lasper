//! Shared title-tab hit testing for panel views.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TitleTabHitbox<T> {
    pub value: T,
    pub area: Rect,
}

pub(crate) struct TitleTabLayout<T> {
    pub(crate) line: Line<'static>,
    pub(crate) hitboxes: Vec<TitleTabHitbox<T>>,
}

/// Keeps a display-column window over a title's complete text stream. The
/// window moves only when the active tab leaves it, and resets when all text
/// fits. Hidden text is indicated without making the arrows clickable.
#[derive(Debug, Default)]
pub(crate) struct TitleTabViewport {
    offset: usize,
    last_active: usize,
    last_available: usize,
    last_total_width: usize,
}

impl TitleTabViewport {
    pub(crate) fn layout<T: Copy>(
        &mut self,
        area: Rect,
        tabs: &[(T, String, Style)],
        separator: &str,
        active: usize,
        marker_style: Style,
    ) -> TitleTabLayout<T> {
        let available = usize::from(area.width.saturating_sub(2));
        if available == 0 || tabs.is_empty() {
            self.offset = 0;
            self.last_active = 0;
            self.last_available = available;
            self.last_total_width = 0;
            return TitleTabLayout {
                line: Line::default(),
                hitboxes: Vec::new(),
            };
        }

        let separator_width = separator.width();
        let tab_widths = tabs
            .iter()
            .map(|(_, label, _)| label.width())
            .collect::<Vec<_>>();
        let total_width = tab_widths
            .iter()
            .sum::<usize>()
            .saturating_add(separator_width.saturating_mul(tab_widths.len().saturating_sub(1)));
        let active = active.min(tabs.len() - 1);
        let (active_start, active_end) = tab_extent(&tab_widths, separator_width, active);

        if total_width <= available {
            self.offset = 0;
        } else {
            self.offset = self.offset.min(total_width.saturating_sub(1));
            let moving_left = active < self.last_active;
            let marker_width = overflow_marker_width(available);
            for _ in 0..4 {
                let (_, _, capacity) =
                    viewport_metrics(available, self.offset, total_width, marker_width);
                if capacity == 0 {
                    break;
                }
                let next = if active_start < self.offset {
                    active_start
                } else if active_end > self.offset.saturating_add(capacity) {
                    if active_end.saturating_sub(active_start) > capacity && moving_left {
                        active_start
                    } else {
                        active_end.saturating_sub(capacity)
                    }
                } else {
                    break;
                };
                if next == self.offset {
                    break;
                }
                self.offset = next;
            }

            let (_, right_hidden, capacity) =
                viewport_metrics(available, self.offset, total_width, marker_width);
            if !right_hidden && capacity > 0 {
                self.offset = total_width.saturating_sub(capacity);
            }

            let gained_space =
                available > self.last_available || total_width < self.last_total_width;
            if gained_space {
                while self.offset > 0 {
                    let candidate = self.offset - 1;
                    let (_, _, capacity) =
                        viewport_metrics(available, candidate, total_width, marker_width);
                    if active_start >= candidate && active_end <= candidate.saturating_add(capacity)
                    {
                        self.offset = candidate;
                    } else {
                        break;
                    }
                }
            }
        }

        self.last_active = active;
        self.last_available = available;
        self.last_total_width = total_width;

        let marker_width = overflow_marker_width(available);
        let (left_hidden, right_hidden, capacity) =
            viewport_metrics(available, self.offset, total_width, marker_width);
        let window_end = self.offset.saturating_add(capacity).min(total_width);
        let content_x = area.x.saturating_add(1).saturating_add(if left_hidden {
            u16::try_from(marker_width).unwrap_or(u16::MAX)
        } else {
            0
        });
        let mut spans = Vec::new();
        if left_hidden {
            spans.push(Span::styled(
                overflow_marker(true, marker_width),
                marker_style,
            ));
        }

        let mut hitboxes = Vec::new();
        let mut cursor = 0usize;
        for (index, ((value, label, style), width)) in
            tabs.iter().zip(tab_widths.iter().copied()).enumerate()
        {
            let tab_start = cursor;
            let tab_end = tab_start.saturating_add(width);
            if let Some((start, end)) = intersection(tab_start, tab_end, self.offset, window_end) {
                let fragment = clip_display_columns(label, start - tab_start, end - start);
                if !fragment.is_empty() {
                    spans.push(Span::styled(fragment, *style));
                }
                hitboxes.push(TitleTabHitbox {
                    value: *value,
                    area: Rect::new(
                        content_x
                            .saturating_add(u16::try_from(start - self.offset).unwrap_or(u16::MAX)),
                        area.y,
                        u16::try_from(end - start).unwrap_or(u16::MAX),
                        1,
                    ),
                });
            }
            cursor = tab_end;

            if index + 1 < tabs.len() {
                let separator_end = cursor.saturating_add(separator_width);
                if let Some((start, end)) =
                    intersection(cursor, separator_end, self.offset, window_end)
                {
                    let fragment = clip_display_columns(separator, start - cursor, end - start);
                    if !fragment.is_empty() {
                        spans.push(Span::raw(fragment));
                    }
                }
                cursor = separator_end;
            }
        }
        if right_hidden {
            spans.push(Span::styled(
                overflow_marker(false, marker_width),
                marker_style,
            ));
        }

        TitleTabLayout {
            line: Line::from(spans),
            hitboxes,
        }
    }
}

fn tab_extent(widths: &[usize], separator_width: usize, index: usize) -> (usize, usize) {
    let start = widths[..index]
        .iter()
        .sum::<usize>()
        .saturating_add(separator_width.saturating_mul(index));
    (start, start.saturating_add(widths[index]))
}

fn overflow_marker_width(available: usize) -> usize {
    if available >= 7 {
        3
    } else if available >= 5 {
        2
    } else {
        1
    }
}

fn overflow_marker(left: bool, width: usize) -> &'static str {
    match (left, width) {
        (true, 3) => " ← ",
        (false, 3) => " → ",
        (true, 2) => "← ",
        (false, 2) => " →",
        (true, _) => "←",
        (false, _) => "→",
    }
}

fn viewport_metrics(
    available: usize,
    offset: usize,
    total: usize,
    marker_width: usize,
) -> (bool, bool, usize) {
    let left_hidden = offset > 0 && available > 0;
    let after_left = available.saturating_sub(if left_hidden { marker_width } else { 0 });
    let right_hidden = offset.saturating_add(after_left) < total && after_left > 0;
    let capacity = after_left.saturating_sub(if right_hidden { marker_width } else { 0 });
    (left_hidden, right_hidden, capacity)
}

fn intersection(
    start: usize,
    end: usize,
    window_start: usize,
    window_end: usize,
) -> Option<(usize, usize)> {
    let visible_start = start.max(window_start);
    let visible_end = end.min(window_end);
    (visible_start < visible_end).then_some((visible_start, visible_end))
}

fn clip_display_columns(text: &str, skip: usize, width: usize) -> String {
    let end = skip.saturating_add(width);
    let mut column = 0usize;
    let mut clipped = String::new();
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        let character_end = column.saturating_add(character_width);
        if character_width == 0 {
            if column > skip && column <= end {
                clipped.push(character);
            }
        } else if column >= skip && character_end <= end {
            clipped.push(character);
        } else if character_end > skip && column < end {
            clipped.push_str(&" ".repeat(character_end.min(end) - column.max(skip)));
        }
        column = character_end;
        if column >= end {
            break;
        }
    }
    clipped
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

    #[test]
    fn viewport_clips_text_tracks_focus_and_resets_when_every_tab_fits() {
        let mut viewport = TitleTabViewport::default();
        let tabs = [
            ('a', "AAAAA".to_owned(), Style::default()),
            ('b', "BBBBB".to_owned(), Style::default()),
            ('c', "CCCCC".to_owned(), Style::default()),
            ('d', "DDDDD".to_owned(), Style::default()),
        ];
        let text = |layout: &TitleTabLayout<char>| {
            layout
                .line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        let first = viewport.layout(Rect::new(0, 0, 13, 4), &tabs, "-", 0, Style::default());
        assert_eq!(text(&first), "AAAAA-BB → ");
        assert_eq!(first.hitboxes[1].area, Rect::new(7, 0, 2, 1));

        let middle = viewport.layout(Rect::new(0, 0, 13, 4), &tabs, "-", 2, Style::default());
        assert_eq!(text(&middle), " ← CCCCC → ");
        assert_eq!(middle.hitboxes[0].value, 'c');

        let last = viewport.layout(Rect::new(0, 0, 13, 4), &tabs, "-", 3, Style::default());
        assert_eq!(text(&last), " ← CC-DDDDD");

        let all = viewport.layout(Rect::new(0, 0, 25, 4), &tabs, "-", 3, Style::default());
        assert_eq!(text(&all), "AAAAA-BBBBB-CCCCC-DDDDD");
        assert_eq!(
            all.hitboxes.iter().map(|tab| tab.value).collect::<Vec<_>>(),
            vec!['a', 'b', 'c', 'd']
        );
    }
}
