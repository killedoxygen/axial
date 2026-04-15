use croopor_minecraft::JavaRuntimeInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSelection {
    pub requested_path: String,
    pub selected_path: String,
    pub selected_info: JavaRuntimeInfo,
    pub effective_path: String,
    pub effective_info: JavaRuntimeInfo,
    pub effective_source: String,
    pub bypassed_requested_runtime: bool,
}
