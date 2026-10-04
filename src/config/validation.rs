use std::collections::BTreeMap;

use thiserror::Error;

use crate::core::dependency::{DependencyError, DependencyGraph};

use super::{DependencyCondition, DevdConfig, HealthCheck};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigValidationError {
    #[error("{field}: {reason}")]
    InvalidField { field: String, reason: &'static str },
    #[error(transparent)]
    Dependency(#[from] DependencyError),
}

fn invalid(field: impl Into<String>, reason: &'static str) -> ConfigValidationError {
    ConfigValidationError::InvalidField {
        field: field.into(),
        reason,
    }
}

impl DevdConfig {
    /// Check service definitions and dependency cycles without starting processes
    /// or inspecting the filesystem or network.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        if self.version != "1" {
            return Err(invalid(
                "version",
                "only configuration version '1' is supported",
            ));
        }
        if self.services.is_empty() {
            return Err(invalid("services", "at least one service is required"));
        }

        let services: BTreeMap<_, _> = self.services.iter().collect();
        for (name, service) in &services {
            let prefix = format!("services.{name}");
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
                || !name
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return Err(invalid(prefix, "service names must start with a letter, digit, or underscore and contain only ASCII letters, digits, '_', '-', or '.'"));
            }
            if service.command.trim().is_empty() || service.command.contains('\0') {
                return Err(invalid(
                    format!("{prefix}.command"),
                    "command must be non-empty and contain no NUL bytes",
                ));
            }
            if let Some(check) = &service.healthcheck {
                validate_healthcheck(&prefix, check)?;
            }
        }

        let graph = DependencyGraph::from_config(self)?;
        for (name, dependency, condition) in graph.edges() {
            let target = &self.services[dependency];
            let matches = matches!(
                (condition, &target.healthcheck),
                (DependencyCondition::Started, _)
                    | (
                        DependencyCondition::HttpReady,
                        Some(HealthCheck::Http { .. })
                    )
                    | (DependencyCondition::TcpReady, Some(HealthCheck::Tcp { .. }))
                    | (
                        DependencyCondition::SocketReady,
                        Some(HealthCheck::Socket { .. })
                    )
            );
            if !matches {
                return Err(invalid(
                    format!("services.{name}.depends-on.{dependency}"),
                    "readiness condition requires a matching healthcheck on the dependency service",
                ));
            }
        }
        graph.validate_acyclic()?;
        Ok(())
    }
}

fn validate_healthcheck(prefix: &str, check: &HealthCheck) -> Result<(), ConfigValidationError> {
    let prefix = format!("{prefix}.healthcheck");
    let (interval, timeout, retries) = match check {
        HealthCheck::Http {
            url,
            interval,
            timeout,
            retries,
        } => {
            let valid = reqwest::Url::parse(url).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            });
            if !valid {
                return Err(invalid(
                    format!("{prefix}.url"),
                    "expected an absolute HTTP or HTTPS URL with a host",
                ));
            }
            (interval, timeout, retries)
        }
        HealthCheck::Tcp {
            host,
            port,
            interval,
            timeout,
            retries,
        } => {
            if host.trim().is_empty()
                || host
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
            {
                return Err(invalid(
                    format!("{prefix}.host"),
                    "host must be non-empty and contain no whitespace or control characters",
                ));
            }
            if *port == 0 {
                return Err(invalid(
                    format!("{prefix}.port"),
                    "port must be between 1 and 65535",
                ));
            }
            (interval, timeout, retries)
        }
        HealthCheck::Socket {
            path,
            interval,
            timeout,
            retries,
        } => {
            if path.as_os_str().is_empty() {
                return Err(invalid(
                    format!("{prefix}.path"),
                    "socket path must be non-empty",
                ));
            }
            (interval, timeout, retries)
        }
    };
    if interval.is_zero() {
        return Err(invalid(
            format!("{prefix}.interval"),
            "interval must be greater than zero",
        ));
    }
    if timeout.is_zero() {
        return Err(invalid(
            format!("{prefix}.timeout"),
            "timeout must be greater than zero",
        ));
    }
    if *retries == 0 {
        return Err(invalid(
            format!("{prefix}.retries"),
            "retries must be greater than zero",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigLoader, Dependency};

    fn validate(services: &str) -> Result<(), ConfigValidationError> {
        ConfigLoader::from_str(&format!("version: '1'\nservices:\n{services}"), "test.yml")
            .unwrap()
            .validate()
    }

    #[test]
    fn test_config_validation_accepts_acyclic_dependencies() {
        validate("  db: {command: db}\n  cache: {command: cache}\n  api: {command: api, depends-on: [db, cache]}\n  web: {command: web, depends-on: [api]}\n").unwrap();
    }

    #[test]
    fn test_config_validation_rejects_unknown_dependency() {
        assert_eq!(
            validate("  api: {command: api, depends-on: [missing]}\n"),
            Err(ConfigValidationError::Dependency(
                DependencyError::UnknownDependency {
                    service: "api".into(),
                    dependency: "missing".into()
                }
            ))
        );
    }

    #[test]
    fn test_config_validation_rejects_duplicate_dependency() {
        assert_eq!(
            validate("  db: {command: db}\n  api: {command: api, depends-on: [db, db]}\n"),
            Err(ConfigValidationError::Dependency(
                DependencyError::DuplicateDependency {
                    service: "api".into(),
                    dependency: "db".into()
                }
            ))
        );
    }

    #[test]
    fn test_dependency_validation_rejects_self_cycle() {
        assert_eq!(
            validate("  api: {command: api, depends-on: [api]}\n"),
            Err(ConfigValidationError::Dependency(
                DependencyError::CircularDependency {
                    path: vec!["api".into(), "api".into()],
                    remaining: vec!["api".into()]
                }
            ))
        );
    }

    #[test]
    fn test_dependency_validation_reports_closed_cycle_without_blocked_services() {
        let error = validate("  a-blocked: {command: blocked, depends-on: [b]}\n  b: {command: b, depends-on: [c]}\n  c: {command: c, depends-on: [d]}\n  d: {command: d, depends-on: [b]}\n  independent: {command: independent}\n").unwrap_err();
        assert_eq!(
            error,
            ConfigValidationError::Dependency(DependencyError::CircularDependency {
                path: vec!["b".into(), "c".into(), "d".into(), "b".into()],
                remaining: vec!["a-blocked".into(), "b".into(), "c".into(), "d".into()]
            })
        );
        assert_eq!(
            error.to_string(),
            "circular dependency detected: b -> c -> d -> b; unresolved services: a-blocked, b, c, d"
        );
    }

    #[test]
    fn test_config_validation_rejects_empty_services_and_unsupported_version() {
        for (yaml, field) in [
            ("version: '1'\nservices: {}", "services"),
            ("version: '2'\nservices: {}", "version"),
        ] {
            let error = ConfigLoader::from_str(yaml, "test.yml")
                .unwrap()
                .validate()
                .unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field: actual, .. } if actual == field)
            );
        }
    }

    #[test]
    fn test_config_validation_checks_names_and_commands() {
        for name in ["", "white space", "../db", "-db", "db/one", "数据库"] {
            assert!(matches!(
                validate(&format!("  '{name}': {{command: db}}\n")),
                Err(ConfigValidationError::InvalidField { .. })
            ));
        }
        validate("  API_1.db-main: {command: db}\n").unwrap();
        for command in ["''", "'  '", "\"bad\\0command\""] {
            let error = validate(&format!("  api: {{command: {command}}}\n")).unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field, .. } if field == "services.api.command")
            );
        }
    }

    #[test]
    fn test_config_validation_checks_healthcheck_fields() {
        for (fields, field) in [
            ("type: http, url: ''", "url"),
            ("type: http, url: /health", "url"),
            ("type: http, url: 'ftp://localhost/health'", "url"),
            ("type: tcp, host: '', port: 80", "host"),
            ("type: tcp, host: 'bad host', port: 80", "host"),
            ("type: tcp, port: 0", "port"),
            ("type: socket, path: ''", "path"),
            (
                "type: http, url: 'http://localhost/health', interval: 0ms",
                "interval",
            ),
            ("type: tcp, port: 80, timeout: 0", "timeout"),
            ("type: socket, path: /tmp/db.sock, retries: 0", "retries"),
        ] {
            let error = validate(&format!(
                "  api:\n    command: api\n    healthcheck: {{{fields}}}\n"
            ))
            .unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field: actual, .. } if actual == format!("services.api.healthcheck.{field}")),
                "invalid healthcheck: {fields}"
            );
        }
    }

    #[test]
    fn test_config_validation_accepts_valid_healthchecks_and_readiness() {
        validate("  db: {command: db, healthcheck: {type: socket, path: /tmp/db.sock}}\n  cache: {command: cache, healthcheck: {type: tcp, host: '::1', port: 6379}}\n  api:\n    command: api\n    healthcheck: {type: http, url: 'https://localhost:3000/health'}\n    depends-on:\n      - {service: db, condition: socket-ready}\n      - {service: cache, condition: tcp-ready}\n  web:\n    command: web\n    depends-on: [{service: api, condition: http-ready}]\n").unwrap();
    }

    #[test]
    fn test_config_validation_rejects_missing_or_mismatched_readiness_probe() {
        for healthcheck in ["", ", healthcheck: {type: tcp, port: 80}"] {
            let error = validate(&format!("  db: {{command: db{healthcheck}}}\n  api: {{command: api, depends-on: [{{service: db, condition: http-ready}}]}}\n")).unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field, .. } if field == "services.api.depends-on.db")
            );
        }
    }

    #[test]
    fn test_config_validation_errors_are_independent_of_insertion_order() {
        let first = validate("  z: {command: ''}\n  a: {command: ''}\n").unwrap_err();
        let second = validate("  a: {command: ''}\n  z: {command: ''}\n").unwrap_err();
        assert_eq!(first, second);
        assert!(
            matches!(first, ConfigValidationError::InvalidField { field, .. } if field == "services.a.command")
        );
    }

    #[test]
    fn test_dependency_validation_handles_long_chain_without_recursion() {
        let mut config = ConfigLoader::from_str(
            "version: '1'\nservices:\n  base: {command: base}\n",
            "test.yml",
        )
        .unwrap();
        let template = config.services["base"].clone();
        let mut previous = "base".to_owned();
        for index in 0..2000 {
            let name = format!("service-{index}");
            let mut service = template.clone();
            service.depends_on.push(Dependency {
                service: previous,
                condition: DependencyCondition::Started,
            });
            config.services.insert(name.clone(), service);
            previous = name;
        }
        config.validate().unwrap();
        config
            .services
            .get_mut("base")
            .unwrap()
            .depends_on
            .push(Dependency {
                service: previous,
                condition: DependencyCondition::Started,
            });
        assert!(
            matches!(config.validate(), Err(ConfigValidationError::Dependency(DependencyError::CircularDependency { path, .. })) if path.len() == 2002)
        );
    }
}
