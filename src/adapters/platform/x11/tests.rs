use super::access::*;
use super::common::CLAIM_RECONCILE_GRACE_MILLIS;
use super::discovery::*;
use super::lifecycle::*;
use super::state::*;
use super::transport::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use x11rb::protocol::xproto::Family;

use crate::application::sessions::{
    MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance, ObservedNamespaceIdentity,
    ShellTarget, ValidatedGuestUserName, X11ProjectionContext,
};
use crate::application::x11::{
    X11AclEntry, X11AclSnapshot, X11AuthorizationPurpose, X11AuthorizationRequest,
    X11EndpointCatalog, X11SourceObservation, X11SourceState,
};
use crate::domain::x11::{HostX11Socket, X11SocketRevision};

#[test]
fn display_and_socket_names_are_kept_distinct() {
    assert_eq!(parse_local_display(":0"), Some(0));
    assert_eq!(parse_local_display("unix/:12.3"), Some(12));
    assert_eq!(parse_local_display("host:0"), None);
    assert_eq!(parse_standard_socket_name("X0"), Some(0));
    assert_eq!(parse_standard_socket_name("X12"), Some(12));
    assert_eq!(parse_standard_socket_name("X0_"), None);
    assert_eq!(parse_standard_socket_name("X"), None);
}

#[test]
fn socket_directory_policy_reports_wslg_compatibility_mode() {
    let path = Path::new("/tmp/.X11-unix");
    let warning = validate_socket_directory_metadata(path, true, 0, 0o777)
        .unwrap()
        .expect("non-sticky root-owned directories need a warning");
    assert!(warning.contains("weaker replacement-race protection"));
    assert!(validate_socket_directory_metadata(path, true, 1000, 0o1777).is_err());
    assert!(validate_socket_directory_metadata(path, true, 0, 0o1777)
        .unwrap()
        .is_none());
    assert!(validate_socket_directory_metadata(path, false, 0, 0o755).is_err());
}

#[test]
fn external_peer_cannot_supply_server_generation_evidence() {
    let socket = HostX11Socket::from_verified_parts(
        0,
        false,
        "/tmp/.X11-unix/X0".into(),
        "/tmp/.X11-unix/X0".into(),
        1000,
        1000,
        0o777,
        0,
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
    let error = x11_peer_start_time(&socket).unwrap_err();
    assert!(error.contains("outside the current PID namespace"));
}

#[test]
fn source_observation_distinguishes_missing_invalid_and_unverified() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let source = directory.join("X0");
    assert_eq!(inspect_source(&source, directory), X11SourceState::Missing);

    fs::write(&source, "not a socket").unwrap();
    assert!(matches!(
        inspect_source(&source, directory),
        X11SourceState::Invalid(_)
    ));
    fs::remove_file(&source).unwrap();

    let _listener = std::os::unix::net::UnixListener::bind(&source).unwrap();
    assert!(matches!(
        inspect_source(&source, directory),
        X11SourceState::Unverified(_)
    ));
    assert!(matches!(
        inspect_source(directory, directory),
        X11SourceState::Unverified(_)
    ));
    assert!(matches!(
        inspect_source(Path::new("/outside/X0"), directory),
        X11SourceState::Unverified(_)
    ));

    let mut catalog = X11EndpointCatalog {
        sources: vec![X11SourceObservation {
            source: source.clone(),
            state: inspect_source(&source, directory),
        }],
        ..Default::default()
    };
    record_endpoint_failure(&mut catalog, &source, "X11 authentication failed");
    assert_eq!(
        catalog.sources[0].state,
        X11SourceState::Unverified("X11 authentication failed".into())
    );
}

#[test]
fn an_observed_entry_is_not_claimed_when_change_hosts_failed() {
    let uid = 1_437_402_088;
    let observed = Ok(X11AclSnapshot::from_wire(
        1,
        vec![X11AclEntry::from_wire(
            Family::SERVER_INTERPRETED.into(),
            numeric_local_user_address(uid),
        )],
    ));
    let endpoint = Ok(());
    let server = Ok(());

    assert!(!authorization_was_confirmed(
        &Err("external race".into()),
        &observed,
        &endpoint,
        &server,
        uid,
    ));
    assert!(authorization_was_confirmed(
        &Ok(()),
        &observed,
        &endpoint,
        &server,
        uid,
    ));
}

pub(super) fn grant_request() -> X11AuthorizationRequest {
    let namespace = ObservedNamespaceIdentity::new(1, 2);
    let identity = MappedGuestIdentity::verified(
        ObservedGuestIdentity::new(1000, 1000),
        1_437_402_088,
        1_437_402_088,
        ObservedMachineInstance::new(42, namespace, namespace),
    );
    let socket = HostX11Socket::from_verified_parts(
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
    let projection = X11ProjectionContext::verified(
        socket,
        "/mnt/host-x11/X0".into(),
        "/tmp/.X11-unix/X0".into(),
        crate::application::sessions::X11FilesystemAccess::observed(true, true),
        identity,
    );
    X11AuthorizationRequest::new(
        ShellTarget::new(
            crate::domain::machine::MachineName::new("archlinux").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        ),
        projection,
    )
}

#[test]
fn grant_records_are_versioned_and_reject_unknown_fields() {
    let request = grant_request();
    let record_id = "0123456789abcdef0123456789abcdef";
    let record = ManagedX11GrantRecord::pending(&request, record_id.into(), 77).unwrap();
    let value = serde_json::to_value(&record).unwrap();
    let decoded: ManagedX11GrantRecord = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(decoded.record_id, record_id);
    assert_eq!(decoded.server_peer_start_time, 77);
    assert_eq!(decoded.acl_address, b"localuser\0#1437402088".to_vec());

    let claim = ManagedX11MachineClaim::active_from_record(&decoded);
    claim.validate(&format!("claim-{record_id}.json")).unwrap();
    let claim_instance = ObservedMachineInstance::new(
        claim.machine_leader_pid,
        ObservedNamespaceIdentity::new(
            claim.machine_pid_namespace.0,
            claim.machine_pid_namespace.1,
        ),
        ObservedNamespaceIdentity::new(
            claim.machine_user_namespace.0,
            claim.machine_user_namespace.1,
        ),
    );
    assert_eq!(
        observe_machine_claim(
            &claim,
            Some(claim.boot_id.as_str()),
            SystemMachineRegistration::Present {
                leader_pid: claim_instance.leader_pid(),
                instance: Some(claim_instance),
            },
            1,
        ),
        Some(MachineClaimObservation::Active)
    );
    assert_eq!(
        observe_machine_claim(
            &claim,
            Some(claim.boot_id.as_str()),
            SystemMachineRegistration::Absent,
            1,
        ),
        Some(MachineClaimObservation::EndedCandidate)
    );
    assert_eq!(
        observe_machine_claim(
            &claim,
            Some(claim.boot_id.as_str()),
            SystemMachineRegistration::Absent,
            123,
        )
        .unwrap()
        .next_phase(&claim, 123),
        Some(MachineClaimPhase::CleanupPending {
            since_unix_millis: 123
        })
    );
    let mut pending = claim.clone();
    pending.phase = MachineClaimPhase::CleanupPending {
        since_unix_millis: 100,
    };
    assert_eq!(
        observe_machine_claim(
            &pending,
            Some(pending.boot_id.as_str()),
            SystemMachineRegistration::Absent,
            100 + CLAIM_RECONCILE_GRACE_MILLIS - 1,
        ),
        Some(MachineClaimObservation::CleanupPending {
            since_unix_millis: 100
        })
    );
    assert_eq!(
        observe_machine_claim(
            &pending,
            Some(pending.boot_id.as_str()),
            SystemMachineRegistration::Absent,
            100 + CLAIM_RECONCILE_GRACE_MILLIS,
        ),
        Some(MachineClaimObservation::CleanupReady)
    );
    assert!(matches!(
        observe_machine_claim(
            &claim,
            Some("22222222-2222-4222-8222-222222222222"),
            SystemMachineRegistration::Present {
                leader_pid: claim_instance.leader_pid(),
                instance: Some(claim_instance),
            },
            1,
        ),
        Some(MachineClaimObservation::NeedsReview(_))
    ));
    assert!(matches!(
        observe_machine_claim(
            &claim,
            None,
            SystemMachineRegistration::Present {
                leader_pid: claim_instance.leader_pid(),
                instance: Some(claim_instance),
            },
            1,
        ),
        Some(MachineClaimObservation::Unavailable(_))
    ));
    let replacement_instance = ObservedMachineInstance::new(
        claim.machine_leader_pid.saturating_add(1),
        ObservedNamespaceIdentity::new(
            claim.machine_pid_namespace.0,
            claim.machine_pid_namespace.1.saturating_add(1),
        ),
        ObservedNamespaceIdentity::new(
            claim.machine_user_namespace.0,
            claim.machine_user_namespace.1.saturating_add(1),
        ),
    );
    assert!(matches!(
        observe_machine_claim(
            &claim,
            Some(claim.boot_id.as_str()),
            SystemMachineRegistration::Present {
                leader_pid: replacement_instance.leader_pid(),
                instance: Some(replacement_instance),
            },
            1,
        ),
        Some(MachineClaimObservation::NeedsReview(reason))
            if reason.contains("different machine instance")
    ));
    let claim_value = serde_json::to_value(&claim).unwrap();
    let decoded_claim: ManagedX11MachineClaim =
        serde_json::from_value(claim_value.clone()).unwrap();
    assert_eq!(decoded_claim.claim_id, record_id);
    assert!(matches!(decoded_claim.phase, MachineClaimPhase::Active));
    let mut review_claim = decoded_claim.clone();
    review_claim.phase = MachineClaimPhase::NeedsReview {
        reason: "machine registration changed".into(),
    };
    review_claim
        .validate(&format!("claim-{record_id}.json"))
        .unwrap();
    let mut invalid_claim = claim_value;
    invalid_claim["key"]["acl_address"] = serde_json::json!([1, 2, 3]);
    let invalid_claim: ManagedX11MachineClaim = serde_json::from_value(invalid_claim).unwrap();
    assert!(invalid_claim
        .validate(&format!("claim-{record_id}.json"))
        .is_err());
    let invalid_claim: ManagedX11MachineClaim =
        serde_json::from_value(serde_json::to_value(&claim).unwrap()).unwrap();
    assert!(invalid_claim
        .validate("claim-ffffffffffffffffffffffffffffffff.json")
        .is_err());

    let mut revoked = decoded.clone();
    revoked.phase = GrantRecordPhase::Revoked;
    let revoked_value = serde_json::to_value(&revoked).unwrap();
    let decoded_revoked: ManagedX11GrantRecord = serde_json::from_value(revoked_value).unwrap();
    assert!(matches!(decoded_revoked.phase, GrantRecordPhase::Revoked));

    let mut unknown = value;
    unknown["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ManagedX11GrantRecord>(unknown).is_err());

    let temporary = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner = temporary.path().metadata().unwrap().uid();
    let grants =
        crate::adapters::trusted_state::TrustedDirectory::open_existing(temporary.path(), owner)
            .unwrap()
            .open_or_create_child("grants", 0o700)
            .unwrap();
    let claims =
        crate::adapters::trusted_state::TrustedDirectory::open_existing(temporary.path(), owner)
            .unwrap()
            .open_or_create_child("claims", 0o700)
            .unwrap();
    let file_name = format!("grant-{record_id}.json");
    grants
        .write_atomic(&file_name, &serde_json::to_vec(&record).unwrap(), 0o600)
        .unwrap();
    claims
        .write_atomic(
            &format!("claim-{record_id}.json"),
            &serde_json::to_vec(&claim).unwrap(),
            0o600,
        )
        .unwrap();
    let catalog = load_grant_records_from(&grants, owner);
    assert!(catalog.complete);
    assert!(catalog.diagnostics.is_empty());
    assert_eq!(catalog.records.len(), 1);
    assert_eq!(catalog.records[0].record_id, record_id);
    let claim_catalog = load_machine_claims_from(&claims, owner);
    assert!(claim_catalog.complete);
    assert!(claim_catalog.diagnostics.is_empty());
    assert_eq!(claim_catalog.claims.len(), 1);
    assert_eq!(claim_catalog.claims[0].grant_record_id, record_id);
    let explicit = X11AuthorizationRequest::for_explicit_session(
        request.target().clone(),
        request.projection().clone(),
    );
    assert_eq!(explicit.purpose(), X11AuthorizationPurpose::ExplicitSession);
    assert!(
        current_generation_record(&catalog, &explicit, 77, &catalog.records[0].boot_id,).is_some()
    );
    assert!(current_generation_record(
        &catalog,
        &explicit,
        77,
        "22222222-2222-4222-8222-222222222222",
    )
    .is_none());

    grants
        .write_atomic(
            "grant-invalid.json",
            &serde_json::to_vec(&record).unwrap(),
            0o600,
        )
        .unwrap();
    grants
        .write_atomic(
            "grant-ffffffffffffffffffffffffffffffff.json",
            b"not-json",
            0o600,
        )
        .unwrap();
    let catalog = load_grant_records_from(&grants, owner);
    assert!(!catalog.complete);
    assert_eq!(catalog.records.len(), 1);
    assert!(catalog
        .diagnostics
        .iter()
        .any(|message| message.contains("invalid record JSON")));
    assert!(catalog
        .diagnostics
        .iter()
        .any(|message| message.contains("record ID does not match")));
}
