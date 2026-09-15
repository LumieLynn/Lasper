use super::*;
use crate::application::configuration::{
    ConfigurationDiscovery, ConfigurationDocument, ConfigurationOrigin, X11BindingDeclaration,
    X11BindingScope,
};
use crate::domain::runtime::ImageName;
use ratatui::{backend::TestBackend, Terminal};

fn target(name: &str) -> ConfigurationTarget {
    ConfigurationTarget::Image(ImageName::new(name).unwrap())
}

fn snapshot(name: &str) -> ConfigurationSnapshot {
    ConfigurationSnapshot {
        target: target(name),
        discovery: ConfigurationDiscovery::NamedImageCandidates,
        document: Some(ConfigurationDocument {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            origin: ConfigurationOrigin::Administrator,
            content: "[Exec]\nEnvironment=PRIVATE_VALUE=secret\n[Files]\nBindReadOnly=/tmp/.X11-unix:/mnt/host-x11:idmap\n".into(),
            content_sha256: "fingerprint".into(),
        }),
        x11_bindings: vec![X11BindingDeclaration {
            line: 4, source: "/tmp/.X11-unix".into(), guest_target: "/mnt/host-x11".into(),
            readonly: true, options: vec!["idmap".into()], scope: X11BindingScope::Directory,
        }],
        other_bind_count: 0, diagnostics: vec![],
    }
}

fn loaded() -> ConfigurationView {
    let mut view = ConfigurationView::new(target("archlinux"));
    view.begin_query(3);
    view.finish_query(3, &target("archlinux"), Ok(snapshot("archlinux")));
    view
}

fn render(view: &mut ConfigurationView, width: u16, height: u16) -> String {
    crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn click(area: Rect) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn stale_query_and_wrong_target_results_cannot_replace_current_snapshot() {
    let mut view = loaded();
    view.finish_query(
        2,
        &target("archlinux"),
        Err(ResourceInspectionError::backend("stale")),
    );
    view.finish_query(3, &target("other-image"), Ok(snapshot("other-image")));
    assert!(
        matches!(&view.state, InspectionState::Ready(snapshot) if snapshot.target == target("archlinux"))
    );
    view.finish_query(3, &target("archlinux"), Ok(snapshot("wrong-response")));
    assert!(
        matches!(&view.state, InspectionState::Failed(message) if message.contains("different resource"))
    );
}

#[test]
fn wide_view_separates_navigation_declarations_and_runtime_access() {
    let mut view = loaded();
    let screen = render(&mut view, 140, 28);
    assert!(screen.contains("Host Integration"));
    assert!(screen.contains("/mnt/host-x11"));
    assert!(screen.contains("Current access"));
    assert!(screen.contains("Operation history: not loaded"));
    assert!(!screen.contains("PRIVATE_VALUE"));
    assert!(!screen.contains("MOD"));
    assert!(!view.hits.navigation.intersects(view.hits.content));
    assert!(!view.hits.content.intersects(view.hits.preview));
}

#[test]
fn narrow_view_can_reach_raw_and_close_without_losing_binding_selection() {
    let mut view = loaded();
    let selected = view.list.selected();
    view.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(view.pane, ConfigurationPane::Preview);
    render(&mut view, 60, 15);
    let raw_tab = view
        .hits
        .preview_tabs
        .iter()
        .find(|tab| tab.value == PreviewTab::Raw)
        .unwrap()
        .area;
    view.handle_mouse(click(raw_tab));
    let screen = render(&mut view, 60, 15);
    assert!(screen.contains("PRIVATE_VALUE=secret"));
    assert!(screen.contains("Esc Close"));
    assert_eq!(view.list.selected(), selected);
    assert!(view.expanded.contains(&0));
    let close = view.hits.close;
    assert_eq!(view.handle_mouse(click(close)), ConfigurationAction::Close);
}

#[test]
fn mouse_and_keyboard_select_the_same_panes_and_binding() {
    let mut view = loaded();
    render(&mut view, 140, 28);
    let nav = view.hits.navigation;
    view.handle_mouse(click(nav));
    assert_eq!(view.pane, ConfigurationPane::Navigation);
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(view.pane, ConfigurationPane::Content);
    let binding = view.hits.bindings[0].0;
    view.handle_mouse(click(binding));
    assert!(!view.expanded.contains(&0));
    view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(view.expanded.contains(&0));
}

#[test]
fn refresh_failure_remains_visible_and_raw_scroll_is_bounded() {
    let mut view = loaded();
    view.select_tab(PreviewTab::Raw);
    render(&mut view, 40, 7);
    for _ in 0..100 {
        view.scroll(true);
    }
    assert_eq!(view.preview_scroll, view.preview_max_scroll);
    view.begin_query(4);
    view.finish_query(
        4,
        &target("archlinux"),
        Err(ResourceInspectionError::backend("Permission denied")),
    );
    let screen = render(&mut view, 40, 7);
    assert!(screen.contains("Permission denied"));
    assert!(!screen.contains("No document"));
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        ConfigurationAction::Refresh
    );
}

#[test]
fn tiny_terminal_sizes_keep_rendering_and_escape_safe() {
    let mut view = loaded();
    for (width, height) in [(0, 0), (1, 1), (8, 3), (30, 6)] {
        for pane in [
            ConfigurationPane::Navigation,
            ConfigurationPane::Content,
            ConfigurationPane::Preview,
        ] {
            view.pane = pane;
            render(&mut view, width, height);
            assert_eq!(
                view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                ConfigurationAction::Close
            );
        }
    }
}

#[tokio::test]
async fn closing_view_cancels_its_pending_inspection() {
    let mut view = ConfigurationView::new(target("archlinux"));
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    view.track_query(tokio::spawn(async move {
        let _sender = sender;
        std::future::pending::<()>().await;
    }));
    tokio::task::yield_now().await;
    drop(view);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver)
            .await
            .unwrap()
            .is_err()
    );
}

#[test]
fn tree_navigation_has_depth_pointers_and_keeps_the_active_page_when_collapsed() {
    let mut view = loaded();
    view.pane = ConfigurationPane::Navigation;
    let screen = render(&mut view, 140, 28);
    assert!(screen.contains(">> X11"));
    assert!(screen.contains("> [-] Host Integration"));
    view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(render(&mut view, 140, 28).contains("> [-] Host Integration"));
    view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    let screen = render(&mut view, 140, 28);
    assert!(screen.contains("> [+] Host Integration"));
    assert!(!screen.contains(">> X11"));
    assert!(screen.contains("/mnt/host-x11"));
    assert_eq!(view.pane, ConfigurationPane::Navigation);
    view.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert!(render(&mut view, 140, 28).contains(">> X11"));
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(view.pane, ConfigurationPane::Content);
    assert_eq!(view.list.selected(), Some(0));
    assert!(view.expanded.contains(&0));
}

#[test]
fn tree_rows_are_clickable_and_binding_disclosure_is_separate_from_selection() {
    let mut view = loaded();
    let screen = render(&mut view, 140, 28);
    assert!(screen.contains(">> [-] Socket directory"));
    let parent = Rect::new(view.hits.navigation.x + 1, view.hits.navigation.y + 1, 1, 1);
    view.handle_mouse(click(parent));
    assert!(render(&mut view, 140, 28).contains("> [+] Host Integration"));
    view.handle_mouse(click(parent));
    render(&mut view, 140, 28);
    view.handle_mouse(click(Rect::new(parent.x, parent.y + 1, 1, 1)));
    assert!(render(&mut view, 140, 28).contains(">> X11"));
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(render(&mut view, 140, 28).contains(">> [+] Socket directory"));
    view.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(render(&mut view, 140, 28).contains(">> [-] Socket directory"));
}

#[test]
fn preview_tabs_use_the_border_and_only_visible_titles_are_clickable() {
    for width in [140, 60, 14, 8] {
        let mut view = loaded();
        view.pane = ConfigurationPane::Preview;
        let screen = render(&mut view, width, 18);
        assert!(!screen.contains("Source / Checks"));
        for tab in view.hits.preview_tabs.clone() {
            assert_eq!(tab.area.y, view.hits.preview.y);
            assert!(tab.area.right() < view.hits.preview.right());
            view.handle_mouse(click(tab.area));
            assert_eq!(view.preview_tab, tab.value);
        }
        let raw = view
            .hits
            .preview_tabs
            .iter()
            .find(|tab| tab.value == PreviewTab::Raw)
            .unwrap()
            .area;
        let before = view.preview_tab;
        view.handle_mouse(click(Rect::new(raw.right(), raw.y, 1, 1)));
        assert_eq!(view.preview_tab, before);
    }
}

#[test]
fn raw_renders_shared_configuration_colors_in_the_preview_buffer() {
    crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
    let mut view = loaded();
    view.select_tab(PreviewTab::Raw);
    let mut terminal = Terminal::new(TestBackend::new(60, 15)).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let origin = (view.hits.preview.x + 1, view.hits.preview.y + 1);
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[origin].symbol(), "[");
    assert_eq!(buffer[origin].fg, crate::tui::theme::theme().config_section);
    assert_eq!(
        buffer[(origin.0, origin.1 + 1)].fg,
        crate::tui::theme::theme().config_key
    );
    assert_eq!(
        buffer[(origin.0 + "Environment".len() as u16, origin.1 + 1)].fg,
        crate::tui::theme::theme().config_value
    );
}
