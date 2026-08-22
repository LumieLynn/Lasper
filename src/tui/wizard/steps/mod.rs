pub mod basic_view;
pub mod bind_mounts_view;
pub mod copy_select_view;
pub mod deploy_view;
pub mod host_integration_view;
pub mod network_view;
pub mod review_view;
pub mod source_view;
pub mod storage_view;
pub mod user_view;

use crate::tui::core::Component;
use crate::tui::wizard::draft::WizardDraft;
use crate::tui::wizard::WizardStep;

pub trait StepComponent: Component {
    fn commit_to_draft(&self, ctx: &mut WizardDraft);
    fn render_step(
        &mut self,
        f: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        context: &WizardDraft,
    );
    fn handle_message(
        &mut self,
        _msg: &crate::tui::core::AppMessage,
    ) -> crate::tui::wizard::StepAction {
        crate::tui::wizard::StepAction::None
    }
}

pub fn build_view(
    step: WizardStep,
    context: &WizardDraft,
    preparation: std::sync::Arc<crate::application::provisioning::ProvisioningPreparationService>,
) -> Box<dyn StepComponent> {
    match step {
        WizardStep::Source => Box::new(source_view::SourceStepView::new(
            &context.source,
            context.host.oci.clone(),
            context.host.tools.clone(),
        )),

        WizardStep::CopySelect => Box::new(copy_select_view::CopySelectStepView::new(
            &context.images,
            context.source.copy_idx,
        )),

        WizardStep::Basic => Box::new(basic_view::BasicStepView::new(
            &context.basic.extract_config(),
            &context.entries,
            !matches!(
                &context.source.kind,
                crate::tui::wizard::draft::SourceKind::Copy
                    | crate::tui::wizard::draft::SourceKind::Oci
            ),
        )),

        WizardStep::Storage => Box::new(storage_view::StorageStepView::new(
            &context.storage,
            preparation.clone(),
            context.host.tools.clone(),
        )),

        WizardStep::User => Box::new(user_view::UserStepView::new(&context.user)),

        WizardStep::Network => Box::new(network_view::NetworkStepView::new(
            &context.network.extract_config(),
            &context.network.bridge_list,
            &context.network.physical_interfaces,
        )),

        WizardStep::HostIntegration => {
            Box::new(host_integration_view::HostIntegrationStepView::new(
                &context.passthrough.extract_config(),
                context.network.network_mode(),
                context.user.users.iter().any(|user| user.wayland.is_some()),
                context.passthrough.discovered_gpus.clone(),
                context.passthrough.hardware_scanning,
            ))
        }

        WizardStep::BindMounts => Box::new(bind_mounts_view::BindMountsStepView::new(
            &context.passthrough.extract_config(),
            &context.passthrough.unclassified_files,
            context.passthrough.nvidia_toolkit_installed,
        )),

        WizardStep::Review => Box::new(review_view::ReviewStepView::new(
            preparation.preview(&context.build_deployment_request()),
        )),

        WizardStep::Deploy => {
            unreachable!("the deployment view requires an application-owned job handle")
        }
    }
}
