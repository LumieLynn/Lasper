use super::*;
use crate::application::configuration::{
    ConfigurationActivation, ConfigurationApplyReport, ConfigurationDiscovery,
    ConfigurationDocument, ConfigurationOrigin, ConfigurationPreview, ConfigurationRevision,
    ConfigurationWriteTarget, X11BindRecommendation, X11BindingDeclaration, X11BindingScope,
};
use crate::domain::machine::MachineName;
use crate::domain::runtime::ImageName;
use crate::domain::x11::{HostX11Socket, X11SocketRevision};
use ratatui::{backend::TestBackend, Terminal};

fn target(name: &str) -> ConfigurationTarget {
    ConfigurationTarget::Image(ImageName::new(name).unwrap())
}

fn snapshot(name: &str) -> ConfigurationSnapshot {
    let host_socket = HostX11Socket::from_verified_parts(
        0,
        false,
        "/tmp/.X11-unix/X0".into(),
        "/tmp/.X11-unix/X0".into(),
        1000,
        1000,
        0o755,
        42,
        1000,
        1000,
        X11SocketRevision {
            device: 1,
            inode: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
    )
    .unwrap();
    ConfigurationSnapshot {
        target: target(name),
        discovery: ConfigurationDiscovery::NamedImageCandidates,
        candidates: vec![],
        revision: Some(ConfigurationRevision {
            discovery: "discovery".into(),
            read_source: Some("source".into()),
            write_target: Some("write-target".into()),
        }),
        write_target: Some(ConfigurationWriteTarget {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            exists: true,
        }),
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
        x11_bind_recommendation: X11BindRecommendation::Ready {
            private_users: "pick".into(),
            idmapped: true,
        },
        host_x11: crate::application::x11::X11EndpointCatalog {
            sockets: vec![host_socket],
            preferred_display: Some(0),
            diagnostics: vec![],
        },
        other_bind_count: 0, diagnostics: vec![],
    }
}

fn loaded() -> ConfigurationView {
    let mut view = ConfigurationView::new(target("archlinux"));
    view.begin_query(3);
    view.finish_query(3, &target("archlinux"), Ok(snapshot("archlinux")));
    view
}

fn loaded_machine() -> ConfigurationView {
    let target = ConfigurationTarget::Machine(MachineName::new("archlinux").unwrap());
    let mut snapshot = snapshot("archlinux");
    snapshot.target = target.clone();
    snapshot.discovery = ConfigurationDiscovery::MachineNameCandidates;
    let mut view = ConfigurationView::new(target.clone());
    view.begin_query(3);
    view.finish_query(3, &target, Ok(snapshot));
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
    assert!(view.expanded.contains(&0));
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!view.expanded.contains(&0));
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(view.expanded.contains(&0));
}

#[test]
fn clicking_the_checkbox_uses_the_same_desired_state_transition_as_space() {
    let mut view = loaded();
    render(&mut view, 140, 28);
    let checkbox = view.hits.checkboxes[0].0;
    let ConfigurationAction::Preview { edit, .. } = view.handle_mouse(click(checkbox)) else {
        panic!("clicking a checked declaration should request its removal preview");
    };
    assert_eq!(edit.x11_changes, [X11BindingChange::Remove { line: 4 }]);
    assert!(render(&mut view, 140, 28).contains("[ ] ▾ Socket directory"));
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
    assert!(screen.contains(">> [x] ▾ Socket directory"));
    let parent = Rect::new(view.hits.navigation.x + 1, view.hits.navigation.y + 1, 1, 1);
    view.handle_mouse(click(parent));
    assert!(render(&mut view, 140, 28).contains("> [+] Host Integration"));
    view.handle_mouse(click(parent));
    render(&mut view, 140, 28);
    view.handle_mouse(click(Rect::new(parent.x, parent.y + 1, 1, 1)));
    assert!(render(&mut view, 140, 28).contains(">> X11"));
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(render(&mut view, 140, 28).contains(">> [x] ▸ Socket directory"));
    view.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(render(&mut view, 140, 28).contains(">> [x] ▾ Socket directory"));
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
        if let Some(raw) = view
            .hits
            .preview_tabs
            .iter()
            .find(|tab| tab.value == PreviewTab::Raw)
            .map(|tab| tab.area)
        {
            let before = view.preview_tab;
            view.handle_mouse(click(Rect::new(raw.right(), raw.y, 1, 1)));
            assert_eq!(view.preview_tab, before);
        }
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

#[test]
fn draft_preview_is_generation_scoped_and_checking_again_restores_clean_state() {
    let mut view = loaded();
    let action = view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    let ConfigurationAction::Preview { generation, edit } = action else {
        panic!("removing a binding should request a preview");
    };
    assert_eq!(edit.x11_changes.len(), 1);
    assert!(render(&mut view, 140, 28).contains("| MOD"));

    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
        ConfigurationAction::None
    );
    view.finish_preview(
        generation,
        &target("archlinux"),
        Ok(ConfigurationPreview::Ready {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            diff: "stale diff".into(),
            activation: ConfigurationActivation::NextMachineStart,
        }),
    );
    assert!(matches!(view.draft_preview, DraftPreviewState::Clean));
    assert!(!render(&mut view, 140, 28).contains("stale diff"));
}

#[test]
fn dirty_close_requires_confirmation_and_keeps_the_draft_when_cancelled() {
    let mut view = loaded();
    assert!(matches!(
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
        ConfigurationAction::Preview { .. }
    ));
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ConfigurationAction::None
    );
    assert!(render(&mut view, 90, 22).contains("Unsaved changes"));
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        ConfigurationAction::None
    );
    assert!(!view.draft.is_empty());
    view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        ConfigurationAction::Close
    );
}

#[test]
fn only_current_ready_preview_can_be_saved_and_success_clears_the_draft() {
    let mut view = loaded();
    let ConfigurationAction::Preview { generation, edit } =
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
    else {
        panic!("expected preview action");
    };
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        ConfigurationAction::None
    );
    view.finish_preview(
        generation,
        &edit.target,
        Ok(ConfigurationPreview::Ready {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            diff: "--- old\n+++ new\n@@ -1,1 +1,0 @@\n-Bind=x\n".into(),
            activation: ConfigurationActivation::NextMachineStart,
        }),
    );
    assert!(render(&mut view, 140, 28).contains("-Bind=x"));
    let action = view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(matches!(
        action,
        ConfigurationAction::Apply {
            generation: applied_generation,
            ..
        } if applied_generation == generation
    ));
    let message = view.finish_apply(
        generation,
        &edit.target,
        Ok(ConfigurationApplyReport::Applied {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            activation: ConfigurationActivation::NextMachineStart,
        }),
    );
    assert!(message.unwrap().contains("next machine start"));
    assert!(view.draft.is_empty());
}

#[test]
fn space_changes_the_check_state_while_enter_only_folds() {
    let mut view = loaded();
    assert!(matches!(
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
        ConfigurationAction::Preview { .. }
    ));
    assert!(view.expanded.contains(&0));
    view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!view.expanded.contains(&0));
}

#[test]
fn available_endpoint_check_generates_add_and_checking_again_cancels_it() {
    let mut view = loaded();
    view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let ConfigurationAction::Preview { edit, .. } =
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
    else {
        panic!("checking a discovered endpoint should request a preview");
    };
    assert_eq!(
        edit.x11_changes,
        [X11BindingChange::Add {
            source: "/tmp/.X11-unix/X0".into(),
        }]
    );
    assert!(render(&mut view, 140, 28).contains("[x] ▸ :0 X0"));
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
        ConfigurationAction::None
    );
    assert!(view.draft.is_empty());
}

#[test]
fn machine_apply_prompts_for_restart_and_returns_the_exact_target() {
    let mut view = loaded_machine();
    let ConfigurationAction::Preview { generation, edit } =
        view.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
    else {
        panic!("expected preview");
    };
    view.finish_preview(
        generation,
        &edit.target,
        Ok(ConfigurationPreview::Ready {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            diff: "diff".into(),
            activation: ConfigurationActivation::NextMachineStart,
        }),
    );
    assert!(matches!(
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        ConfigurationAction::Apply { .. }
    ));
    view.finish_apply(
        generation,
        &edit.target,
        Ok(ConfigurationApplyReport::Applied {
            path: "/etc/systemd/nspawn/archlinux.nspawn".into(),
            activation: ConfigurationActivation::NextMachineStart,
        }),
    );
    assert!(view.restart_confirmation_pending());
    assert!(render(&mut view, 90, 22).contains("Restart archlinux now"));
    assert_eq!(
        view.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        ConfigurationAction::Restart(MachineName::new("archlinux").unwrap())
    );
}

#[test]
fn machine_x11_check_uses_inline_guest_user_and_keeps_acl_state_separate() {
    let mut view = loaded_machine();
    view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(view.x11_content_focus, X11ContentFocus::GuestUser);
    for character in "alice".chars() {
        view.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let ConfigurationAction::CheckX11 {
        generation,
        target,
        host_socket,
    } = view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("inline Check projection should emit the runtime request");
    };
    assert_eq!(target.machine().as_str(), "archlinux");
    assert_eq!(target.user().as_str(), "alice");
    assert_eq!(host_socket.source(), PathBuf::from("/tmp/.X11-unix/X0"));

    let namespace = crate::application::sessions::ObservedNamespaceIdentity::new(1, 2);
    let instance =
        crate::application::sessions::ObservedMachineInstance::new(42, namespace, namespace);
    let identity = crate::application::sessions::MappedGuestIdentity::verified(
        crate::application::sessions::ObservedGuestIdentity::new(1000, 1000),
        1_437_402_088,
        1_437_402_088,
        instance,
    );
    let context = crate::application::sessions::X11ProjectionContext::verified(
        host_socket,
        "/mnt/host-x11/X0".into(),
        "/tmp/.X11-unix/X0".into(),
        identity,
    );
    let configuration_target = ConfigurationTarget::Machine(MachineName::new("archlinux").unwrap());
    view.finish_x11_probe(generation, &configuration_target, Ok(context));
    let screen = render(&mut view, 140, 30);
    assert!(screen.contains("Guest uid 1000"));
    assert!(screen.contains("1437402088"));
    assert!(screen.contains("X server ACL has not"));
}
