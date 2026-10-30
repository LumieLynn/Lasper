//! Resolve operation records and consumers of one exact ACL key. Read-only
//! assessment uses a snapshot; revoke and lifecycle cleanup rebuild this view
//! under the ACL lock before changing the ACL or finalizing shared records.

use crate::application::x11::{
    X11GrantRecordCatalog, X11GrantRecordEvidence, X11GrantRecordPhase, X11RevokeRequest,
};

use super::state::{MachineClaimCatalog, ManagedX11GrantKey, ManagedX11MachineClaim};

pub(super) struct GrantOwnership<'a> {
    pub(super) records: Vec<&'a X11GrantRecordEvidence>,
    pub(super) claims: Vec<&'a ManagedX11MachineClaim>,
    uncovered_records: Vec<&'a str>,
    blockers: Vec<String>,
}

impl<'a> GrantOwnership<'a> {
    pub(super) fn inspect(
        records: &'a X11GrantRecordCatalog,
        claims: &'a MachineClaimCatalog,
        key: &ManagedX11GrantKey,
        boot_id: &str,
    ) -> Result<Self, String> {
        if !records.complete || !claims.complete {
            return Err("X11 grant or machine claim catalog is incomplete".into());
        }
        let mut confirmed = Vec::new();
        let mut blockers = Vec::new();
        for record in &records.records {
            if record.boot_id != boot_id || ManagedX11GrantKey::from_evidence(record) != *key {
                continue;
            }
            match &record.phase {
                X11GrantRecordPhase::ConfirmedAdded => confirmed.push(record),
                X11GrantRecordPhase::Revoked => {}
                X11GrantRecordPhase::Pending | X11GrantRecordPhase::OutcomeUnknown { .. } => {
                    blockers.push(format!(
                        "grant {} has an unresolved ACL operation for the same key",
                        record.record_id
                    ));
                }
            }
        }
        if confirmed.is_empty() {
            return Err(
                "no confirmed grant record matches the current ACL key and host boot".into(),
            );
        }
        let consumers = claims
            .claims
            .iter()
            .filter(|claim| {
                claim.boot_id == boot_id && claim.key == *key && claim.phase.requires_reconcile()
            })
            .collect::<Vec<_>>();
        for claim in &consumers {
            if !confirmed
                .iter()
                .any(|record| record.record_id == claim.grant_record_id)
            {
                blockers.push(format!(
                    "claim {} has no matching confirmed grant record",
                    claim.claim_id
                ));
            }
        }
        // Ended claims still explain retired records after an explicit
        // handoff. Their history is finalized with the shared ACL, not dropped
        // merely because another operation record became the consumer's owner.
        let uncovered_records = confirmed
            .iter()
            .filter(|record| {
                !claims.claims.iter().any(|claim| {
                    claim.boot_id == boot_id
                        && claim.key == *key
                        && claim.grant_record_id == record.record_id
                })
            })
            .map(|record| record.record_id.as_str())
            .collect();
        Ok(Self {
            records: confirmed,
            claims: consumers,
            uncovered_records,
            blockers,
        })
    }

    pub(super) fn require_lifecycle_cleanup(&self, entry_present: bool) -> Result<(), String> {
        self.require_resolved_operations()?;
        self.require_covered_records(None)?;
        // Ended history permits finishing a partially saved revocation when
        // the entry is absent. It cannot justify deleting a reappeared entry.
        if entry_present && self.claims.is_empty() {
            return Err("the ACL entry is present, but all its machine claims have ended; automatic cleanup cannot establish current ownership".into());
        }
        Ok(())
    }

    /// Completing every record for an absent ACL has the same consumer scope
    /// as explicitly revoking it. Absence alone does not end other consumers.
    pub(super) fn require_exclusive_consumer(
        &self,
        request: &X11RevokeRequest,
    ) -> Result<(), String> {
        // The caller has already matched the selected record to this request.
        // That permits explicitly revoking a legacy record without a claim,
        // but cannot account for another record's missing lifecycle evidence.
        self.require_resolved_operations()?;
        self.require_covered_records(Some(request.record_id()))?;
        for claim in &self.claims {
            if !self.matches_consumer(claim, request) {
                return Err(format!(
                    "X11 ACL entry is shared with unresolved machine claim {} for {}; stop its consumers and let lifecycle cleanup revoke the shared entry",
                    claim.claim_id, claim.machine
                ));
            }
        }
        Ok(())
    }

    /// Matching an explicit request is separate from permission to remove the
    /// shared ACL. Both inspection and execution use these same checks.
    pub(super) fn require_request(
        &self,
        request: &X11RevokeRequest,
        caller_uid: u32,
    ) -> Result<(), String> {
        let record = self
            .records
            .iter()
            .find(|record| record.record_id == request.record_id())
            .ok_or_else(|| "the selected grant is not a current confirmed record".to_owned())?;
        let projection = request.projection();
        let socket = projection.host_socket();
        if record.caller_uid != caller_uid {
            return Err("X11 grant record belongs to another invoking user".into());
        }
        let original =
            record.target == *request.target() && record.identity.matches(projection.identity());
        let current = self.claims.iter().any(|claim| {
            claim.grant_record_id == record.record_id && self.matches_consumer(claim, request)
        });
        if !(original || current) {
            return Err("X11 grant record does not match the current machine instance".into());
        }
        if record.display != socket.display()
            || record.alternate_endpoint != socket.alternate()
            || record.source != socket.source()
            || record.canonical_source != socket.canonical_path()
            || record.socket_revision != socket.revision()
            || record.server_peer != socket.peer_identity().legacy_tuple()
        {
            return Err("the selected X11 endpoint differs from the grant record; check the recorded endpoint before revoking".into());
        }
        Ok(())
    }

    /// Only claims proven to describe this request's consumer may be ended by
    /// an AlreadyAbsent result. Other consumers and uncertain states survive.
    pub(super) fn consumer_claims(
        &self,
        request: &X11RevokeRequest,
    ) -> Vec<&'a ManagedX11MachineClaim> {
        self.claims
            .iter()
            .copied()
            .filter(|claim| self.matches_consumer(claim, request))
            .collect()
    }

    fn matches_consumer(&self, claim: &ManagedX11MachineClaim, request: &X11RevokeRequest) -> bool {
        self.records
            .iter()
            .find(|record| record.record_id == claim.grant_record_id)
            .and_then(|record| {
                claim.active_evidence(record).map(|evidence| {
                    evidence.matches(record, request.target(), request.projection().identity())
                })
            })
            .unwrap_or(false)
    }

    fn require_resolved_operations(&self) -> Result<(), String> {
        match self.blockers.first() {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }

    fn require_covered_records(&self, explicit_record: Option<&str>) -> Result<(), String> {
        if let Some(record) = self
            .uncovered_records
            .iter()
            .find(|record| Some(**record) != explicit_record)
        {
            return Err(format!("confirmed grant {record} has no machine claim; automatic cleanup cannot establish its lifecycle"));
        }
        Ok(())
    }
}
