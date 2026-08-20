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

use crate::ui::core::Component;
use crate::ui::wizard::context::WizardContext;
use crate::ui::wizard::WizardStep;

pub trait StepComponent: Component {
    fn commit_to_context(&self, ctx: &mut WizardContext);
    fn render_step(
        &mut self,
        f: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        context: &WizardContext,
    );
    fn handle_message(
        &mut self,
        _msg: &crate::ui::core::AppMessage,
    ) -> crate::ui::wizard::StepAction {
        crate::ui::wizard::StepAction::None
    }
}

pub fn build_view(step: WizardStep, context: &WizardContext) -> Box<dyn StepComponent> {
    match step {
        WizardStep::Source => Box::new(source_view::SourceStepView::new(&context.source)),

        WizardStep::CopySelect => Box::new(copy_select_view::CopySelectStepView::new(
            &context.images,
            context.source.copy_idx,
        )),

        WizardStep::Basic => Box::new(basic_view::BasicStepView::new(
            &context.basic.extract_config(),
            &context.entries,
            context.source.kind != crate::ui::wizard::context::SourceKind::Oci,
        )),

        WizardStep::Storage => Box::new(storage_view::StorageStepView::new(&context.storage)),

        WizardStep::User => Box::new(user_view::UserStepView::new(&context.user.extract_config())),

        WizardStep::Network => Box::new(network_view::NetworkStepView::new(
            &context.network.extract_config(),
            &context.network.bridge_list,
            &context.network.physical_interfaces,
        )),

        WizardStep::HostIntegration => {
            Box::new(host_integration_view::HostIntegrationStepView::new(
                &context.passthrough.extract_config(),
                context.network.network_mode(),
                context.passthrough.wayland_sockets.clone(),
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
            context.build_preview_nspawn(),
        )),

        WizardStep::Deploy => {
            let rx = context
                .deploy
                .log_rx
                .borrow_mut()
                .take()
                .unwrap_or_else(|| context.deploy.log_tx.subscribe());
            Box::new(deploy_view::DeployStepView::new(
                rx,
                context.deploy.done.clone(),
                context.deploy.success.clone(),
                context.deploy.cancelled.clone(),
                context.deploy.rolling_back.clone(),
                context.deploy.cancellation.clone(),
            ))
        }
    }
}
