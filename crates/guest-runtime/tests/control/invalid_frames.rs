use super::{bootstrap::GuardianProcess, client, client::Client, wire};
use lasper_guest_protocol::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Stage {
    Established,
    Prepared,
    Started,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Major,
    Minor,
    Hello,
    ReverseHelloRejected,
    ReverseExit,
}

fn rejected_at_each_stage(privileged: bool) {
    for stage in [Stage::Established, Stage::Prepared, Stage::Started] {
        for fault in [
            Fault::Major,
            Fault::Minor,
            Fault::Hello,
            Fault::ReverseHelloRejected,
            Fault::ReverseExit,
        ] {
            let mut guardian =
                GuardianProcess::spawn(super::command("fatal-frame", privileged)).unwrap();
            guardian.release();
            guardian.hello();
            let mut guardian = Some(guardian);
            let mut session = match stage {
                Stage::Established => None,
                Stage::Prepared | Stage::Started => Some(Client::prepare(
                    guardian.take().unwrap(),
                    ExecutionKind::Piped,
                    &["/bin/sleep", "30"],
                )),
            };
            if matches!(stage, Stage::Started) {
                session.as_mut().unwrap().start();
            }
            let process = match session.as_mut() {
                Some(session) => &mut session.guardian,
                None => guardian.as_mut().unwrap(),
            };
            process
                .control
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let selected = client::protocol().profile().version();
            let version = match fault {
                Fault::Major => ProtocolVersion {
                    major: selected.major + 1,
                    ..selected
                },
                Fault::Minor => ProtocolVersion {
                    minor: selected.minor + 1,
                    ..selected
                },
                _ => selected,
            };
            let kind = match fault {
                Fault::Major | Fault::Minor => MessageKind::Recheck,
                Fault::Hello => MessageKind::HelloRequest,
                Fault::ReverseHelloRejected => MessageKind::HelloRejected,
                Fault::ReverseExit => MessageKind::Exit,
            };
            let header = ControlFrameHeader::new_with_version(version, kind, 20, 0, 0).unwrap();
            process.control.write_all(&header.encode()).unwrap();
            assert_eq!(
                process.control.read(&mut [0]).unwrap(),
                0,
                "{stage:?}, {fault:?} must close without a response"
            );
            match session.as_mut() {
                Some(session) => {
                    session.wait();
                    // The exported pidfd remains usable after control EOF. A
                    // reaped guardian must not leave its assigned worker alive.
                    let mut descriptor = libc::pollfd {
                        fd: session.descriptors[1].as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 1);
                    assert_ne!(descriptor.revents & libc::POLLIN, 0);
                }
                None => {
                    let guardian = guardian.as_mut().unwrap();
                    assert!(guardian.wait().success());
                    std::fs::remove_dir_all(
                        std::env::temp_dir()
                            .join(format!("lasper-account-test-{}", guardian.child.id())),
                    )
                    .unwrap();
                }
            }
        }
    }
}

fn invalid_requests_preserve_preparation(privileged: bool) {
    let mut session = Client::prepared(
        "invalid-request-ids",
        privileged,
        ExecutionKind::Piped,
        &["/bin/true"],
    );
    let preparation = session.ready.response().preparation_id();
    for (id, token, expected) in [
        (11, preparation, None),
        (11, preparation, Some(GuestErrorCode::InvalidRequest)),
        (10, preparation, Some(GuestErrorCode::InvalidRequest)),
        (
            12,
            PreparationId::new([9; 16]),
            Some(GuestErrorCode::InvalidRequest),
        ),
        (12, preparation, Some(GuestErrorCode::InvalidRequest)),
        (13, preparation, None),
    ] {
        let command = PreparationControlFrame::new(
            id,
            PreparationControlKind::Recheck,
            PreparationControl::new(session.request.broker_id(), token),
        );
        let mut bytes = [0; 512];
        let (header, length) = client::protocol()
            .encode_control_request(&command, &mut bytes)
            .unwrap();
        wire::send(
            session.guardian.control.as_raw_fd(),
            header,
            &bytes[..length],
            &[],
        )
        .unwrap();
        let response = session.receive();
        if let Some(code) = expected {
            let error = client::protocol()
                .decode_error(
                    header,
                    &session.request,
                    Some(preparation),
                    response.header,
                    &response.payload,
                    response.descriptors.len(),
                )
                .unwrap();
            assert_eq!(error.code(), code);
        } else {
            let reply = client::protocol()
                .decode_control_reply(
                    &command,
                    response.header,
                    &response.payload,
                    response.descriptors.len(),
                )
                .unwrap();
            assert!(matches!(
                reply.status(),
                ControlReplyStatus::Prepared { .. }
            ));
        }
    }
    session.next_id = 14;
    session.start();
    let response = session.finish();
    let exit = client::protocol()
        .decode_exit(
            &session.request,
            preparation,
            response.header,
            &response.payload,
            response.descriptors.len(),
        )
        .unwrap();
    assert_eq!(exit.report().command_exit(), Some(ExitStatus::Exited(0)));
    session.wait();
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "fatal frame matrix closes and reaps at Established, Prepared and Started",
        rejected_at_each_stage,
    ),
    (
        "duplicate and regressing IDs and wrong tokens reject without losing preparation",
        invalid_requests_preserve_preparation,
    ),
];
