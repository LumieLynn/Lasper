use super::*;

fn apply_report() -> ApplyReport {
    ApplyReport::new(crate::domain::machine::MachineName::new("test").unwrap())
}

#[test]
fn apply_report_records_only_resources_created_by_this_attempt() {
    let mut report = apply_report();
    report
        .record_apply(AppliedResource::NspawnConfig, ResourceApplyStatus::Created)
        .unwrap();
    report
        .record_apply(AppliedResource::NspawnConfig, ResourceApplyStatus::Created)
        .unwrap();
    report
        .record_apply(
            AppliedResource::SystemdOverride,
            ResourceApplyStatus::Unchanged,
        )
        .unwrap();

    assert!(report.owns(AppliedResource::NspawnConfig));
    assert!(!report.owns(AppliedResource::SystemdOverride));
}

#[test]
fn unknown_nspawn_owner_blocks_external_image_compensation() {
    let mut report = apply_report();
    let error = report
        .record_apply(
            AppliedResource::NspawnConfig,
            ResourceApplyStatus::ConflictUnknownOwner,
        )
        .unwrap_err();

    assert!(error.to_string().contains("unknown ownership"));
    assert!(!report.owns(AppliedResource::NspawnConfig));
    assert_eq!(report.external_image_blockers.len(), 1);
}

#[test]
fn sidecar_conflicts_are_preserved_and_owned_replacements_are_adopted() {
    let mut report = apply_report();
    report
        .record_apply(
            AppliedResource::NvidiaState,
            ResourceApplyStatus::ConflictUnknownOwner,
        )
        .unwrap();
    report
        .record_apply(
            AppliedResource::SystemdOverride,
            ResourceApplyStatus::ReplacedOwned,
        )
        .unwrap();

    assert!(report.owns(AppliedResource::SystemdOverride));
    assert!(!report.owns(AppliedResource::NvidiaState));
    assert!(report.external_image_blockers.is_empty());
}

#[test]
fn unknown_effects_are_not_rolled_back_as_owned_resources() {
    let mut report = apply_report();
    let unknown = DeploymentResource::NspawnConfig(report.target.clone());
    report.record_outcome_unknown_if_unclassified(unknown.clone());

    assert_eq!(report.outcome_unknown_resources(), vec![unknown]);
    assert!(!report.owns(AppliedResource::NspawnConfig));
    assert!(report.typed_owned_in_reverse().is_empty());
}

#[test]
fn removing_owned_storage_resolves_unknown_rootfs_effects() {
    let mut report = apply_report();
    report.record_created(AppliedResource::LocalStorage);
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsAccounts(
        report.target.clone(),
    ));
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsHostname(
        report.target.clone(),
    ));
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsNetwork(
        report.target.clone(),
    ));
    report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsNvidia(
        report.target.clone(),
    ));

    report.remove_rootfs_dependents();

    assert!(report.outcome_unknown_resources().is_empty());
    assert!(report.owns(AppliedResource::LocalStorage));
}

#[test]
fn failed_unmount_blocks_local_and_external_storage_compensation() {
    let mut report = apply_report();
    report
        .external_image_blockers
        .push("unknown .nspawn owner".into());
    report.block_storage_removal("storage is still mounted");

    assert_eq!(
        report.removal_blockers(AppliedResource::LocalStorage),
        vec!["storage is still mounted"]
    );
    assert_eq!(
        report.removal_blockers(AppliedResource::ExternalImage),
        vec!["unknown .nspawn owner", "storage is still mounted"]
    );
}
