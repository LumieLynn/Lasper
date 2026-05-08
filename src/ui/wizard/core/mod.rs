pub mod context;
pub mod manager;

#[macro_export]
macro_rules! impl_wizard_nav {
    ($name:ident, $comps_macro:ident) => {
        impl $name {
            fn update_focus(&mut self) {
                let mut comps = $comps_macro!(self);
                let len = comps.len();
                if self.focus.active_idx >= len {
                    self.focus.active_idx = len.saturating_sub(1);
                }
                self.focus.update_focus(&mut comps, true);
            }

            fn next(&mut self) {
                let mut comps = $comps_macro!(self);
                self.focus.next(&mut comps);
                self.update_focus();
            }

            fn prev(&mut self) {
                let mut comps = $comps_macro!(self);
                self.focus.prev(&mut comps);
                self.update_focus();
            }
        }
    };
}

#[macro_export]
macro_rules! wizard_set_focus {
    ($self:ident, $focused:ident, $comps_macro:ident) => {{
        if $focused {
            $self.update_focus();
        } else {
            for comp in $comps_macro!($self) {
                comp.set_focus(false);
            }
        }
    }};
}

#[macro_export]
macro_rules! delegate_wizard_navigation {
    ($self:ident, $key:ident, $comps_macro:ident) => {{
        match $key.code {
            ::crossterm::event::KeyCode::Tab => {
                $self.next();
                return $crate::ui::core::EventResult::Consumed;
            }
            ::crossterm::event::KeyCode::BackTab => {
                $self.prev();
                return $crate::ui::core::EventResult::Consumed;
            }
            _ => {}
        }

        let mut comps = $comps_macro!($self);
        if $self.focus.active_idx < comps.len() {
            let res = comps[$self.focus.active_idx].handle_key($key);
            match res {
                $crate::ui::core::EventResult::FocusNext => {
                    $self.next();
                    $crate::ui::core::EventResult::Consumed
                }
                $crate::ui::core::EventResult::FocusPrev => {
                    $self.prev();
                    $crate::ui::core::EventResult::Consumed
                }
                _ => res,
            }
        } else {
            $crate::ui::core::EventResult::Ignored
        }
    }};
}
