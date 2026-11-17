//! Startup budgets installed by the ELF entry and inherited across fork.
//!
//! These bound Hello, preparation and the prepared lease. Command execution
//! timeouts come from Start and do not use these defaults.

use core::{
    ffi::{c_char, CStr},
    sync::atomic::{AtomicU64, Ordering},
};
use lasper_guest_protocol::MAX_EXECUTION_TIMEOUT_MS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestPolicy {
    hello_timeout_ms: u64,
    prepare_receive_timeout_ms: u64,
    worker_setup_timeout_ms: u64,
    prepared_lease_ms: u32,
}

const DEFAULT_POLICY: GuestPolicy = GuestPolicy {
    hello_timeout_ms: 1_000,
    prepare_receive_timeout_ms: 5_000,
    worker_setup_timeout_ms: 5_000,
    prepared_lease_ms: 45_000,
};

static HELLO: AtomicU64 = AtomicU64::new(DEFAULT_POLICY.hello_timeout_ms);
static PREPARE: AtomicU64 = AtomicU64::new(DEFAULT_POLICY.prepare_receive_timeout_ms);
static SETUP: AtomicU64 = AtomicU64::new(DEFAULT_POLICY.worker_setup_timeout_ms);
static LEASE: AtomicU64 = AtomicU64::new(DEFAULT_POLICY.prepared_lease_ms as u64);

impl Default for GuestPolicy {
    fn default() -> Self {
        DEFAULT_POLICY
    }
}

impl GuestPolicy {
    /// # Safety
    ///
    /// `envp` must be a readable, null-terminated array of valid C strings. The
    /// caller must retain the array and strings until this function returns.
    pub unsafe fn from_envp(envp: *const *const c_char) -> Result<Self, &'static str> {
        let mut policy = Self::default();
        let mut slot = envp;
        while !unsafe { *slot }.is_null() {
            // SAFETY: The caller supplies a terminated array of valid C strings.
            let bytes = unsafe { CStr::from_ptr(*slot) }.to_bytes();
            slot = unsafe { slot.add(1) };
            let Some(equal) = bytes.iter().position(|&b| b == b'=') else {
                continue;
            };
            let (name, value) = (&bytes[..equal], &bytes[equal + 1..]);
            let field = match name {
                b"LASPER_GUEST_HELLO_TIMEOUT_MS" => &mut policy.hello_timeout_ms,
                b"LASPER_GUEST_PREPARE_RECEIVE_TIMEOUT_MS" => {
                    &mut policy.prepare_receive_timeout_ms
                }
                b"LASPER_GUEST_WORKER_SETUP_TIMEOUT_MS" => &mut policy.worker_setup_timeout_ms,
                b"LASPER_GUEST_PREPARED_LEASE_MS" => {
                    let lease = milliseconds(value).ok_or("LASPER_GUEST_PREPARED_LEASE_MS")?;
                    if lease > u64::from(lasper_guest_protocol::MAX_PREPARED_LEASE_MS) {
                        return Err("LASPER_GUEST_PREPARED_LEASE_MS");
                    }
                    policy.prepared_lease_ms = lease as u32;
                    continue;
                }
                _ => continue,
            };
            *field = milliseconds(value).ok_or(match name {
                b"LASPER_GUEST_HELLO_TIMEOUT_MS" => "LASPER_GUEST_HELLO_TIMEOUT_MS",
                b"LASPER_GUEST_PREPARE_RECEIVE_TIMEOUT_MS" => {
                    "LASPER_GUEST_PREPARE_RECEIVE_TIMEOUT_MS"
                }
                _ => "LASPER_GUEST_WORKER_SETUP_TIMEOUT_MS",
            })?;
        }
        for profile in lasper_guest_protocol::protocol_profiles() {
            policy.validate_lease(profile.capacities())?;
        }
        Ok(policy)
    }

    fn validate_lease(
        &self,
        capacities: &lasper_guest_protocol::ProfileCapacities,
    ) -> Result<(), &'static str> {
        if self.prepared_lease_ms == 0 || self.prepared_lease_ms > capacities.max_prepared_lease_ms
        {
            return Err("LASPER_GUEST_PREPARED_LEASE_MS");
        }
        Ok(())
    }

    /// Install during startup, before forking. Children retain their policy copy.
    pub fn install(self) {
        HELLO.store(self.hello_timeout_ms, Ordering::Relaxed);
        PREPARE.store(self.prepare_receive_timeout_ms, Ordering::Relaxed);
        SETUP.store(self.worker_setup_timeout_ms, Ordering::Relaxed);
        LEASE.store(u64::from(self.prepared_lease_ms), Ordering::Relaxed);
    }
}

fn milliseconds(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut result = 0u64;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        result = result.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    (result > 0 && result <= MAX_EXECUTION_TIMEOUT_MS).then_some(result)
}

pub(crate) fn hello_timeout_ms() -> u64 {
    HELLO.load(Ordering::Relaxed)
}

pub(crate) fn prepare_receive_timeout_ms() -> u64 {
    PREPARE.load(Ordering::Relaxed)
}

pub(crate) fn worker_setup_timeout_ms() -> u64 {
    SETUP.load(Ordering::Relaxed)
}

pub(crate) fn prepared_lease_ms() -> u32 {
    LEASE.load(Ordering::Relaxed) as u32
}

#[cfg(test)]
#[path = "../tests/policy/mod.rs"]
mod tests;
