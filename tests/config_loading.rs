use std::path::Path;
use std::time::Duration;

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

#[test]
fn test_dependency_timeout_defaults_and_profile_override() {
    let yaml = r#"
version: "1"
dependency-timeout: 2m
services:
  api:
    command: sleep 60
profiles:
  fast:
    dependency-timeout: 45s
  default:
    dependency-timeout: null
"#;
    let base = ConfigLoader::from_str(yaml, "devd.yml").unwrap();
    let fast = ConfigLoader::from_str_profile(yaml, "devd.yml", Some("fast")).unwrap();
    let reset = ConfigLoader::from_str_profile(yaml, "devd.yml", Some("default")).unwrap();
    assert_eq!(base.dependency_timeout, Duration::from_secs(120));
    assert_eq!(fast.dependency_timeout, Duration::from_secs(45));
    assert_eq!(reset.dependency_timeout, Duration::from_secs(30));
}

#[test]
fn test_dependency_timeout_must_be_positive() {
    let yaml = "version: '1'\ndependency-timeout: 0s\nservices:\n  api: {command: sleep 60}\n";
    let error = ConfigLoader::from_str(yaml, "devd.yml")
        .unwrap()
        .validate()
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("dependency readiness timeout must be positive"));
}

#[test]
fn test_dependency_timeout_rejects_invalid_and_runtime_overflow_values() {
    for value in [
        "0s",
        "-1",
        "1.5",
        "null",
        "true",
        "2d",
        "18446744073709551615s",
    ] {
        let yaml = format!(
            "version: '1'\ndependency-timeout: {value}\nservices:\n  api: {{command: sleep}}\n"
        );
        let result = ConfigLoader::from_str(&yaml, "devd.yml");
        assert!(
            result
                .map(|config| config.validate().is_err())
                .unwrap_or(true),
            "{value}"
        );
    }
    let yaml = "version: '1'\ndependency_timeout: 120\nservices:\n  api: {command: sleep}\nprofiles:\n  inherited: {}\n";
    let config = ConfigLoader::from_str_profile(yaml, "devd.yml", Some("inherited")).unwrap();
    assert_eq!(config.dependency_timeout, Duration::from_secs(120));
    let duplicate = yaml.replace(
        "dependency_timeout: 120",
        "dependency_timeout: 120\ndependency-timeout: 60s",
    );
    assert!(ConfigLoader::from_str(&duplicate, "devd.yml").is_err());
}
