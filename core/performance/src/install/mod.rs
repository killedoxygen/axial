mod artifact;
mod fallback;
mod manager;
mod model;
mod mutation;
mod promotion;
mod rules_refresh;

#[cfg(test)]
mod tests;

pub use manager::{
    ManagedCompositionAuthority, ManagedIdentityError, ManagedInstanceIdentity, PerformanceManager,
    PerformanceRulesAuthority,
};
pub use model::{InstallError, PERFORMANCE_RULES_URL_ENV, RulesRefreshError, VerifiedRemoteRules};
pub use mutation::{
    ManagedCompositionInspection, ManagedIndeterminate, ManagedMutationError,
    ManagedResolvedInspection,
};
pub use rules_refresh::remote_rules_refresh_warning;
