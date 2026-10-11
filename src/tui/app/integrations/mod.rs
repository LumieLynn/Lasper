//! App extensions for concrete features and their cross-component effects.
//!
//! Components own local state, input and presentation. These modules connect
//! their commands and completions to services, shared notices and other owners.
//! The dispatch module is the central entry from the application core.

mod configuration;
mod dispatch;
mod inspector;
mod resources;
mod terminals;
mod wizard;
mod workspace;
