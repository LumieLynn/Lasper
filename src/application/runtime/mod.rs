//! Runtime queries and the application-owned observation workflow.

mod catalog;
mod contract;
mod error;
mod observation;

pub use catalog::RuntimeCatalog;
pub use contract::{RuntimeQuery, RuntimeUpdate};
pub use error::{RuntimeError, RuntimeResult};
pub use observation::RuntimeObservation;

pub(crate) use contract::{RuntimeChangePort, RuntimePort};
pub(crate) use observation::RuntimeObservers;

#[cfg(test)]
pub(crate) use contract::MockRuntimePort;
