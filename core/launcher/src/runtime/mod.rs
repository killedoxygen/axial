use axial_minecraft::JavaRuntimeInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSelection {
    pub effective_path: String,
    pub effective_info: JavaRuntimeInfo,
    pub effective_source: String,
}
