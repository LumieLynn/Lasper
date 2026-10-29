//! Direct deployment execution shared by normal mode and daemon-owned jobs.
//!
//! `execution` defines typed inputs. `transaction` applies them and owns cleanup
//! and durable finalization. `sources` keeps each provider's policy, invocation
//! and artifact handling together.
//!
//! `host` supplies host operations; `resources` records their ownership.
//! `guest` initializes the deployed OS without owning its storage or mount lifetime.
//! `command` waits for provider completion; `diagnostics` logs and publishes events.
//! `failure` retains primary failures and cleanup errors separately.

mod command;
mod diagnostics;
mod execution;
mod failure;
mod guest;
mod host;
mod resources;
pub(super) mod sources;
mod transaction;

pub(super) use diagnostics::DeploymentDiagnostics;
pub(super) use execution::{DeploymentExecution, DeploymentMethod, SystemConfiguration};
pub(super) use failure::DeploymentFailure;
pub(super) use host::DirectProvisioningCapabilities;
pub(super) use transaction::run_deployment;
