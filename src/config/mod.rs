mod loader;
mod profile;
mod schema;
mod validation;

pub use loader::{ConfigError, ConfigLoader};
pub use profile::{validate_profile_name, ProfileError};
pub(crate) use schema::default_dependency_timeout;
pub(crate) use schema::parse_duration;
pub(crate) use schema::ResourceThresholds;
pub use schema::{
    BackoffType, Dependency, DependencyCondition, DevdConfig, HealthCheck, PathRequirement,
    PathRequirementType, PathScope, ResourceLimitAction, ResourceLimits, RestartPolicy,
    RestartPolicyType, RuntimePath, ServiceConfig, ServiceKind,
};
pub use validation::ConfigValidationError;
