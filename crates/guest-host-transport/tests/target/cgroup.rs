use super::*;

fn group(root: &str, hierarchy: Hierarchy) -> MachineCgroup {
    MachineCgroup {
        root: root.into(),
        hierarchy,
    }
}

#[test]
fn leader_and_services_can_occupy_different_descendants_of_the_unit() {
    let group = group("/machine.slice/machine-test.scope", Hierarchy::Unified);
    for path in [
        "/machine.slice/machine-test.scope",
        "/machine.slice/machine-test.scope/init.scope",
        "/machine.slice/machine-test.scope/system.slice/lasper.service",
    ] {
        group
            .verify_membership(&Membership::parse(format!("0::{path}\n").as_bytes()).unwrap())
            .unwrap();
    }
    for path in [
        "/machine.slice/machine-test.scope-other",
        "/machine.slice/machine-other.scope/system.slice/lasper.service",
        "/machine.slice",
        "/",
    ] {
        assert!(
            group
                .verify_membership(&Membership::parse(format!("0::{path}\n").as_bytes()).unwrap())
                .is_err(),
            "{path}"
        );
    }
}

#[test]
fn legacy_and_hybrid_hosts_use_the_named_systemd_hierarchy() {
    for prefix in ["", "0::/unrelated-unified-subtree\n"] {
        let membership = Membership::parse(format!("{prefix}3:cpu,cpuacct:/irrelevant\n7:name=systemd:/machine.slice/test.scope/init.scope\n").as_bytes()).unwrap();
        assert_eq!(membership.hierarchy, Hierarchy::Systemd(7));
        let group = group("/machine.slice/test.scope", Hierarchy::Systemd(7));
        group.verify_membership(&membership).unwrap();
        assert!(group
            .verify_membership(
                &Membership::parse(b"8:name=systemd:/machine.slice/test.scope/init.scope\n")
                    .unwrap()
            )
            .is_err());
    }
}

#[test]
fn ambiguous_missing_or_outside_namespace_membership_is_rejected() {
    for bytes in [
        b"".as_slice(),
        b"1:cpu:/machine\n",
        b"0::/machine\n0::/other\n",
        b"1:name=systemd:/machine\n2:name=systemd:/other\n",
        b"0::/../machine\n",
        b"0::relative\n",
        b"0::/machine/./service\n",
        b"0::/machine\0/service\n",
        b"invalid::/machine\n",
        b"0:\n",
    ] {
        assert!(Membership::parse(bytes).is_err(), "{bytes:?}");
    }
}
