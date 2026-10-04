use std::path::Path;

use devd::config::{ConfigError, ConfigLoader, ConfigValidationError};
use devd::core::dependency::DependencyError;

#[tokio::test]
async fn test_config_load_accepts_valid_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/valid-config.yml");
    let config = ConfigLoader::new().load(path).await.unwrap();
    assert_eq!(config.services.len(), 2);
    assert_eq!(config.services["api"].depends_on[0].service, "db");
}

#[tokio::test]
async fn test_config_load_rejects_dependency_cycle_with_file_context() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cyclic-config.yml");
    let error = ConfigLoader::new().load(&path).await.unwrap_err();
    assert!(error.to_string().contains("cyclic-config.yml"));
    match error {
        ConfigError::Validation {
            path: actual,
            source:
                ConfigValidationError::Dependency(DependencyError::CircularDependency {
                    path: cycle,
                    remaining,
                }),
        } => {
            assert_eq!(actual, path);
            assert_eq!(cycle, ["api", "worker", "api"]);
            assert_eq!(remaining, ["api", "worker"]);
        }
        error => panic!("expected a validation error, got {error}"),
    }
}
