//! Trusted user-runtime records for X11 grants and machine lifecycle claims.
//!
//! Operation history and active machine claims are intentionally separate:
//! access code records what ACL mutation occurred, while lifecycle code uses a
//! claim to decide whether that exact grant may remain alive.

use std::path::{Path, PathBuf};

use x11rb::protocol::xproto::Family;

use crate::application::sessions::{
    MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance, ObservedNamespaceIdentity,
    ShellTarget, ValidatedGuestUserName,
};
use crate::application::x11::{
    X11AclEntry, X11AuthorizationRequest, X11GrantRecordCatalog, X11GrantRecordEvidence,
    X11GrantRecordPhase,
};
use crate::domain::machine::MachineName;
use crate::domain::x11::X11SocketRevision;

use super::common::{
    GRANT_RECORD_VERSION, MACHINE_CLAIM_VERSION, MAX_GRANT_DIAGNOSTICS, MAX_GRANT_REASON_BYTES,
    MAX_GRANT_RECORDS, MAX_GRANT_RECORD_BYTES, MAX_MACHINE_CLAIMS, MAX_MACHINE_CLAIM_BYTES,
    X11_SOCKET_DIRECTORY,
};
use super::transport::{
    host_boot_id, numeric_local_user_address, unix_millis, user_runtime_directory,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
pub(super) enum GrantRecordPhase {
    Pending,
    ConfirmedAdded,
    Revoked,
    OutcomeUnknown { reason: String },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManagedX11GrantRecord {
    pub(super) version: u32,
    pub(super) record_id: String,
    pub(super) phase: GrantRecordPhase,
    pub(super) created_unix_millis: u64,
    pub(super) caller_uid: u32,
    pub(super) boot_id: String,
    pub(super) machine: String,
    pub(super) guest_user: String,
    pub(super) guest_uid: u32,
    pub(super) guest_gid: u32,
    pub(super) host_uid: u32,
    pub(super) host_gid: u32,
    pub(super) machine_leader_pid: u32,
    pub(super) machine_pid_namespace: (u64, u64),
    pub(super) machine_user_namespace: (u64, u64),
    pub(super) display: u16,
    pub(super) alternate_endpoint: bool,
    pub(super) source: PathBuf,
    pub(super) canonical_source: PathBuf,
    pub(super) socket_revision: X11SocketRevision,
    pub(super) server_peer: (u32, u32, u32),
    pub(super) server_peer_start_time: u64,
    pub(super) acl_family: u8,
    pub(super) acl_address: Vec<u8>,
}

impl ManagedX11GrantRecord {
    pub(super) fn pending(
        request: &X11AuthorizationRequest,
        record_id: String,
        server_peer_start_time: u64,
    ) -> Result<Self, String> {
        let projection = request.projection();
        let identity = projection.identity();
        let instance = identity.instance();
        let pid_namespace = instance.pid_namespace();
        let user_namespace = instance.user_namespace();
        let host_uid = identity.host_uid();
        Ok(Self {
            version: GRANT_RECORD_VERSION,
            record_id,
            phase: GrantRecordPhase::Pending,
            created_unix_millis: unix_millis()?,
            caller_uid: uzers::get_effective_uid(),
            boot_id: host_boot_id()?,
            machine: request.target().machine().as_str().to_owned(),
            guest_user: request.target().user().as_str().to_owned(),
            guest_uid: identity.guest().uid(),
            guest_gid: identity.guest().gid(),
            host_uid,
            host_gid: identity.host_gid(),
            machine_leader_pid: instance.leader_pid(),
            machine_pid_namespace: (pid_namespace.device(), pid_namespace.inode()),
            machine_user_namespace: (user_namespace.device(), user_namespace.inode()),
            display: projection.host_socket().display(),
            alternate_endpoint: projection.host_socket().alternate(),
            source: projection.host_socket().source().to_path_buf(),
            canonical_source: projection.host_socket().canonical_path().to_path_buf(),
            socket_revision: projection.host_socket().revision(),
            server_peer: projection.host_socket().peer_identity().legacy_tuple(),
            server_peer_start_time,
            acl_family: Family::SERVER_INTERPRETED.into(),
            acl_address: numeric_local_user_address(host_uid),
        })
    }

    pub(super) fn into_evidence(self, file_name: &str) -> Result<X11GrantRecordEvidence, String> {
        if self.version != GRANT_RECORD_VERSION {
            return Err(format!("unsupported record version {}", self.version));
        }
        let expected_name = format!("grant-{}.json", self.record_id);
        if file_name != expected_name || !valid_record_id(&self.record_id) {
            return Err("record ID does not match its filename".into());
        }
        let boot_id = uuid::Uuid::parse_str(&self.boot_id)
            .map_err(|error| format!("invalid host boot identity: {error}"))?
            .to_string();
        let machine = MachineName::new(self.machine)
            .map_err(|error| format!("invalid machine name: {error}"))?;
        let guest_user = ValidatedGuestUserName::new(self.guest_user)
            .map_err(|error| format!("invalid guest user: {error}"))?;
        if self.machine_leader_pid == 0
            || self.machine_leader_pid > i32::MAX as u32
            || self.machine_pid_namespace.1 == 0
            || self.machine_user_namespace.1 == 0
        {
            return Err("recorded machine instance is invalid".into());
        }
        if self.server_peer.0 == 0 || self.server_peer.0 > i32::MAX as u32 {
            return Err("recorded X server PID is invalid".into());
        }
        if self.server_peer_start_time == 0 {
            return Err("recorded X server start time is invalid".into());
        }
        let expected_source = Path::new(X11_SOCKET_DIRECTORY).join(format!(
            "X{}{}",
            self.display,
            if self.alternate_endpoint { "_" } else { "" }
        ));
        if self.source != expected_source
            || !self.canonical_source.is_absolute()
            || self.socket_revision.inode == 0
        {
            return Err("recorded X11 endpoint path is invalid".into());
        }
        let expected_acl = numeric_local_user_address(self.host_uid);
        if self.acl_family != u8::from(Family::SERVER_INTERPRETED)
            || self.acl_address != expected_acl
        {
            return Err("recorded ACL key is not the exact mapped numeric localuser".into());
        }
        let phase = match self.phase {
            GrantRecordPhase::Pending => X11GrantRecordPhase::Pending,
            GrantRecordPhase::ConfirmedAdded => X11GrantRecordPhase::ConfirmedAdded,
            GrantRecordPhase::Revoked => X11GrantRecordPhase::Revoked,
            GrantRecordPhase::OutcomeUnknown { reason } => {
                if reason.len() > MAX_GRANT_REASON_BYTES || reason.chars().any(char::is_control) {
                    return Err("recorded outcome reason is invalid".into());
                }
                X11GrantRecordPhase::OutcomeUnknown { reason }
            }
        };
        let pid_namespace = ObservedNamespaceIdentity::new(
            self.machine_pid_namespace.0,
            self.machine_pid_namespace.1,
        );
        let user_namespace = ObservedNamespaceIdentity::new(
            self.machine_user_namespace.0,
            self.machine_user_namespace.1,
        );
        Ok(X11GrantRecordEvidence {
            record_id: self.record_id,
            phase,
            created_unix_millis: self.created_unix_millis,
            caller_uid: self.caller_uid,
            boot_id,
            target: ShellTarget::new(machine, guest_user),
            identity: MappedGuestIdentity::verified(
                ObservedGuestIdentity::new(self.guest_uid, self.guest_gid),
                self.host_uid,
                self.host_gid,
                ObservedMachineInstance::new(
                    self.machine_leader_pid,
                    pid_namespace,
                    user_namespace,
                ),
            ),
            display: self.display,
            alternate_endpoint: self.alternate_endpoint,
            source: self.source,
            canonical_source: self.canonical_source,
            socket_revision: self.socket_revision,
            server_peer: self.server_peer,
            server_peer_start_time: self.server_peer_start_time,
            acl_entry: X11AclEntry::from_wire(self.acl_family, self.acl_address),
        })
    }
}

/// The exact desktop ACL identity owned by one Lasper grant.  A machine
/// claim references this value instead of treating a display number as an
/// ownership key: the X server generation, endpoint revision, and mapped UID
/// are all part of the key.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManagedX11GrantKey {
    pub(super) host_uid: u32,
    pub(super) display: u16,
    pub(super) alternate_endpoint: bool,
    pub(super) source: PathBuf,
    pub(super) canonical_source: PathBuf,
    pub(super) socket_revision: X11SocketRevision,
    pub(super) server_peer: (u32, u32, u32),
    pub(super) server_peer_start_time: u64,
    pub(super) acl_family: u8,
    pub(super) acl_address: Vec<u8>,
}

impl ManagedX11GrantKey {
    pub(super) fn from_record(record: &ManagedX11GrantRecord) -> Self {
        Self {
            host_uid: record.host_uid,
            display: record.display,
            alternate_endpoint: record.alternate_endpoint,
            source: record.source.clone(),
            canonical_source: record.canonical_source.clone(),
            socket_revision: record.socket_revision,
            server_peer: record.server_peer,
            server_peer_start_time: record.server_peer_start_time,
            acl_family: record.acl_family,
            acl_address: record.acl_address.clone(),
        }
    }

    pub(super) fn from_evidence(record: &X11GrantRecordEvidence) -> Self {
        Self {
            host_uid: record.identity.host_uid(),
            display: record.display,
            alternate_endpoint: record.alternate_endpoint,
            source: record.source.clone(),
            canonical_source: record.canonical_source.clone(),
            socket_revision: record.socket_revision,
            server_peer: record.server_peer,
            server_peer_start_time: record.server_peer_start_time,
            acl_family: record.acl_entry.family(),
            acl_address: record.acl_entry.address().to_vec(),
        }
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        let expected_source = Path::new(X11_SOCKET_DIRECTORY).join(format!(
            "X{}{}",
            self.display,
            if self.alternate_endpoint { "_" } else { "" }
        ));
        if self.source != expected_source
            || !self.canonical_source.is_absolute()
            || self.socket_revision.inode == 0
        {
            return Err("machine claim contains an invalid X11 endpoint key".into());
        }
        if self.server_peer.0 == 0 || self.server_peer.0 > i32::MAX as u32 {
            return Err("machine claim contains an invalid X server PID".into());
        }
        if self.server_peer_start_time == 0 {
            return Err("machine claim contains an invalid X server generation".into());
        }
        if self.acl_family != u8::from(Family::SERVER_INTERPRETED)
            || self.acl_address != numeric_local_user_address(self.host_uid)
        {
            return Err(
                "machine claim does not contain the exact numeric localuser ACL key".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
pub(super) enum MachineClaimPhase {
    Active,
    CleanupPending {
        since_unix_millis: u64,
    },
    Ended,
    NeedsReview {
        reason: String,
    },
    /// An uncertain ACL mutation must not be retried automatically. Older
    /// versions also stored observation errors here; lifecycle recognizes only
    /// those known errors when recovering existing claims.
    Unknown {
        reason: String,
    },
}

impl MachineClaimPhase {
    pub(super) fn requires_reconcile(&self) -> bool {
        !matches!(self, Self::Ended)
    }
}

/// A lifecycle claim is deliberately separate from the grant operation
/// record.  The grant record answers “what ACL mutation happened”; this file
/// answers “which currently-running system-scope machine instance may keep
/// that exact ACL key alive”.  Keeping the two records separate lets future
/// reconciliation end a claim without rewriting operation history.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManagedX11MachineClaim {
    pub(super) version: u32,
    pub(super) claim_id: String,
    pub(super) grant_record_id: String,
    pub(super) phase: MachineClaimPhase,
    pub(super) created_unix_millis: u64,
    pub(super) boot_id: String,
    pub(super) machine: String,
    pub(super) guest_user: String,
    pub(super) guest_uid: u32,
    pub(super) guest_gid: u32,
    pub(super) host_gid: u32,
    pub(super) machine_leader_pid: u32,
    pub(super) machine_pid_namespace: (u64, u64),
    pub(super) machine_user_namespace: (u64, u64),
    pub(super) key: ManagedX11GrantKey,
}

impl ManagedX11MachineClaim {
    pub(super) fn active_from_record(record: &ManagedX11GrantRecord) -> Self {
        Self {
            version: MACHINE_CLAIM_VERSION,
            claim_id: record.record_id.clone(),
            grant_record_id: record.record_id.clone(),
            phase: MachineClaimPhase::Active,
            created_unix_millis: record.created_unix_millis,
            boot_id: record.boot_id.clone(),
            machine: record.machine.clone(),
            guest_user: record.guest_user.clone(),
            guest_uid: record.guest_uid,
            guest_gid: record.guest_gid,
            host_gid: record.host_gid,
            machine_leader_pid: record.machine_leader_pid,
            machine_pid_namespace: record.machine_pid_namespace,
            machine_user_namespace: record.machine_user_namespace,
            key: ManagedX11GrantKey::from_record(record),
        }
    }

    pub(super) fn validate(&self, file_name: &str) -> Result<(), String> {
        if self.version != MACHINE_CLAIM_VERSION {
            return Err(format!(
                "unsupported machine claim version {}",
                self.version
            ));
        }
        if !valid_record_id(&self.claim_id)
            || self.claim_id != self.grant_record_id
            || file_name != format!("claim-{}.json", self.claim_id)
            || !valid_record_id(&self.grant_record_id)
        {
            return Err("machine claim ID does not match its filename or grant record".into());
        }
        uuid::Uuid::parse_str(&self.boot_id)
            .map_err(|error| format!("invalid machine claim host boot identity: {error}"))?;
        MachineName::new(self.machine.clone())
            .map_err(|error| format!("invalid machine claim machine name: {error}"))?;
        ValidatedGuestUserName::new(self.guest_user.clone())
            .map_err(|error| format!("invalid machine claim guest user: {error}"))?;
        if self.machine_leader_pid == 0
            || self.machine_leader_pid > i32::MAX as u32
            || self.machine_pid_namespace.1 == 0
            || self.machine_user_namespace.1 == 0
        {
            return Err("machine claim contains an invalid machine instance".into());
        }
        self.key.validate()?;
        if let MachineClaimPhase::NeedsReview { reason } | MachineClaimPhase::Unknown { reason } =
            &self.phase
        {
            if reason.is_empty()
                || reason.len() > MAX_GRANT_REASON_BYTES
                || reason.chars().any(char::is_control)
            {
                return Err("machine claim state reason is invalid".into());
            }
        }
        if let MachineClaimPhase::CleanupPending { since_unix_millis } = self.phase {
            if since_unix_millis == 0 {
                return Err("machine claim cleanup-pending timestamp is invalid".into());
            }
        }
        Ok(())
    }

    pub(super) fn matches_machine_instance(&self, instance: ObservedMachineInstance) -> bool {
        self.machine_leader_pid == instance.leader_pid()
            && self.machine_pid_namespace
                == (
                    instance.pid_namespace().device(),
                    instance.pid_namespace().inode(),
                )
            && self.machine_user_namespace
                == (
                    instance.user_namespace().device(),
                    instance.user_namespace().inode(),
                )
    }
}

#[derive(Clone, Debug)]
pub(super) struct MachineClaimCatalog {
    pub(super) claims: Vec<ManagedX11MachineClaim>,
    pub(super) diagnostics: Vec<String>,
    pub(super) complete: bool,
}

impl Default for MachineClaimCatalog {
    fn default() -> Self {
        Self {
            claims: Vec::new(),
            diagnostics: Vec::new(),
            complete: true,
        }
    }
}

pub(super) fn valid_record_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) struct X11RuntimeState {
    pub(super) access: crate::adapters::trusted_state::TrustedDirectory,
    pub(super) grants: crate::adapters::trusted_state::TrustedDirectory,
    pub(super) claims: Option<crate::adapters::trusted_state::TrustedDirectory>,
}

impl X11RuntimeState {
    pub(super) fn open() -> Result<Self, String> {
        let uid = uzers::get_effective_uid();
        let runtime = user_runtime_directory(uid)?;
        let runtime =
            crate::adapters::trusted_state::TrustedDirectory::open_existing(&runtime, uid)
                .map_err(|error| format!("open user runtime directory: {error}"))?;
        let lasper = runtime
            .open_or_create_child("lasper", 0o700)
            .map_err(|error| format!("open Lasper runtime directory: {error}"))?;
        let access = lasper
            .open_or_create_child("x11-access", 0o700)
            .map_err(|error| format!("open X11 access runtime directory: {error}"))?;
        let grants = access
            .open_or_create_child("grants", 0o700)
            .map_err(|error| format!("open X11 grant record directory: {error}"))?;
        let claims = access
            .open_or_create_child("claims", 0o700)
            .map_err(|error| format!("open X11 machine claim directory: {error}"))?;
        Ok(Self {
            access,
            grants,
            claims: Some(claims),
        })
    }

    pub(super) fn open_existing() -> Result<Option<Self>, String> {
        let uid = uzers::get_effective_uid();
        let runtime = user_runtime_directory(uid)?;
        let runtime =
            crate::adapters::trusted_state::TrustedDirectory::open_existing(&runtime, uid)
                .map_err(|error| format!("open user runtime directory: {error}"))?;
        let Some(lasper) = runtime
            .open_existing_child("lasper")
            .map_err(|error| format!("open Lasper runtime directory: {error}"))?
        else {
            return Ok(None);
        };
        let Some(access) = lasper
            .open_existing_child("x11-access")
            .map_err(|error| format!("open X11 access runtime directory: {error}"))?
        else {
            return Ok(None);
        };
        let Some(grants) = access
            .open_existing_child("grants")
            .map_err(|error| format!("open X11 grant record directory: {error}"))?
        else {
            return Ok(None);
        };
        let claims = access
            .open_existing_child("claims")
            .map_err(|error| format!("open X11 machine claim directory: {error}"))?;
        Ok(Some(Self {
            access,
            grants,
            claims,
        }))
    }

    pub(super) fn lock(&self) -> Result<std::fs::File, String> {
        self.access
            .lock_exclusive("acl")
            .map_err(|error| format!("lock X11 access operations: {error}"))
    }

    pub(super) fn write(
        &self,
        file_name: &str,
        record: &ManagedX11GrantRecord,
    ) -> Result<(), String> {
        let bytes = serde_json::to_vec(record)
            .map_err(|error| format!("serialize X11 grant record: {error}"))?;
        self.grants
            .write_atomic(file_name, &bytes, 0o600)
            .map_err(|error| format!("persist X11 grant record: {error}"))
    }

    pub(super) fn write_claim(&self, claim: &ManagedX11MachineClaim) -> Result<(), String> {
        let claims = self
            .claims
            .as_ref()
            .ok_or_else(|| "X11 machine claim directory is unavailable".to_owned())?;
        let file_name = format!("claim-{}.json", claim.claim_id);
        claim.validate(&file_name)?;
        let bytes = serde_json::to_vec(claim)
            .map_err(|error| format!("serialize X11 machine claim: {error}"))?;
        claims
            .write_atomic(&file_name, &bytes, 0o600)
            .map_err(|error| format!("persist X11 machine claim: {error}"))
    }

    /// End the claim associated with one grant record. Older runtime state
    /// has no claim directory; that is an ordinary compatibility case and is
    /// intentionally treated as a no-op.
    pub(super) fn end_claim(&self, grant_record_id: &str) -> Result<(), String> {
        let Some(claims) = self.claims.as_ref() else {
            return Ok(());
        };
        let file_name = format!("claim-{grant_record_id}.json");
        let Some(file) = claims
            .read_bounded(&file_name, MAX_MACHINE_CLAIM_BYTES)
            .map_err(|error| format!("read X11 machine claim {grant_record_id}: {error}"))?
        else {
            return Ok(());
        };
        if file.uid != uzers::get_effective_uid() || file.mode & 0o077 != 0 {
            return Err(format!(
                "X11 machine claim {grant_record_id} is not owned by the invoking user"
            ));
        }
        let mut claim: ManagedX11MachineClaim = serde_json::from_slice(&file.bytes)
            .map_err(|error| format!("invalid X11 machine claim {grant_record_id}: {error}"))?;
        claim
            .validate(&file_name)
            .map_err(|error| format!("invalid X11 machine claim {grant_record_id}: {error}"))?;
        claim.phase = MachineClaimPhase::Ended;
        self.write_claim(&claim)
    }

    pub(super) fn load_records(&self) -> X11GrantRecordCatalog {
        load_grant_records_from(&self.grants, uzers::get_effective_uid())
    }

    pub(super) fn load_claims(&self) -> MachineClaimCatalog {
        self.claims
            .as_ref()
            .map(|claims| load_machine_claims_from(claims, uzers::get_effective_uid()))
            .unwrap_or_default()
    }
}

pub(super) fn has_unresolved_claims_sync() -> Result<bool, String> {
    let Some(state) = X11RuntimeState::open_existing()? else {
        return Ok(false);
    };
    let _lock = state.lock()?;
    claims_need_activation(&state.load_claims(), &state.load_records())
}

/// Keep the lifecycle wake-up while any claim or confirmed grant remains.
/// Observation/review failures are unresolved work, not evidence of cleanup.
pub(super) fn claims_need_activation(
    claims: &MachineClaimCatalog,
    records: &X11GrantRecordCatalog,
) -> Result<bool, String> {
    if !claims.complete {
        return Err(claims
            .diagnostics
            .first()
            .cloned()
            .unwrap_or_else(|| "X11 machine claim set is incomplete".into()));
    }
    if !records.complete {
        return Err(records
            .diagnostics
            .first()
            .cloned()
            .unwrap_or_else(|| "X11 grant record set is incomplete".into()));
    }
    Ok(claims
        .claims
        .iter()
        .any(|claim| claim.phase.requires_reconcile())
        || records
            .records
            .iter()
            .any(|record| matches!(record.phase, X11GrantRecordPhase::ConfirmedAdded)))
}

pub(super) fn load_existing_grant_records() -> X11GrantRecordCatalog {
    match X11RuntimeState::open_existing() {
        Ok(Some(state)) => state.load_records(),
        Ok(None) => X11GrantRecordCatalog::empty(),
        Err(error) => X11GrantRecordCatalog::unavailable(format!(
            "X11 grant records could not be inspected: {error}"
        )),
    }
}

pub(super) fn load_existing_machine_claims() -> MachineClaimCatalog {
    match X11RuntimeState::open_existing() {
        Ok(Some(state)) => state.load_claims(),
        Ok(None) => MachineClaimCatalog::default(),
        Err(error) => MachineClaimCatalog {
            diagnostics: vec![format!(
                "X11 machine claims could not be inspected: {error}"
            )],
            complete: false,
            ..Default::default()
        },
    }
}

pub(super) fn load_grant_records_from(
    grants: &crate::adapters::trusted_state::TrustedDirectory,
    expected_uid: u32,
) -> X11GrantRecordCatalog {
    let mut names = match grants.entry_names() {
        Ok(names) => names
            .into_iter()
            .filter(|name| {
                name.strip_prefix("grant-")
                    .and_then(|value| value.strip_suffix(".json"))
                    .is_some()
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return X11GrantRecordCatalog::unavailable(format!(
                "X11 grant record directory could not be listed: {error}"
            ));
        }
    };
    names.sort();
    let mut complete = true;
    let mut diagnostics = Vec::new();
    let mut omitted_diagnostics = 0usize;
    if names.len() > MAX_GRANT_RECORDS {
        complete = false;
        let excess = names.len() - MAX_GRANT_RECORDS;
        names.truncate(MAX_GRANT_RECORDS);
        push_grant_diagnostic(
            &mut diagnostics,
            &mut omitted_diagnostics,
            format!(
                "X11 grant record count exceeded {MAX_GRANT_RECORDS}; {excess} records were not read"
            ),
        );
    }

    let mut records = Vec::with_capacity(names.len());
    for name in names {
        let record = (|| {
            let file = grants
                .read_bounded(&name, MAX_GRANT_RECORD_BYTES)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "record disappeared while it was being read".to_owned())?;
            if file.uid != expected_uid || file.mode & 0o077 != 0 {
                return Err(format!(
                    "record must be owned by uid {expected_uid} and inaccessible to group/other"
                ));
            }
            serde_json::from_slice::<ManagedX11GrantRecord>(&file.bytes)
                .map_err(|error| format!("invalid record JSON: {error}"))?
                .into_evidence(&name)
        })();
        match record {
            Ok(record) => records.push(record),
            Err(error) => {
                complete = false;
                push_grant_diagnostic(
                    &mut diagnostics,
                    &mut omitted_diagnostics,
                    format!("{name:?}: {}", bounded_record_diagnostic(&error)),
                );
            }
        }
    }
    if omitted_diagnostics > 0 {
        diagnostics.push(format!(
            "{omitted_diagnostics} additional X11 grant record diagnostics were omitted"
        ));
    }
    X11GrantRecordCatalog {
        records,
        diagnostics,
        complete,
    }
}

pub(super) fn load_machine_claims_from(
    claims: &crate::adapters::trusted_state::TrustedDirectory,
    expected_uid: u32,
) -> MachineClaimCatalog {
    let mut names = match claims.entry_names() {
        Ok(names) => names
            .into_iter()
            .filter(|name| {
                name.strip_prefix("claim-")
                    .and_then(|value| value.strip_suffix(".json"))
                    .is_some()
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return MachineClaimCatalog {
                diagnostics: vec![format!(
                    "X11 machine claim directory could not be listed: {error}"
                )],
                complete: false,
                ..Default::default()
            }
        }
    };
    names.sort();
    let mut complete = true;
    let mut diagnostics = Vec::new();
    let mut omitted_diagnostics = 0usize;
    if names.len() > MAX_MACHINE_CLAIMS {
        complete = false;
        let excess = names.len() - MAX_MACHINE_CLAIMS;
        names.truncate(MAX_MACHINE_CLAIMS);
        push_claim_diagnostic(
            &mut diagnostics,
            &mut omitted_diagnostics,
            format!(
                "X11 machine claim count exceeded {MAX_MACHINE_CLAIMS}; {excess} claims were not read"
            ),
        );
    }

    let mut loaded = Vec::with_capacity(names.len());
    for name in names {
        let claim = (|| {
            let file = claims
                .read_bounded(&name, MAX_MACHINE_CLAIM_BYTES)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "claim disappeared while it was being read".to_owned())?;
            if file.uid != expected_uid || file.mode & 0o077 != 0 {
                return Err(format!(
                    "claim must be owned by uid {expected_uid} and inaccessible to group/other"
                ));
            }
            let claim = serde_json::from_slice::<ManagedX11MachineClaim>(&file.bytes)
                .map_err(|error| format!("invalid claim JSON: {error}"))?;
            claim.validate(&name)?;
            Ok(claim)
        })();
        match claim {
            Ok(claim) => loaded.push(claim),
            Err(error) => {
                complete = false;
                push_claim_diagnostic(
                    &mut diagnostics,
                    &mut omitted_diagnostics,
                    format!("{name:?}: {}", bounded_record_diagnostic(&error)),
                );
            }
        }
    }
    if omitted_diagnostics > 0 {
        diagnostics.push(format!(
            "{omitted_diagnostics} additional X11 machine claim diagnostics were omitted"
        ));
    }
    MachineClaimCatalog {
        claims: loaded,
        diagnostics,
        complete,
    }
}

pub(super) fn push_claim_diagnostic(
    diagnostics: &mut Vec<String>,
    omitted: &mut usize,
    message: String,
) {
    if diagnostics.len() < MAX_GRANT_DIAGNOSTICS.saturating_sub(1) {
        diagnostics.push(message);
    } else {
        *omitted += 1;
    }
}

pub(super) fn push_grant_diagnostic(
    diagnostics: &mut Vec<String>,
    omitted: &mut usize,
    message: String,
) {
    if diagnostics.len() < MAX_GRANT_DIAGNOSTICS.saturating_sub(1) {
        diagnostics.push(message);
    } else {
        *omitted += 1;
    }
}
pub(super) fn bounded_record_diagnostic(message: &str) -> String {
    const MAX_BYTES: usize = 512;
    let mut rendered = String::new();
    for character in message.chars() {
        let escaped = character.escape_default().to_string();
        if rendered.len().saturating_add(escaped.len()) > MAX_BYTES {
            rendered.push_str("...");
            break;
        }
        rendered.push_str(&escaped);
    }
    rendered
}
