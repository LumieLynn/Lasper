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
    let raw_tab = view.hits.raw_tab;
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
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
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
