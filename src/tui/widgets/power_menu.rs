use crate::tui::core::{Component, EventResult};
use crate::tui::widgets::lists::selectable_list::SelectableList;
use ratatui::{layout::Rect, Frame};

pub struct PowerMenu {
    list: SelectableList<String>,
    image_mode: bool,
}

impl PowerMenu {
    pub fn new(selected: usize) -> Self {
        let items = vec![
            "  ▶  Start Container".to_string(),
            "  ⏹  Poweroff (soft)".to_string(),
            "  ↻  Reboot Container".to_string(),
            "  ⚠  Terminate (force)".to_string(),
            "  ☠  Kill (SIGKILL)".to_string(),
            "  ⬆  Enable at Boot".to_string(),
            "  ⬇  Disable at Boot".to_string(),
        ];

        let mut list = SelectableList::new(" [ Power Actions ] ", items, |s| s.clone());
        list.select(selected);

        Self {
            list,
            image_mode: false,
        }
    }

    pub fn new_for_images(selected: usize) -> Self {
        let items = vec![
            "  ▶  Start Image".to_string(),
            "  ✕  Delete Image".to_string(),
        ];
        let mut list = SelectableList::new(" [ Image Actions ] ", items, |s| s.clone());
        list.select(selected);
        Self {
            list,
            image_mode: true,
        }
    }

    pub fn get_selected(&self) -> usize {
        self.list.selected_idx().unwrap_or(0)
    }

    pub fn is_image_menu(&self) -> bool {
        self.image_mode
    }
}

impl Component for PowerMenu {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // Use fixed height for 7 items + borders + title
        let menu_height = if self.image_mode { 4 } else { 9 };
        let menu_width = if self.image_mode { 28 } else { 32 };

        // Manual centering
        let x = area.x + (area.width.saturating_sub(menu_width)) / 2;
        let y = area.y + (area.height.saturating_sub(menu_height)) / 2;

        let area = Rect::new(
            x,
            y,
            menu_width.min(area.width),
            menu_height.min(area.height),
        );

        f.render_widget(ratatui::widgets::Clear, area);
        self.list.set_focus(true);
        self.list.render(f, area);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> EventResult {
        self.list.handle_key(key)
    }
}
