use super::{ensure_activation, stop_activation, ActivationSpec, PATH_UNIT, SERVICE_UNIT};
use crate::adapters::platform::x11::activation::test_support::FakeUnitManager;

fn spec(executable: &str) -> ActivationSpec {
    ActivationSpec {
        executable: executable.into(),
        watched_directory: "/run/systemd/machines".into(),
    }
}

#[tokio::test]
async fn repeated_registration_does_not_interrupt_a_running_reconcile() {
    let spec = spec("/opt/Lasper builds/lasper");
    let manager = FakeUnitManager::installed(&spec, "activating");
    ensure_activation(&manager, &spec).await.unwrap();
    assert!(manager.operations.lock().unwrap().is_empty());

    stop_activation(&manager).await.unwrap();
    assert_eq!(
        *manager.operations.lock().unwrap(),
        [format!("stop {PATH_UNIT}")]
    );
    assert!(manager
        .units
        .lock()
        .unwrap()
        .get(SERVICE_UNIT)
        .unwrap()
        .running());
}

#[tokio::test]
async fn replacing_a_binary_recreates_only_inactive_owned_units() {
    let manager = FakeUnitManager::installed(&spec("/opt/old/lasper"), "inactive");
    ensure_activation(&manager, &spec("/opt/new/lasper"))
        .await
        .unwrap();
    assert_eq!(
        *manager.operations.lock().unwrap(),
        [format!("stop {PATH_UNIT}"), format!("create {PATH_UNIT}")]
    );

    let manager = FakeUnitManager::installed(&spec("/opt/old/lasper"), "activating");
    assert!(ensure_activation(&manager, &spec("/opt/new/lasper"))
        .await
        .unwrap_err()
        .contains("while reconciliation is running"));
    assert!(manager.operations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn new_claim_rearms_a_path_while_the_previous_worker_is_exiting() {
    let spec = spec("/opt/lasper");
    let manager = FakeUnitManager::installed(&spec, "activating");
    manager.units.lock().unwrap().remove(PATH_UNIT);
    ensure_activation(&manager, &spec).await.unwrap();
    assert_eq!(
        *manager.operations.lock().unwrap(),
        [format!("create-path {PATH_UNIT}")]
    );

    let manager = FakeUnitManager::installed(&spec, "failed");
    manager.units.lock().unwrap().remove(PATH_UNIT);
    ensure_activation(&manager, &spec).await.unwrap();
    assert_eq!(
        *manager.operations.lock().unwrap(),
        [
            format!("reset-failed {SERVICE_UNIT}"),
            format!("create {PATH_UNIT}")
        ]
    );
}

#[tokio::test]
async fn service_collection_during_rearm_recreates_the_complete_pair() {
    for rearm_error in [None, Some("trigger service not found")] {
        let spec = spec("/opt/lasper");
        let mut manager = FakeUnitManager::installed(&spec, "deactivating");
        manager.units.lock().unwrap().remove(PATH_UNIT);
        manager.service_exits_before_rearm = true;
        manager.rearm_error = rearm_error;

        ensure_activation(&manager, &spec).await.unwrap();

        let units = manager.units.lock().unwrap();
        assert_eq!(units.get(PATH_UNIT).unwrap().active_state, "active");
        assert_eq!(
            units.get(SERVICE_UNIT).unwrap().description,
            spec.service_description()
        );
        let mut expected = vec![format!("create-path {PATH_UNIT}")];
        if rearm_error.is_none() {
            expected.push(format!("stop {PATH_UNIT}"));
        }
        expected.push(format!("create {PATH_UNIT}"));
        assert_eq!(*manager.operations.lock().unwrap(), expected);
    }
}

#[tokio::test]
async fn failed_rearm_preserves_the_error_when_the_companion_still_exists() {
    let spec = spec("/opt/lasper");
    let mut manager = FakeUnitManager::installed(&spec, "activating");
    manager.units.lock().unwrap().remove(PATH_UNIT);
    manager.rearm_error = Some("user manager rejected the watcher");

    assert_eq!(
        ensure_activation(&manager, &spec).await.unwrap_err(),
        "user manager rejected the watcher"
    );
    assert_eq!(
        *manager.operations.lock().unwrap(),
        [format!("create-path {PATH_UNIT}")]
    );
}

#[tokio::test]
async fn external_units_are_not_changed_and_registration_failures_propagate() {
    let spec = spec("/opt/lasper");
    let manager = FakeUnitManager::installed(&spec, "inactive");
    manager
        .units
        .lock()
        .unwrap()
        .get_mut(PATH_UNIT)
        .unwrap()
        .transient = false;
    assert!(ensure_activation(&manager, &spec)
        .await
        .unwrap_err()
        .contains("external"));
    assert!(stop_activation(&manager)
        .await
        .unwrap_err()
        .contains("external"));
    assert!(manager.operations.lock().unwrap().is_empty());

    let manager = FakeUnitManager {
        create_error: Some("manager rejected transient unit"),
        ..Default::default()
    };
    assert_eq!(
        ensure_activation(&manager, &spec).await.unwrap_err(),
        "manager rejected transient unit"
    );
}
