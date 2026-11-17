//! Hello messages, negotiated capacities and connection header validation.
//! WireSessionState handles the execution exchange after Hello.

mod bootstrap;
mod connection;
mod hello;
mod limits;

pub use bootstrap::{
    BootstrapManifest, BootstrapMessageSchema, BootstrapSchemaId, BOOTSTRAP_MANIFEST,
};
pub use connection::{ControlConnectionState, ControlSessionError};
pub use hello::{BrokerId, HelloRejected, HelloRequest, HelloResponse};
pub use limits::{
    ControlLimitError, ControlLimits, EnforcedControlLimits, HELLO_CONTROL_FRAME_BYTES,
    HELLO_CONTROL_LIMITS, HELLO_PAYLOAD_BUFFER_BYTES, MIN_NEGOTIATED_CONTROL_FRAME_BYTES,
};
