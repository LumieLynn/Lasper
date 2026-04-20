#[macro_export]
macro_rules! render_column_layout {
    ($lines:expr, $key:expr, $value:expr, $key_width:expr, $val_width:expr, $style:expr) => {{
        let mut wrapped_lines = Vec::new();
        for line in $value.split('\n') {
            if line.is_empty() {
                wrapped_lines.push(String::new());
                continue;
            }
            let mut current = String::new();
            let mut width = 0;
            for c in line.chars() {
                if width >= $val_width {
                    wrapped_lines.push(current);
                    current = String::new();
                    width = 0;
                }
                current.push(c);
                width += 1;
            }
            if !current.is_empty() {
                wrapped_lines.push(current);
            }
        }
        if wrapped_lines.is_empty() {
            wrapped_lines.push(String::new());
        }

        for (i, v_line) in wrapped_lines.into_iter().enumerate() {
            let mut spans = Vec::new();
            if i == 0 {
                spans.push(ratatui::text::Span::styled(
                    format!("{:<width$}", $key, width = $key_width),
                    ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
                ));
            } else {
                spans.push(ratatui::text::Span::raw(" ".repeat($key_width)));
            }
            spans.push(ratatui::text::Span::raw("   ")); // spacer
            spans.push(ratatui::text::Span::styled(v_line, $style));
            $lines.push(ratatui::text::Line::from(spans));
        }
    }};
}

#[macro_export]
macro_rules! handle_nav {
    ($self:expr, $field:ident, $len:expr, $step:expr, $height:expr, $key:expr) => {{
        let max = ($len).saturating_sub($height as usize);
        let safe_max = max.min(u16::MAX as usize) as u16;
        match $key.code {
            crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
                $self.$field = $self.$field.saturating_sub(1)
            }
            crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
                $self.$field = ($self.$field + 1).min(safe_max)
            }
            crossterm::event::KeyCode::PageUp => $self.$field = $self.$field.saturating_sub($step),
            crossterm::event::KeyCode::PageDown => {
                $self.$field = ($self.$field + $step).min(safe_max)
            }
            _ => return crate::ui::core::EventResult::Ignored,
        }
        return crate::ui::core::EventResult::Consumed;
    }};
}
