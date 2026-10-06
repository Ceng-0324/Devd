use std::{path::Path, time::Duration};

use devd::config::{ConfigError, ConfigLoader, HealthCheck, ProfileError, RestartPolicyType};

const CONFIG: &str = r#"
version: '1'
services:
  db:
    command: sleep 60
  api:
    command: base-api
    cwd: backend
    env_file: base.env
    env: {MODE: base, KEEP: inherited}
    depends_on: [db]
    healthcheck: {type: tcp, port: 3000}
    restart: {policy: always, initial_delay: 2s, max_attempts: 9}
profiles:
  dev:
    services:
      api:
        command: dev-api
        env: {MODE: dev}
        env-file: dev.env
        depends-on: []
        healthcheck: {type: http, url: 'http://localhost:4000/health'}
        restart: {max-attempts: 5}
      worker:
        command: dev-worker
        depends-on: [api]
  staging:
    services:
      api:
        cwd: null
        env-file: null
        healthcheck: null
        restart: {policy: never}
"#;

#[test]
fn test_profiles_merge_maps_replace_lists_and_probes_and_add_services() {
    let base = ConfigLoader::from_str(CONFIG, "devd.yml").unwrap();
    assert_eq!(base.services.len(), 2);
    assert_eq!(base.services["api"].command, "base-api");
    let dev = ConfigLoader::from_str_profile(CONFIG, "devd.yml", Some("dev")).unwrap();
    dev.validate().unwrap();
    let api = &dev.services["api"];
    assert_eq!(api.command, "dev-api");
    assert_eq!(api.cwd.as_deref(), Some(Path::new("backend")));
    assert_eq!(api.env_file.as_deref(), Some(Path::new("dev.env")));
    assert_eq!(api.env["MODE"], "dev");
    assert_eq!(api.env["KEEP"], "inherited");
    assert!(api.depends_on.is_empty());
    assert!(matches!(api.healthcheck, Some(HealthCheck::Http { .. })));
    assert_eq!(api.restart.policy, RestartPolicyType::Always);
    assert_eq!(api.restart.initial_delay, Duration::from_secs(2));
    assert_eq!(api.restart.max_attempts, 5);
    assert_eq!(dev.services["worker"].command, "dev-worker");
    assert_eq!(dev.services["worker"].depends_on[0].service, "api");
    // Selection never changes the base definition.
    assert_eq!(base, ConfigLoader::from_str(CONFIG, "devd.yml").unwrap());
}

#[test]
fn test_profile_null_clears_optional_fields_and_empty_maps_inherit() {
    let staging = ConfigLoader::from_str_profile(CONFIG, "devd.yml", Some("staging")).unwrap();
    let api = &staging.services["api"];
    assert!(api.cwd.is_none());
    assert!(api.env_file.is_none());
    assert!(api.healthcheck.is_none());
    assert_eq!(api.env["MODE"], "base");
    assert_eq!(api.restart.max_attempts, 9);
    let yaml = CONFIG
        .replace("env: {MODE: dev}", "env: {}\n        restart: {}")
        .replace("        restart: {max-attempts: 5}\n", "");
    let dev = ConfigLoader::from_str_profile(&yaml, "devd.yml", Some("dev")).unwrap();
    assert_eq!(dev.services["api"].env["MODE"], "base");
    assert_eq!(dev.services["api"].restart.max_attempts, 9);
}

#[test]
fn test_profiles_reject_invalid_unselected_overrides() {
    for patch in [
        "typo: true",
        "env: null",
        "env: {MODE: null}",
        "command: null",
        "restart: null",
        "restart: {max-attempt: 2}",
        "healthcheck: {type: tcp, port: 80, url: wrong}",
        "depends-on: null",
        "env_file: one\n        env-file: two",
        "restart: {initial_delay: 1s, initial-delay: 2s}",
    ] {
        let yaml = format!("version: '1'\nservices:\n  api: {{command: sleep 60}}\nprofiles:\n  bad:\n    services:\n      api:\n        {patch}\n");
        let error = ConfigLoader::from_str(&yaml, "test.yml")
            .err()
            .unwrap_or_else(|| panic!("accepted override: {patch}"));
        assert!(
            error.to_string().contains("profiles.bad.services.api"),
            "{patch}: {error}"
        );
    }
    for extra in [
        "profiles: {dev: {typo: {}}}",
        "profiles: {dev: {services: {api: null}}}",
        "profiles: null",
        "profiles: {dev: {services: null}}",
        "profiles: {dev: {}, dev: {}}",
        "profiles: {dev: {services: {api: {command: one}, api: {command: two}}}}",
    ] {
        assert!(ConfigLoader::from_str(
            &format!("version: '1'\nservices: {{api: {{command: sleep 60}}}}\n{extra}"),
            "test.yml"
        )
        .is_err());
    }
}

#[test]
fn test_profiles_reject_unsafe_names_and_report_available_profiles() {
    for name in [
        "",
        ".",
        "..",
        "../dev",
        "/dev",
        "dev/prod",
        "two words",
        "开发",
    ] {
        let yaml = format!("version: '1'\nprofiles:\n  '{}': {{}}\n", name);
        assert!(ConfigLoader::from_str(&yaml, "test.yml").is_err(), "{name}");
        assert!(matches!(
            ConfigLoader::from_str_profile(CONFIG, "test.yml", Some(name)),
            Err(ConfigError::Profile {
                source: ProfileError::InvalidName(_),
                ..
            })
        ));
    }
    let error = ConfigLoader::from_str_profile(CONFIG, "test.yml", Some("prod")).unwrap_err();
    assert!(error
        .to_string()
        .contains("unknown profile 'prod'; available profiles: dev, staging"));
    let error =
        ConfigLoader::from_str_profile("version: '1'\n", "test.yml", Some("dev")).unwrap_err();
    assert!(error.to_string().contains("available profiles: (none)"));
}

#[tokio::test]
async fn test_profile_validation_uses_effective_dependencies_and_context() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("devd.yml");
    for patch in [
        "depends-on: [missing]",
        "depends-on: [worker]",
        "depends-on: [{service: db, condition: http-ready}]",
        "command: ''",
        "limits: {cpu: '50%'}",
    ] {
        let yaml = format!("version: '1'\nservices:\n  db: {{command: sleep 60}}\n  api: {{command: sleep 60}}\n  worker: {{command: sleep 60, depends-on: [api]}}\nprofiles:\n  dev:\n    services:\n      api:\n        {patch}\n");
        tokio::fs::write(&path, yaml).await.unwrap();
        ConfigLoader::new().load(&path).await.unwrap();
        let error = ConfigLoader::new()
            .load_profile(&path, Some("dev"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::Profile {
                    source: ProfileError::Validation { .. },
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("profile 'dev'"));
        assert!(error.to_string().contains("devd.yml"));
    }
    tokio::fs::write(&path, "version: '1'\nservices: {}\nprofiles:\n  dev:\n    services:\n      app: {command: sleep 60}\n").await.unwrap();
    ConfigLoader::new()
        .load_profile(&path, Some("dev"))
        .await
        .unwrap();
    assert!(ConfigLoader::new().load(&path).await.is_err());
}

#[test]
fn test_profile_new_services_require_command_and_base_is_strict() {
    let yaml = "version: '1'\nservices: {}\nprofiles:\n  dev:\n    services:\n      worker: {env: {MODE: dev}}\n";
    let error = ConfigLoader::from_str_profile(yaml, "test.yml", Some("dev")).unwrap_err();
    assert!(
        error.to_string().contains("missing field `command`"),
        "{error}"
    );
    assert!(error.to_string().contains("profile 'dev'"));
    assert!(ConfigLoader::from_str_profile(
        &CONFIG.replace("command: base-api", "commmand: base-api"),
        "test.yml",
        Some("dev")
    )
    .is_err());
}
