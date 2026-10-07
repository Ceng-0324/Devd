use std::collections::BTreeMap;

use thiserror::Error;

use crate::core::dependency::{DependencyError, DependencyGraph};

use super::{BackoffType, DependencyCondition, DevdConfig, HealthCheck, RestartPolicyType};

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
            if service.listen.iter().any(|address| address.port() == 0) {
                return Err(invalid(
                    format!("{prefix}.listen"),
                    "listening port must be between 1 and 65535",
                ));
            }
            for (index, requirement) in service.requires.iter().enumerate() {
                if requirement.path.as_os_str().is_empty()
                    || requirement.path.to_string_lossy().contains('\0')
                {
                    return Err(invalid(
                        format!("{prefix}.requires[{index}].path"),
                        "path must be non-empty and contain no NUL bytes",
                    ));
                }
            }
            if let Some(limits) = &service.limits {
                if limits.on_exceed == super::ResourceLimitAction::Restart
                    && service.restart.policy == RestartPolicyType::Never
                {
                    return Err(invalid(
                        format!("{prefix}.limits.on-exceed"),
                        "restart requires an automatic restart policy (on-failure or always), not never",
                    ));
                }
                if limits.cpu.is_none() && limits.memory.is_none() {
                    return Err(invalid(format!("{prefix}.limits"), "set cpu and/or memory"));
                }
                if limits.cpu.is_some()
                    && limits
                        .cpu
                        .as_deref()
                        .and_then(super::schema::parse_cpu_limit)
                        .is_none()
                {
                    return Err(invalid(
                        format!("{prefix}.limits.cpu"),
                        "expected a positive integer percentage such as '50%'",
                    ));
                }
                if limits.memory.is_some()
                    && limits
                        .memory
                        .as_deref()
                        .and_then(super::schema::parse_memory_limit)
                        .is_none()
                {
                    return Err(invalid(
                        format!("{prefix}.limits.memory"),
                        "expected a positive size with B, KB, MB, GB, KiB, MiB, or GiB suffix",
                    ));
                }
            }
            if service.restart_on_dep_recovery {
                if service.depends_on.is_empty() {
                    return Err(invalid(
                        format!("{prefix}.restart-on-dep-recovery"),
                        "requires at least one dependency",
                    ));
                }
                if service.restart.policy == RestartPolicyType::Never {
                    return Err(invalid(
                        format!("{prefix}.restart-on-dep-recovery"),
                        "requires an automatic restart policy (on-failure or always), not never",
                    ));
                }
            }
            if service.restart.backoff == BackoffType::Exponential
                && service.restart.max_delay < service.restart.initial_delay
            {
                return Err(invalid(
                    format!("{prefix}.restart.max-delay"),
                    "must be at least initial-delay for exponential backoff",
                ));
            }
            if let Some(check) = &service.healthcheck {
                check.validate().map_err(|error| match error {
                    ConfigValidationError::InvalidField { field, reason } => {
                        invalid(format!("{prefix}.{field}"), reason)
                    }
                    error => error,
                })?;
            }
        }

        let graph = DependencyGraph::from_config(self)?;
        for (name, dependency, condition) in graph.edges() {
            let target = &self.services[dependency];
            let matches = matches!(
                (condition, &target.healthcheck),
                (DependencyCondition::Started, _)
                    | (
                        DependencyCondition::ScriptReady,
                        Some(HealthCheck::Script { .. })
                    )
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

impl HealthCheck {
    /// Check probe fields without accessing the network or filesystem.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        let prefix = "healthcheck";
        let (interval, timeout, retries) = match self {
            HealthCheck::Script {
                command,
                interval,
                timeout,
                retries,
            } => {
                if command.contains('\0')
                    || !shell_words::split(command)
                        .is_ok_and(|args| args.first().is_some_and(|arg| !arg.is_empty()))
                {
                    return Err(invalid("healthcheck.command", "expected a non-empty command with valid shell-style quoting and no NUL bytes"));
                }
                (interval, timeout, retries)
            }
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
    fn test_config_dependency_recovery_rejects_missing_dependencies_and_never_policy() {
        for fields in [
            "restart-on-dep-recovery: true",
            "depends-on: [db], restart-on-dep-recovery: true, restart: {policy: never}",
        ] {
            let error = validate(&format!(
                "  db: {{command: db}}\n  api: {{command: api, {fields}}}\n"
            ))
            .unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field, .. }
                if field == "services.api.restart-on-dep-recovery")
            );
        }
        for policy in ["on-failure", "always"] {
            validate(&format!("  db: {{command: db}}\n  api: {{command: api, depends-on: [db], restart-on-dep-recovery: true, restart: {{policy: {policy}}}}}\n")).unwrap();
        }
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
    fn test_config_validation_rejects_ephemeral_listen_ports() {
        let error = validate("  api: {command: api, listen: ['127.0.0.1:0']}\n").unwrap_err();
        assert!(matches!(
            error,
            ConfigValidationError::InvalidField { field, .. }
                if field == "services.api.listen"
        ));
        validate("  api: {command: api, listen: ['127.0.0.1:3000', '[::1]:3001']}\n").unwrap();
    }

    #[test]
    fn test_config_validation_rejects_empty_required_paths() {
        for path in ["''", "\"\"", "\"bad\\0path\""] {
            let error = validate(&format!(
                "  api:\n    command: api\n    requires: [{{type: file, path: {path}}}]\n"
            ))
            .unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field, .. } if field == "services.api.requires[0].path")
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
    fn test_script_validation_checks_command_timing_and_readiness_type() {
        for (fields, field) in [
            ("command: ''", "command"),
            ("command: \"''\"", "command"),
            ("command: \"sh '\"", "command"),
            ("command: \"sh \\0\"", "command"),
            ("command: probe, interval: 0s", "interval"),
            ("command: probe, timeout: 0s", "timeout"),
            ("command: probe, retries: 0", "retries"),
        ] {
            let error = validate(&format!(
                "  api: {{command: api, healthcheck: {{type: script, {fields}}}}}\n"
            ))
            .unwrap_err();
            assert!(
                matches!(error, ConfigValidationError::InvalidField { field: actual, .. } if actual == format!("services.api.healthcheck.{field}")),
                "{fields}"
            );
        }
        validate("  api: {command: api, healthcheck: {type: script, command: 'sh check.sh'}}\n  web: {command: web, depends-on: [{service: api, condition: script-ready}]}\n").unwrap();
        for condition in ["tcp-ready", "http-ready", "socket-ready"] {
            assert!(validate(&format!("  api: {{command: api, healthcheck: {{type: script, command: probe}}}}\n  web: {{command: web, depends-on: [{{service: api, condition: {condition}}}]}}\n")).is_err());
        }
        assert!(validate("  api: {command: api, healthcheck: {type: tcp, port: 80}}\n  web: {command: web, depends-on: [{service: api, condition: script-ready}]}\n").is_err());
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
    fn test_resource_limits_require_positive_bounded_thresholds() {
        for (limits, field) in [
            ("{}", "limits"),
            ("{cpu: '0%'}", "limits.cpu"),
            ("{cpu: '+5%'}", "limits.cpu"),
            ("{cpu: 'NaN%'}", "limits.cpu"),
            ("{cpu: '50'}", "limits.cpu"),
            ("{cpu: '1.5%'}", "limits.cpu"),
            ("{cpu: '4294967296%'}", "limits.cpu"),
            ("{memory: 0MiB}", "limits.memory"),
            ("{memory: 1TB}", "limits.memory"),
            ("{memory: 18446744073709551615GiB}", "limits.memory"),
        ] {
            let error =
                validate(&format!("  api: {{command: api, limits: {limits}}}\n")).unwrap_err();
            assert!(
                matches!(&error, ConfigValidationError::InvalidField { field: actual, .. } if *actual == format!("services.api.{field}")),
                "{limits}: {error}"
            );
        }
        validate("  api: {command: api, limits: {cpu: '250%', memory: 1GiB}}\n").unwrap();
    }

    #[test]
    fn test_resource_restart_permission_rejects_never_policy() {
        let error = validate("  api: {command: sleep, restart: {policy: never}, limits: {memory: 1MiB, on-exceed: restart}}\n").unwrap_err();
        assert!(
            matches!(error, ConfigValidationError::InvalidField { field, .. } if field == "services.api.limits.on-exceed")
        );
        for policy in ["on-failure", "always"] {
            validate(&format!("  api: {{command: sleep, restart: {{policy: {policy}}}, limits: {{cpu: '50%', on-exceed: restart}}}}\n")).unwrap();
        }
        validate("  api: {command: sleep, restart: {policy: never}, limits: {memory: 1MiB}}\n")
            .unwrap();
    }

    #[test]
    fn test_resource_threshold_units_preserve_decimal_and_binary_sizes() {
        for (unit, bytes) in [
            ("B", 1),
            ("KB", 1_000),
            ("MB", 1_000_000),
            ("GB", 1_000_000_000),
            ("KiB", 1024),
            ("MiB", 1024 * 1024),
            ("GiB", 1024 * 1024 * 1024),
        ] {
            let config = ConfigLoader::from_str(&format!("version: '1'\nservices:\n  api: {{command: sleep, limits: {{memory: 2{unit}, cpu: '200%'}}}}\n"), "test.yml").unwrap();
            config.validate().unwrap();
            let parsed = config.services["api"]
                .limits
                .as_ref()
                .unwrap()
                .thresholds()
                .unwrap();
            assert_eq!(parsed.memory_bytes, Some(2 * bytes));
            assert_eq!(parsed.cpu_percent, Some(200));
        }
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
