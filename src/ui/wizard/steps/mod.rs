pub mod basic_view;
pub mod copy_select_view;
pub mod deploy_view;
pub mod devices_view;
pub mod network_view;
pub mod passthrough_view;
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
}

pub fn build_view(step: WizardStep, context: &WizardContext) -> Box<dyn StepComponent> {
    match step {
        WizardStep::Source => Box::new(source_view::SourceStepView::new(
            &context.source.extract_config(),
        )),

        WizardStep::CopySelect => Box::new(copy_select_view::CopySelectStepView::new(
            &context.entries,
            context.source.copy_idx,
        )),

        WizardStep::Basic => Box::new(basic_view::BasicStepView::new(
            &context.basic.extract_config(),
        )),

        WizardStep::Storage => Box::new(storage_view::StorageStepView::new(&context.storage)),

        WizardStep::User => Box::new(user_view::UserStepView::new(&context.user.extract_config())),

        WizardStep::Network => Box::new(network_view::NetworkStepView::new(
            &context.network.extract_config(),
            &context.network.bridge_list,
            &context.network.physical_interfaces,
        )),

        WizardStep::Passthrough => Box::new(passthrough_view::PassthroughStepView::new(
            &context
                .passthrough
                .extract_config(context.network.network_mode()),
            context.network.network_mode(),
            context.passthrough.nvidia_toolkit_installed,
        )),

        WizardStep::Devices => Box::new(devices_view::DevicesStepView::new(
            &context
                .passthrough
                .extract_config(context.network.network_mode()),
        )),

        WizardStep::Review => Box::new(review_view::ReviewStepView::new(
            context.build_preview_nspawn(),
        )),

        WizardStep::Deploy => Box::new(deploy_view::DeployStepView::new(
            context.deploy.log_tx.clone(),
            context.deploy.done.clone(),
            context.deploy.success.clone(),
        )),
    }
}
