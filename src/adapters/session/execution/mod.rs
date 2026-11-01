//! Guest execution providers. Display policy stays in the session resolvers;
//! providers consume process requests and own their execution resources.

mod identity;
mod native;
mod prepared;
mod readiness;

pub(crate) use identity::MachineInstanceSnapshot;
pub(crate) use native::{NativeExecution, NativeSession};
pub(crate) use prepared::{PreparedNativeCommand, PreparedNativeExecution};
