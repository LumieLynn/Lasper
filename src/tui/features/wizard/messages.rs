//! Requests and edits produced by wizard steps and their dialogs.

use crate::application::provisioning::DeploymentId;
use crate::domain::provisioning::{BindMount, PortForward};
use crate::tui::features::wizard::dialogs::nvidia_config::NvidiaConfigResult;
use crate::tui::features::wizard::draft::{UnclassifiedFile, UserDraft, WaylandAccessDraft};

#[derive(Debug, PartialEq)]
pub enum WizardMessage {
    Submit,
    Close,
    DialogCancel,
    AcknowledgeConfigurationWarning,
    AcceptUnsafeRemoteTar,
    DeclineUnsafeRemoteTar,
    ReleaseUnresolvedDeployment(DeploymentId),
    UnresolvedDeploymentReleaseFinished {
        deployment_id: DeploymentId,
        error: Option<String>,
    },

    // Dialog open requests (add = blank, edit = pre-filled)
    OpenUserDialog,
    OpenUserEditDialog(usize, UserDraft),
    OpenPortDialog,
    OpenPortEditDialog(usize, PortForward),
    OpenBindDialog,
    OpenBindEditDialog(usize, BindMount),
    OpenNvidiaConfigDialog,
    NvidiaConfigSaved(NvidiaConfigResult),
    WaylandAccessConfigured(WaylandAccessDraft),
    OpenUnclassifiedEditDialog(usize, UnclassifiedFile),

    // Macro-events for atomic data changes
    UserAdded(UserDraft),
    UserUpdated(usize, UserDraft),
    UserRemoved(usize),
    PortForwardAdded(PortForward),
    PortForwardUpdated(usize, PortForward),
    PortForwardRemoved(usize),
    BindMountAdded(BindMount),
    BindMountUpdated(usize, BindMount),
    BindMountRemoved(usize),
    UnclassifiedFileUpdated(usize, UnclassifiedFile),
}
