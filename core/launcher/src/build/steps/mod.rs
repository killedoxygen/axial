#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStepKind {
    ResolvePlan,
    Validate,
    Prepare,
    Start,
    Monitor,
}
