use std::path::{Path, PathBuf};

use thiserror::Error;

use super::schema::DevdConfig;
use super::validation::ConfigValidationError;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("invalid configuration file {path}: {source}")]
    Validation {
        path: PathBuf,
        #[source]
        source: ConfigValidationError,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ConfigLoader;

impl ConfigLoader {
    pub fn new() -> Self {
        Self
    }

    /// Load, deserialize, and validate a YAML configuration file.
    pub async fn load<P>(&self, path: P) -> Result<DevdConfig, ConfigError>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_path_buf();
        let contents =
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;

        let config = Self::from_str(&contents, &path)?;
        config
            .validate()
            .map_err(|source| ConfigError::Validation { path, source })?;
        Ok(config)
    }

    /// Deserialize YAML without semantic validation. Call `DevdConfig::validate`
    /// before using the configuration to start services.
    pub fn from_str(contents: &str, path: impl Into<PathBuf>) -> Result<DevdConfig, ConfigError> {
        let path = path.into();
        serde_yaml::from_str(contents).map_err(|source| ConfigError::Parse { path, source })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;
    use crate::config::{BackoffType, DependencyCondition, HealthCheck, RestartPolicyType};

    const MINIMAL_CONFIG: &str = r#"
version: "1"
services:
  api:
    command: cargo run
"#;

    #[test]
    fn test_config_loader_from_str_with_minimal_service() {
        let config = ConfigLoader::from_str(MINIMAL_CONFIG, "devd.yml").unwrap();

        assert_eq!(config.version, "1");
        assert!(config.services.contains_key("api"));
        assert_eq!(config.services["api"].command, "cargo run");
        assert!(config.services["api"].depends_on.is_empty());
        assert!(config.services["api"].healthcheck.is_none());
    }

    #[test]
    fn test_config_loader_from_str_with_optional_fields() {
        let contents = r#"
version: "1"
services:
  postgres:
    command: postgres -D /tmp/postgres
    healthcheck:
      type: socket
      path: /tmp/.s.PGSQL.5432
      interval: 5s
    restart:
      policy: always
      max-attempts: 3
  backend:
    command: npm run dev
    cwd: ./backend
    env-file: .env.local
    env:
      RUST_LOG: debug
    depends-on:
      - service: postgres
        condition: socket-ready
    healthcheck:
      type: http
      url: http://localhost:3000/health
      interval: 10s
      timeout: 2s
      retries: 3
    restart:
      policy: on-failure
      backoff: exponential
      initial-delay: 1s
      max-delay: 60s
      max-attempts: 5
    limits:
      cpu: 50%
      memory: 1GB
"#;

        let config = ConfigLoader::from_str(contents, "devd.yml").unwrap();
        let postgres = &config.services["postgres"];
        let backend = &config.services["backend"];

        assert!(matches!(
            postgres.healthcheck,
            Some(HealthCheck::Socket { .. })
        ));
        assert_eq!(postgres.restart.policy, RestartPolicyType::Always);
        assert_eq!(postgres.restart.max_attempts, 3);
        assert_eq!(backend.cwd.as_deref(), Some(Path::new("./backend")));
        assert_eq!(backend.env["RUST_LOG"], "debug");
        assert_eq!(backend.depends_on[0].service, "postgres");
        assert_eq!(
            backend.depends_on[0].condition,
            DependencyCondition::SocketReady
        );
        assert!(matches!(
            backend.healthcheck,
            Some(HealthCheck::Http { .. })
        ));
        assert_eq!(backend.restart.backoff, BackoffType::Exponential);
        assert_eq!(backend.restart.max_attempts, 5);
        assert_eq!(
            backend.limits.as_ref().unwrap().memory.as_deref(),
            Some("1GB")
        );
    }

    #[test]
    fn test_config_loader_rejects_malformed_yaml() {
        let error = ConfigLoader::from_str("version: [", "broken.yml").unwrap_err();

        assert!(matches!(error, ConfigError::Parse { .. }));
        assert!(error.to_string().contains("broken.yml"));
    }

    #[test]
    fn test_config_loader_rejects_service_without_command() {
        let error = ConfigLoader::from_str(
            "version: \"1\"\nservices:\n  api: {}\n",
            "missing-command.yml",
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::Parse { .. }));
        assert!(error.to_string().contains("missing-command.yml"));
    }

    #[tokio::test]
    async fn test_config_loader_rejects_missing_file() {
        let path = tempdir().unwrap().path().join("missing.yml");
        let error = ConfigLoader::new().load(&path).await.unwrap_err();

        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("missing.yml"));
    }

    #[tokio::test]
    async fn test_config_loader_reads_yaml_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("devd.yml");
        tokio::fs::write(&path, MINIMAL_CONFIG).await.unwrap();

        let config = ConfigLoader::new().load(&path).await.unwrap();

        assert_eq!(config.services["api"].command, "cargo run");
    }
}
