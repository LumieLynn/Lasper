//! Deployment intent and rules shared by every entry point and executor.

pub mod bootstrap;
pub mod oci;
pub mod source;
mod spec;
pub mod storage;
pub mod wayland;

pub use spec::{
    BootstrapDeployment, CloneDeployment, DeploymentCategory, DeploymentIntegration,
    DeploymentSpec, DeploymentStorage, DeploymentValidationError, ImageImport, ImportDeployment,
    ImportSource, OciDeployment,
};
