//! Native system-bus backend.
//!
//! Connection generations and deadlines are shared by read-only queries,
//! control requests, guest sessions, and change subscriptions. The protocol
//! module contains only the machine1 wire signatures.

mod connection;
mod control;
mod events;
mod protocol;
mod query;
mod session;

use crate::adapters::error::{NspawnError, Result};
use crate::domain::machine::MachineName;
use std::sync::Arc;
use tokio::sync::Mutex;
use zbus::Connection;

use connection::ConnectionCache;

#[derive(Clone)]
pub struct DbusBackend {
    conn: Arc<Mutex<ConnectionCache<Connection>>>,
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}
