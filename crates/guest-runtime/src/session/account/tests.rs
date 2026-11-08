extern crate std;

use super::*;
use std::format;

fn resolve<'a>(
    storage: &'a mut MaybeUninit<Account>,
    passwd: &[u8],
    group: &[u8],
) -> Result<&'a Account, WorkerFailure> {
    let account = Account::initialize(storage);
    account.read_passwd("alice", passwd)?;
    account.read_groups(group)?;
    Ok(account)
}

const PASSWD: &[u8] = b"root:x:0:0::/root:/bin/sh\nalice:x:1000:100::/home/alice:/bin/zsh\n";

#[test]
fn snapshot_owns_paths_and_a_complete_deduplicated_group_set() {
    let mut storage = MaybeUninit::uninit();
    let mut passwd = PASSWD.to_vec();
    let account = resolve(&mut storage, &passwd, b"primary:x:100:\nvideo:x:44:alice\nalias:x:44:alice\nother:x:3:malice\naudio:x:18:bob,alice\n").unwrap();
    passwd.fill(0);
    assert_eq!(account.username.as_str(), "alice");
    assert_eq!((account.uid, account.gid), (1000, 100));
    assert_eq!(account.home.as_str(), "/home/alice");
    assert_eq!(account.shell.as_c_str(), c"/bin/zsh");
    assert_eq!(account.groups(), &[18, 44, 100]);
}

#[test]
fn execution_does_not_select_from_an_ambiguous_or_partial_database() {
    let mut storage = MaybeUninit::uninit();
    for content in [
        &b"alice:x:1:2::/:/bin/sh\nalice:x:3:4::/:/bin/sh\n"[..],
        b"alice:x:1:2::/:/bin/sh\nbroken\n",
        b"alice:x:1:2::/:/bin/sh\nalice:x:bad:4::/:/bin/sh\n",
    ] {
        assert_eq!(
            resolve(&mut storage, content, b"").err().unwrap().stage,
            WorkerStage::Account
        );
    }
    let error = resolve(&mut storage, b"root:x:0:0::/:/bin/sh\n", b"")
        .err()
        .unwrap();
    assert_eq!(error, WorkerFailure::new(WorkerStage::Account, ENOENT));
    let error = resolve(&mut storage, PASSWD, b"invalid:x:no:alice\n")
        .err()
        .unwrap();
    assert_eq!(
        error,
        WorkerFailure::new(WorkerStage::GroupDatabase, EINVAL)
    );
}

#[test]
fn sentinel_ids_and_incomplete_paths_cannot_reach_identity_switch() {
    let mut storage = MaybeUninit::uninit();
    for (record, stage) in [
        ("alice:x:4294967295:1::/:/bin/sh", WorkerStage::Account),
        ("alice:x:1:4294967295::/:/bin/sh", WorkerStage::Account),
        ("alice:x:1:2::: /bin/sh", WorkerStage::HomeDirectory),
        ("alice:x:1:2::relative:/bin/sh", WorkerStage::HomeDirectory),
        ("alice:x:1:2::/home/alice:", WorkerStage::LoginShell),
        ("alice:x:1:2::/home/alice:bin/sh", WorkerStage::LoginShell),
    ] {
        assert_eq!(
            resolve(&mut storage, record.as_bytes(), b"")
                .err()
                .unwrap()
                .stage,
            stage
        );
    }
    assert_eq!(
        resolve(&mut storage, PASSWD, b"bad:x:4294967295:alice")
            .err()
            .unwrap()
            .stage,
        WorkerStage::GroupDatabase
    );
}

#[test]
fn excessive_group_memberships_fail_without_dropping_memberships() {
    let mut storage = MaybeUninit::uninit();
    let mut group = std::string::String::new();
    for gid in 100..100 + MAX_GROUPS {
        group.push_str(&format!("g{gid}:x:{gid}:alice\n"));
    }
    assert_eq!(
        resolve(&mut storage, PASSWD, group.as_bytes())
            .unwrap()
            .groups()
            .len(),
        MAX_GROUPS
    );
    group.push_str("extra:x:5000:alice\n");
    assert_eq!(
        resolve(&mut storage, PASSWD, group.as_bytes())
            .err()
            .unwrap(),
        WorkerFailure::new(WorkerStage::GroupDatabase, E2BIG)
    );
}
