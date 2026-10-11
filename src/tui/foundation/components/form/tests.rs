use super::*;
use crate::tui::foundation::widgets::inputs::{
    button::ButtonControl, number_input::NumberInput, text_input::Input,
};
use crossterm::event::KeyModifiers;
use ratatui::{backend::TestBackend, Terminal};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Id {
    Extra,
    Number,
    Submit,
    Cancel,
}
struct Editor {
    state: FormState<Id>,
    number: Input,
    show_extra: bool,
    number_enabled: bool,
    submit_enabled: bool,
}
impl Editor {
    fn new() -> Self {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut state = FormState::new(Id::Number);
        state.set_focus(true);
        Self {
            state,
            number: Input::from("42"),
            show_extra: false,
            number_enabled: true,
            submit_enabled: true,
        }
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, Id>) -> R) -> R {
        let mut extra = ButtonControl::new("Extra");
        let mut number = NumberInput::new(&mut self.number, "Number", None, 100);
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        let mut items = Vec::new();
        if self.show_extra {
            items.push(FormItem::field(Id::Extra, &mut extra));
        }
        items.push(FormItem::field(Id::Number, &mut number).with_enabled(self.number_enabled));
        items.push(FormItem::row([
            FormItem::field(Id::Submit, &mut submit).with_enabled(self.submit_enabled),
            FormItem::field(Id::Cancel, &mut cancel),
        ]));
        visit(&mut Form::new(&mut self.state, items).with_actions(Id::Submit, Id::Cancel))
    }
    fn click(&self, id: Id) -> MouseEvent {
        let placement = self
            .state
            .placements
            .iter()
            .find(|entry| entry.id == id)
            .unwrap();
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: placement.area.x,
            row: placement.area.y,
            modifiers: KeyModifiers::NONE,
        }
    }
}

#[test]
fn minimum_height_follows_the_declared_fields_and_rows() {
    let mut editor = Editor::new();
    assert_eq!(editor.with_form(|form| form.minimum_height()), 6);
    editor.show_extra = true;
    assert_eq!(editor.with_form(|form| form.minimum_height()), 9);
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
#[test]
fn conditional_fields_preserve_identity_and_invalidate_old_hitboxes() {
    let mut editor = Editor::new();
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    let old_number = editor.click(Id::Number);
    editor.show_extra = true;
    assert_eq!(
        editor.with_form(|form| form.handle_mouse(old_number)),
        FormOutcome::Ignored
    );
    assert_eq!(editor.state.focus, Some(Id::Number));
    terminal
        .draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    let old_number = editor.click(Id::Number);
    editor.number_enabled = false;
    assert_eq!(
        editor.with_form(|form| form.handle_mouse(old_number)),
        FormOutcome::Ignored
    );
    assert_eq!(editor.state.focus, Some(Id::Submit));
    editor.with_form(|form| form.handle_key(key(KeyCode::BackTab)));
    assert_eq!(editor.state.focus, Some(Id::Extra));
    editor.with_form(|form| form.handle_key(key(KeyCode::Char('9'))));
    assert_eq!(editor.number.value(), "42");
    assert_eq!(editor.number.cursor(), 2);
}
#[test]
fn inactive_forms_and_disabled_actions_do_not_emit_activation() {
    let mut editor = Editor::new();
    editor.submit_enabled = false;
    assert_eq!(
        editor.with_form(|form| form.handle_key(key(KeyCode::Enter))),
        FormOutcome::Consumed
    );
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    let click = editor.click(Id::Cancel);
    editor.state.set_focus(false);
    assert_eq!(
        editor.with_form(|form| form.handle_mouse(click)),
        FormOutcome::Ignored
    );
    assert_eq!(
        editor.with_form(|form| form.handle_key(key(KeyCode::Enter))),
        FormOutcome::Ignored
    );
    editor.state.set_focus(true);
    assert_eq!(
        editor.with_form(|form| form.handle_mouse(click)),
        FormOutcome::Ignored
    );
    terminal
        .draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    let click = editor.click(Id::Cancel);
    assert_eq!(
        editor.with_form(|form| form.handle_mouse(click)),
        FormOutcome::Activated(Id::Cancel)
    );
}

#[test]
fn action_rows_keep_their_height_and_scroll_reveals_focus_then_resets_on_resize() {
    let mut editor = Editor::new();
    let mut compact = Terminal::new(TestBackend::new(40, 5)).unwrap();
    compact
        .draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    editor.with_form(|form| form.handle_key(key(KeyCode::BackTab)));
    compact
        .draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    let submit = editor.state.control_area(Id::Submit).unwrap();
    let cancel = editor.state.control_area(Id::Cancel).unwrap();
    assert_eq!(submit.height, 3);
    assert_eq!(cancel.height, 3);
    assert_eq!(submit.y, cancel.y);
    assert_eq!(editor.state.scroll.offset(), 1);
    assert_eq!(
        editor.with_form(|form| form.handle_mouse(editor_click(cancel))),
        FormOutcome::Activated(Id::Cancel)
    );

    let mut wide = Terminal::new(TestBackend::new(80, 20)).unwrap();
    wide.draw(|frame| editor.with_form(|form| form.render(frame, frame.area())))
        .unwrap();
    assert_eq!(editor.state.scroll.offset(), 0);
    assert_eq!(editor.state.control_area(Id::Number).unwrap().height, 3);
    assert_eq!(editor.state.control_area(Id::Submit).unwrap().height, 3);
    assert_eq!(editor.state.focus, Some(Id::Cancel));
}

fn editor_click(area: Rect) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn split_columns_share_the_same_geometry_and_focus_order_as_keyboard_input() {
    crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
    use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
    let mut state = FormState::new(Id::Extra);
    state.set_focus(true);
    let mut selection = ListControlState::new(0);
    let entries = [ListEntry::new("first"), ListEntry::new("last")];
    let mut input = Input::from("42");
    let mut visit = |operation: &mut dyn FnMut(&mut Form<'_, Id>)| {
        let mut list = ListControl::new(&mut selection, "Left", &entries);
        let mut number = NumberInput::new(&mut input, "Right", None, 100);
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        let mut form = Form::new(
            &mut state,
            [FormItem::row([
                FormItem::field(Id::Extra, &mut list).fill(4),
                FormItem::column([
                    FormItem::field(Id::Number, &mut number),
                    FormItem::row([
                        FormItem::field(Id::Submit, &mut submit),
                        FormItem::field(Id::Cancel, &mut cancel),
                    ]),
                ]),
            ])
            .fill(4)],
        );
        operation(&mut form);
    };
    let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
    terminal
        .draw(|frame| visit(&mut |form| form.render(frame, frame.area())))
        .unwrap();
    visit(&mut |form| {
        let left = form.state.control_area(Id::Extra).unwrap();
        let right = form.state.control_area(Id::Number).unwrap();
        let cancel = form.state.control_area(Id::Cancel).unwrap();
        assert_eq!(left.height, 6);
        assert_eq!(right.height, 3);
        assert_eq!(cancel.height, 3);
        assert_eq!(left.right(), right.x);
        assert_eq!(form.handle_key(key(KeyCode::Tab)), FormOutcome::Consumed);
        assert_eq!(form.state.focus, Some(Id::Number));
        assert_eq!(form.handle_key(key(KeyCode::Enter)), FormOutcome::Ignored);
        assert_eq!(
            form.handle_mouse(editor_click(cancel)),
            FormOutcome::Activated(Id::Cancel)
        );
        assert_eq!(form.state.focus, Some(Id::Cancel));
        assert_eq!(form.handle_key(key(KeyCode::Tab)), FormOutcome::Consumed);
        assert_eq!(form.state.focus, Some(Id::Extra));
    });
}

#[test]
fn release_events_and_noop_input_do_not_change_values_and_hints_match_actions() {
    let mut editor = Editor::new();
    let mut release = key(KeyCode::Char('5'));
    release.kind = KeyEventKind::Release;
    assert_eq!(
        editor.with_form(|form| form.handle_key(release)),
        FormOutcome::Ignored
    );
    assert_eq!(editor.number.value(), "42");
    editor.with_form(|form| form.handle_key(key(KeyCode::Tab)));
    assert_eq!(
        editor
            .state
            .hints
            .iter()
            .find(|hint| hint.key == "Enter")
            .unwrap()
            .label,
        "confirm"
    );
    editor.with_form(|form| form.handle_key(key(KeyCode::Tab)));
    assert_eq!(
        editor
            .state
            .hints
            .iter()
            .find(|hint| hint.key == "Enter")
            .unwrap()
            .label,
        "cancel"
    );
    assert_eq!(
        editor.with_form(|form| form.handle_key(key(KeyCode::Enter))),
        FormOutcome::Activated(Id::Cancel)
    );
}
