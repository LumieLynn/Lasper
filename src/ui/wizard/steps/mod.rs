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

pub trait StepComponent: Component {
    fn commit_to_context(&self, ctx: &mut WizardContext);
}

