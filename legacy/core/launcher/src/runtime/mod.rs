use axial_minecraft::{JavaRuntimeInfo, ManagedRuntimeLaunchReceipt, RuntimeSource};

#[derive(Debug, Clone)]
pub struct RuntimeSelection {
    pub effective_path: String,
    pub effective_info: JavaRuntimeInfo,
    pub effective_source: RuntimeSource,
    pub managed_launch: Option<ManagedRuntimeLaunchReceipt>,
}
