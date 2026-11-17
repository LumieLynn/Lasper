extern crate std;

use super::*;
use std::{ffi::CString, vec::Vec};

fn parse(values: &[&str]) -> Result<GuestPolicy, &'static str> {
    let strings: Vec<_> = values
        .iter()
        .map(|value| CString::new(*value).unwrap())
        .collect();
    let mut pointers: Vec<_> = strings.iter().map(|value| value.as_ptr()).collect();
    pointers.push(core::ptr::null());
    // SAFETY: All strings and the terminated pointer array remain live.
    unsafe { GuestPolicy::from_envp(pointers.as_ptr()) }
}

#[test]
fn policy_can_increase_every_default_budget() {
    let policy = parse(&[
        "LASPER_GUEST_HELLO_TIMEOUT_MS=2000",
        "LASPER_GUEST_PREPARE_RECEIVE_TIMEOUT_MS=10000",
        "LASPER_GUEST_WORKER_SETUP_TIMEOUT_MS=20000",
        "LASPER_GUEST_PREPARED_LEASE_MS=300000",
    ])
    .unwrap();
    assert_eq!(policy.hello_timeout_ms, 2000);
    assert_eq!(policy.prepare_receive_timeout_ms, 10000);
    assert_eq!(policy.worker_setup_timeout_ms, 20000);
    assert_eq!(policy.prepared_lease_ms, 300000);
}

#[test]
fn invalid_policy_is_rejected_instead_of_wrapping_or_using_defaults() {
    for value in ["", "0", "-1", "1s", "18446744073709551616"] {
        let variable = std::format!("LASPER_GUEST_HELLO_TIMEOUT_MS={value}");
        assert_eq!(parse(&[&variable]), Err("LASPER_GUEST_HELLO_TIMEOUT_MS"));
    }
    assert_eq!(
        parse(&["LASPER_GUEST_PREPARED_LEASE_MS=300001"]),
        Err("LASPER_GUEST_PREPARED_LEASE_MS")
    );
    assert_eq!(parse(&["UNRELATED=invalid"]), Ok(GuestPolicy::default()));
}

#[test]
fn lease_must_fit_each_advertised_profile_without_clamping() {
    let policy = GuestPolicy::default();
    let capacities = lasper_guest_protocol::ProfileCapacities {
        max_prepared_lease_ms: policy.prepared_lease_ms - 1,
        ..lasper_guest_protocol::V1_0_CAPACITIES
    };
    assert_eq!(
        policy.validate_lease(&capacities),
        Err("LASPER_GUEST_PREPARED_LEASE_MS")
    );
    assert_eq!(policy.prepared_lease_ms, 45_000);
}
