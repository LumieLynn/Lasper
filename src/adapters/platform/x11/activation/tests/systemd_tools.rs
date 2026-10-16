use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output};

use crate::adapters::process::MockCommandRunner;

use super::UserManager;
use crate::adapters::platform::x11::activation::{
    ActivationSpec, UnitManager, COMMAND_TIMEOUT, PATH_UNIT,
};

#[tokio::test]
async fn registration_preserves_argv_and_reports_manager_rejection() {
    let mut runner = MockCommandRunner::new();
    runner
        .expect_run_bounded()
        .times(1)
        .returning(|program, args, timeout| {
            assert_eq!(program, "systemd-run");
            assert_eq!(timeout, COMMAND_TIMEOUT);
            assert!(args.contains(&format!("--unit={PATH_UNIT}")));
            assert!(args.contains(&"--path-property=PathChanged=/run/systemd/machines".into()));
            assert!(args.contains(&"--property=Type=oneshot".into()));
            assert!(args.contains(&"--property=UMask=0077".into()));
            assert_eq!(
                &args[args.len() - 3..],
                [
                    "--",
                    "/opt/Lasper builds/lasper",
                    "--internal-x11-reconcile"
                ]
            );
            assert!(!args.contains(&"--systemd-tools".into()));
            Ok(Output {
                status: ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"Failed to start transient path unit: rejected".to_vec(),
            })
        });
    let manager = UserManager { runner };
    let spec = ActivationSpec {
        executable: "/opt/Lasper builds/lasper".into(),
        watched_directory: "/run/systemd/machines".into(),
    };
    assert!(manager
        .create(&spec, true)
        .await
        .unwrap_err()
        .contains("rejected"));
}

#[tokio::test]
async fn inspection_distinguishes_a_missing_unit_from_a_failed_query() {
    let mut runner = MockCommandRunner::new();
    let mut sequence = mockall::Sequence::new();
    runner
        .expect_run_bounded()
        .times(1)
        .in_sequence(&mut sequence)
        .returning(|_, _, _| {
            Ok(Output {
                status: ExitStatus::from_raw(1 << 8),
                stdout: b"LoadState=not-found\nActiveState=inactive\n".to_vec(),
                stderr: b"localized error text".to_vec(),
            })
        });
    runner
        .expect_run_bounded()
        .times(1)
        .in_sequence(&mut sequence)
        .returning(|_, _, _| {
            Ok(Output {
                status: ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"Failed to connect to bus: access denied".to_vec(),
            })
        });
    let manager = UserManager { runner };
    assert!(manager.inspect(PATH_UNIT).await.unwrap().is_none());
    assert!(manager
        .inspect(PATH_UNIT)
        .await
        .err()
        .unwrap()
        .contains("access denied"));
}
