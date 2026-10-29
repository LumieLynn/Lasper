use super::{inspect, parse, MAX_ACCOUNT_ENTRIES, MAX_PASSWD_BYTES};
use crate::domain::machine::accounts::LocalAccountObservation;
use std::fs;

#[test]
fn observation_keeps_local_accounts_and_reports_malformed_lines_without_credentials() {
    let content = b"root:secret-sentinel:0:0:root:/root:/bin/bash\n\
_service:x:42:18::/var/empty:/usr/sbin/nologin\n\
alice@example:x:501:20::/home/alice:/bin/zsh\n\
broken:x:not-a-uid:20::/home/broken:/bin/sh\n\
empty-shell:x:123:123:::\n";
    let observation = parse(content);
    let LocalAccountObservation::Read {
        accounts,
        rejected_lines,
    } = &observation
    else {
        panic!("expected readable accounts: {observation:?}");
    };
    assert_eq!(
        accounts
            .iter()
            .map(|entry| entry.username.as_str())
            .collect::<Vec<_>>(),
        ["root", "_service", "alice@example", "empty-shell"]
    );
    assert_eq!(accounts[1].uid, 42);
    assert_eq!(accounts[2].uid, 501);
    assert_eq!(accounts[3].shell, "");
    assert_eq!(rejected_lines, &[4]);
    assert!(!serde_json::to_string(&observation)
        .unwrap()
        .contains("secret-sentinel"));
}

#[test]
fn invalid_numeric_fields_and_terminal_controls_do_not_become_accounts() {
    let observation = parse(
        b"bad:x:+1:0::/:/bin/sh\n\
bad:x:4294967296:0::/:/bin/sh\n\
bad:x:1:2::/:/bin/\x1bsh\n\
bad:x:1:2::/home/bad:/bin/sh:extra\n",
    );
    assert_eq!(
        observation,
        LocalAccountObservation::Read {
            accounts: vec![],
            rejected_lines: vec![1, 2, 3, 4],
        }
    );
    let LocalAccountObservation::Read {
        accounts,
        rejected_lines,
    } = parse(b"bad:\xff:1:2::/:/bin/sh\ngood:x:1:2::/:/bin/sh")
    else {
        panic!("one bad record must not discard the other local accounts");
    };
    assert_eq!(accounts[0].username, "good");
    assert_eq!(rejected_lines, [1]);
}

#[test]
fn absent_empty_invalid_and_unavailable_passwd_files_are_distinct() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("etc")).unwrap();
    assert_eq!(inspect(root.path()), LocalAccountObservation::Missing);
    fs::write(root.path().join("etc/passwd"), "").unwrap();
    assert_eq!(
        inspect(root.path()),
        LocalAccountObservation::Read {
            accounts: vec![],
            rejected_lines: vec![]
        }
    );
    fs::remove_file(root.path().join("etc/passwd")).unwrap();
    fs::create_dir(root.path().join("etc/passwd")).unwrap();
    assert!(matches!(
        inspect(root.path()),
        LocalAccountObservation::Invalid(_)
    ));
    assert!(matches!(
        inspect(&root.path().join("missing-root")),
        LocalAccountObservation::Unavailable(_)
    ));
}

#[test]
fn inspection_has_explicit_byte_and_entry_bounds() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("etc")).unwrap();
    fs::write(
        root.path().join("etc/passwd"),
        vec![b'a'; MAX_PASSWD_BYTES + 1],
    )
    .unwrap();
    assert!(
        matches!(inspect(root.path()), LocalAccountObservation::Invalid(reason) if reason.contains("byte"))
    );
    let content = "a:x:0:0:::\n".repeat(MAX_ACCOUNT_ENTRIES + 1);
    assert!(
        matches!(parse(content.as_bytes()), LocalAccountObservation::Invalid(reason) if reason.contains("entry"))
    );
}

#[test]
fn the_largest_account_snapshot_fits_in_one_deployment_stream_frame() {
    use crate::application::provisioning::{DeploymentId, DeploymentResult, DeploymentStatus};
    use crate::ipc::protocol::deployment::{
        DeploymentClaimState, DeploymentJobSnapshot, DeploymentStreamFrame,
        MAX_DEPLOYMENT_STREAM_FRAME_BYTES,
    };

    let mut content = "\\\":x:0:0::/\\\":/bin/sh\n".repeat(MAX_ACCOUNT_ENTRIES);
    content.insert_str(0, &"\\\"".repeat((MAX_PASSWD_BYTES - content.len()) / 2));
    let frame = DeploymentStreamFrame::Snapshot(DeploymentJobSnapshot {
        deployment_id: DeploymentId::from_u128(1),
        revision: 1,
        status: DeploymentStatus::Succeeded(DeploymentResult {
            local_accounts: Some(parse(content.as_bytes())),
        }),
        cancellation_requested: false,
        claim: DeploymentClaimState::Released,
    });
    let encoded = serde_json::to_vec(&frame).unwrap();
    assert!(encoded.len() < MAX_DEPLOYMENT_STREAM_FRAME_BYTES);
    let decoded: DeploymentStreamFrame = serde_json::from_slice(&encoded).unwrap();
    let DeploymentStreamFrame::Snapshot(snapshot) = decoded else {
        panic!("expected snapshot")
    };
    assert_eq!(
        snapshot.status,
        match frame {
            DeploymentStreamFrame::Snapshot(snapshot) => snapshot.status,
            _ => unreachable!(),
        }
    );
}
