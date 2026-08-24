//! In-process resource coordination for application operations.
//!
//! The registry is deliberately small and semantic.  It is not a replacement
//! for systemd's locks; it prevents Lasper from submitting conflicting work
//! while a request is still active and gives presentation a stable operation
//! projection.

use crate::nspawn::models::{ImageName, MachineName};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    Nspawn(String),
    Image(String),
}

impl ResourceKey {
    pub fn for_machine(name: &MachineName) -> Self {
        Self::Nspawn(name.as_str().to_string())
    }

    pub fn for_image(name: &ImageName) -> Self {
        // A regular image whose name is also a valid machine name resolves to
        // the same nspawn resource as a machine start. Hidden/non-machine
        // images remain independent image resources.
        MachineName::new(name.as_str())
            .map(|machine| Self::for_machine(&machine))
            .unwrap_or_else(|_| Self::Image(name.as_str().to_string()))
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceAccess {
    Shared,
    Exclusive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceClaim {
    pub key: ResourceKey,
    pub access: ResourceAccess,
}

impl ResourceClaim {
    pub fn shared(key: ResourceKey) -> Self {
        Self {
            key,
            access: ResourceAccess::Shared,
        }
    }

    pub fn exclusive(key: ResourceKey) -> Self {
        Self {
            key,
            access: ResourceAccess::Exclusive,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceConflict {
    pub key: ResourceKey,
}

#[derive(Default)]
struct RegistryState {
    claims: HashMap<ResourceKey, Vec<(ResourceAccess, u64)>>,
    image_removals: HashMap<String, u64>,
}

/// A reservation guard releases every claim when dropped.
pub struct ResourceReservation {
    registry: Arc<OperationRegistry>,
    id: u64,
    keys: Vec<ResourceKey>,
}

impl std::fmt::Debug for ResourceReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceReservation")
            .field("id", &self.id)
            .field("keys", &self.keys)
            .finish()
    }
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        self.registry.release(self.id, &self.keys);
    }
}

/// Atomic, in-memory claim registry shared by application services and views.
#[derive(Default)]
pub struct OperationRegistry {
    next_id: AtomicU64,
    state: Mutex<RegistryState>,
}

impl OperationRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn reserve(
        self: &Arc<Self>,
        claims: impl IntoIterator<Item = ResourceClaim>,
    ) -> Result<ResourceReservation, ResourceConflict> {
        self.reserve_inner(claims, None)
    }

    pub fn reserve_image_removal(
        self: &Arc<Self>,
        image_name: &str,
        claims: impl IntoIterator<Item = ResourceClaim>,
    ) -> Result<ResourceReservation, ResourceConflict> {
        self.reserve_inner(claims, Some(image_name))
    }

    fn reserve_inner(
        self: &Arc<Self>,
        claims: impl IntoIterator<Item = ResourceClaim>,
        image_removal: Option<&str>,
    ) -> Result<ResourceReservation, ResourceConflict> {
        let claims = canonicalize(claims);
        let mut state = self.state.lock();
        if let Some(conflict) = claims.iter().find_map(|claim| {
            state
                .claims
                .get(&claim.key)
                .filter(|holders| {
                    holders.iter().any(|(held_access, _)| {
                        *held_access == ResourceAccess::Exclusive
                            || claim.access == ResourceAccess::Exclusive
                    })
                })
                .map(|_| ResourceConflict {
                    key: claim.key.clone(),
                })
        }) {
            return Err(conflict);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        for claim in &claims {
            state
                .claims
                .entry(claim.key.clone())
                .or_default()
                .push((claim.access, id));
        }
        if let Some(image_name) = image_removal {
            state.image_removals.insert(image_name.to_string(), id);
        }
        Ok(ResourceReservation {
            registry: Arc::clone(self),
            id,
            keys: claims.into_iter().map(|claim| claim.key).collect(),
        })
    }

    #[allow(dead_code)]
    pub fn is_held(&self, key: &ResourceKey) -> bool {
        self.state.lock().claims.contains_key(key)
    }

    pub fn active_image_names(&self) -> HashSet<String> {
        self.state.lock().image_removals.keys().cloned().collect()
    }

    fn release(&self, id: u64, keys: &[ResourceKey]) {
        let mut state = self.state.lock();
        for key in keys {
            if let Some(holders) = state.claims.get_mut(key) {
                holders.retain(|(_, owner)| *owner != id);
                if holders.is_empty() {
                    state.claims.remove(key);
                }
            }
        }
        state.image_removals.retain(|_, owner| *owner != id);
    }
}

fn canonicalize(claims: impl IntoIterator<Item = ResourceClaim>) -> Vec<ResourceClaim> {
    let mut canonical = HashMap::<ResourceKey, ResourceAccess>::new();
    for claim in claims {
        canonical
            .entry(claim.key)
            .and_modify(|access| {
                if claim.access == ResourceAccess::Exclusive {
                    *access = ResourceAccess::Exclusive;
                }
            })
            .or_insert(claim.access);
    }
    let mut claims = canonical
        .into_iter()
        .map(|(key, access)| ResourceClaim { key, access })
        .collect::<Vec<_>>();
    claims.sort_by(|left, right| format!("{:?}", left.key).cmp(&format!("{:?}", right.key)));
    claims
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_and_machine_share_regular_machine_resource() {
        let image = ImageName::new("ubuntu").unwrap();
        let machine = MachineName::new("ubuntu").unwrap();
        assert_eq!(
            ResourceKey::for_image(&image),
            ResourceKey::for_machine(&machine)
        );
    }

    #[test]
    fn non_machine_images_keep_an_independent_resource() {
        let image = ImageName::new("Ubuntu Resolute image").unwrap();
        assert_eq!(
            ResourceKey::for_image(&image),
            ResourceKey::Image("Ubuntu Resolute image".into())
        );
    }

    #[test]
    fn exclusive_reservation_rejects_same_resource_and_releases_on_drop() {
        let registry = OperationRegistry::new();
        let key = ResourceKey::Nspawn("ubuntu".into());
        let first = registry
            .reserve([ResourceClaim::exclusive(key.clone())])
            .unwrap();
        assert!(registry
            .reserve([ResourceClaim::exclusive(key.clone())])
            .is_err());
        drop(first);
        assert!(registry.reserve([ResourceClaim::exclusive(key)]).is_ok());
    }

    #[test]
    fn failed_multi_claim_reservation_does_not_partially_hold() {
        let registry = OperationRegistry::new();
        let held = ResourceKey::Nspawn("held".into());
        let free = ResourceKey::Nspawn("free".into());
        let _guard = registry
            .reserve([ResourceClaim::exclusive(held.clone())])
            .unwrap();
        assert!(registry
            .reserve([
                ResourceClaim::exclusive(held.clone()),
                ResourceClaim::exclusive(free.clone()),
            ])
            .is_err());
        assert!(!registry.is_held(&free));
    }

    #[test]
    fn shared_claims_coexist_but_exclusive_conflicts() {
        let registry = OperationRegistry::new();
        let key = ResourceKey::Nspawn("base".into());
        let first = registry
            .reserve([ResourceClaim {
                key: key.clone(),
                access: ResourceAccess::Shared,
            }])
            .unwrap();
        let second = registry
            .reserve([ResourceClaim {
                key: key.clone(),
                access: ResourceAccess::Shared,
            }])
            .unwrap();
        assert!(registry
            .reserve([ResourceClaim::exclusive(key.clone())])
            .is_err());
        drop((first, second));
        assert!(registry.reserve([ResourceClaim::exclusive(key)]).is_ok());
    }

    #[test]
    fn only_removal_reservations_enter_the_removing_projection() {
        let registry = OperationRegistry::new();
        let key = ResourceKey::Nspawn("ubuntu".into());
        let start = registry
            .reserve([ResourceClaim::exclusive(key.clone())])
            .unwrap();
        assert!(registry.active_image_names().is_empty());
        drop(start);
        let removal = registry
            .reserve_image_removal("ubuntu", [ResourceClaim::exclusive(key)])
            .unwrap();
        assert!(registry.active_image_names().contains("ubuntu"));
        drop(removal);
        assert!(registry.active_image_names().is_empty());
    }
}
