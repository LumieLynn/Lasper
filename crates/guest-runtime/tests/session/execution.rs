extern crate std;

use super::*;
use core::mem::MaybeUninit;
use lasper_guest_protocol::{ExecutionKind, ProcessEnvironment, TerminalSize, V1_0_CAPACITIES};
use std::{vec, vec::Vec};

fn request<'a>(execution: ExecutionKind, entries: &[(&'a str, &'a str)]) -> PrepareRequest<'a> {
    PrepareRequest::new(
        "alice",
        execution,
        GuestProcess::LoginShell,
        ProcessEnvironment::new(entries.iter().copied()).unwrap(),
        (execution == ExecutionKind::Terminal).then(|| TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap()
}

fn entries<'a>(environment: &'a Environment<'_>) -> Vec<&'a str> {
    environment.offsets[..environment.count]
        .iter()
        .map(|offset| {
            CStr::from_bytes_until_nul(&environment.storage[*offset..])
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect()
}

#[test]
fn full_additional_budget_preserves_defaults_and_explicit_empty_overrides() {
    let capacities = V1_0_CAPACITIES;
    let mut account = MaybeUninit::uninit();
    let account = Account::initialize(&mut account);
    let overrides = [("HOME", "/chosen"), ("PAYLOAD", ""), ("TERM", "")];
    let overhead: usize = overrides
        .iter()
        .map(|(name, value)| name.len() + value.len() + 2)
        .sum();
    let value = "x".repeat(capacities.environment_bytes - overhead);
    let request = request(
        ExecutionKind::Terminal,
        &[("HOME", "/chosen"), ("PAYLOAD", &value), ("TERM", "")],
    );
    let mut scratch = vec![0; capacities.prepare_unescape_bytes()];
    let mut storage = vec![0; capacities.merged_environment_bytes()];
    let mut pointers = [core::ptr::null(); MAX_ENVIRONMENT_ENTRIES + 1];
    let mut environment = Environment::new(&mut storage, &mut pointers);
    environment
        .for_account(account, &request, &capacities, &mut scratch)
        .unwrap();
    let entries = entries(&environment);
    assert_eq!(entries.len(), 7);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.starts_with("HOME="))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.starts_with("TERM="))
            .count(),
        1
    );
    assert!(entries.contains(&"HOME=/chosen"));
    assert!(entries.contains(&"TERM="));
    assert!(entries
        .iter()
        .any(|entry| entry.strip_prefix("PAYLOAD=") == Some(value.as_str())));
    assert!(entries.iter().any(|entry| entry.starts_with("PATH=")));
    let count = entries.len();
    assert!(environment.pointers()[count].is_null());
}

#[test]
fn selected_profile_limits_the_default_environment_independently() {
    let mut account = MaybeUninit::uninit();
    let account = Account::initialize(&mut account);
    let request = request(ExecutionKind::Piped, &[]);
    let mut scratch = vec![0; V1_0_CAPACITIES.prepare_unescape_bytes()];
    let mut storage = vec![0; V1_0_CAPACITIES.merged_environment_bytes()];
    let mut pointers = [core::ptr::null(); MAX_ENVIRONMENT_ENTRIES + 1];
    let mut environment = Environment::new(&mut storage, &mut pointers);
    let capacities = ProfileCapacities {
        default_environment_bytes: 1,
        ..V1_0_CAPACITIES
    };
    assert_eq!(
        environment.for_account(account, &request, &capacities, &mut scratch),
        Err(E2BIG)
    );
}

#[test]
fn terminal_default_is_not_inherited_by_piped_processes() {
    let mut account = MaybeUninit::uninit();
    let account = Account::initialize(&mut account);
    for execution in [ExecutionKind::Terminal, ExecutionKind::Piped] {
        let request = request(execution, &[]);
        let mut scratch = vec![0; V1_0_CAPACITIES.prepare_unescape_bytes()];
        let mut storage = vec![0; V1_0_CAPACITIES.merged_environment_bytes()];
        let mut pointers = [core::ptr::null(); MAX_ENVIRONMENT_ENTRIES + 1];
        let mut environment = Environment::new(&mut storage, &mut pointers);
        environment
            .for_account(account, &request, &V1_0_CAPACITIES, &mut scratch)
            .unwrap();
        assert_eq!(
            entries(&environment).contains(&"TERM=dumb"),
            execution == ExecutionKind::Terminal
        );
    }
}

#[test]
fn prepared_environment_strings_and_pointers_stay_inside_the_mapping() {
    let mut account = MaybeUninit::uninit();
    let account = Account::initialize(&mut account);
    let request = PrepareRequest::new(
        "alice",
        ExecutionKind::Terminal,
        GuestProcess::Command(CommandArgv::new(&["/bin/sh"]).unwrap()),
        ProcessEnvironment::new([("TERM", "")]).unwrap(),
        Some(TerminalSize::new(80, 24).unwrap()),
    )
    .unwrap();
    let mut memory = ExecutionMemory::new(&V1_0_CAPACITIES).unwrap();
    let base = memory.mapping.as_ref().as_ptr() as usize;
    let bytes = memory.mapping.as_ref().len();
    let layout = EnvironmentLayout::new(&V1_0_CAPACITIES);
    let mut argv = [0; 64];
    let mut execution = memory.prepare(account, &request, &mut argv).unwrap();
    assert_eq!(bytes, 64 * 1024);
    assert_eq!(execution.environment.storage.len(), 32 * 1024);
    assert_eq!(execution.environment.pointers.len(), 39);
    assert_eq!(
        execution.environment.storage.as_ptr() as usize,
        base + layout.scratch_bytes
    );
    let pointers = execution.environment.pointers();
    assert_eq!(pointers.as_ptr() as usize, base + layout.pointer_offset);
    assert!(pointers.last().unwrap().is_null());
    for pointer in &pointers[..pointers.len() - 1] {
        assert!((base + layout.scratch_bytes..base + layout.pointer_offset)
            .contains(&(*pointer as usize)));
    }
}
