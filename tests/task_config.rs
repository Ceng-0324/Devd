use devd::config::{ConfigLoader, RestartPolicyType, ServiceConfig, ServiceKind};
use std::time::Duration;

#[test]
fn test_task_defaults_are_contextual_in_yaml_json_and_profiles() {
    for yaml in [
        "command: build\nkind: task",
        "command: build\nkind: task\nrestart: {initial-delay: 2s}",
    ] {
        let config: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.kind, ServiceKind::Task);
        assert_eq!(config.restart.policy, RestartPolicyType::Never);
        assert_eq!(
            config.effective_task_timeout(),
            Some(Duration::from_secs(300))
        );
        let roundtrip: ServiceConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(config, roundtrip);
    }
    let config: ServiceConfig =
        serde_json::from_str(r#"{"command":"build","kind":"task"}"#).unwrap();
    assert_eq!(config.restart.policy, RestartPolicyType::Never);
    let yaml = "version: '1'\nservices:\n  build: {command: build}\nprofiles:\n  tasks:\n    services:\n      build: {kind: task, task_timeout: 2s}\n";
    let base = ConfigLoader::from_str(yaml, "config.yml").unwrap();
    assert_eq!(
        base.services["build"].restart.policy,
        RestartPolicyType::OnFailure
    );
    let task = ConfigLoader::from_str_profile(yaml, "config.yml", Some("tasks")).unwrap();
    task.validate().unwrap();
    assert_eq!(
        task.services["build"].restart.policy,
        RestartPolicyType::Never
    );
    assert_eq!(
        task.services["build"].task_timeout,
        Some(Duration::from_secs(2))
    );
    let yaml = "version: '1'\nservices:\n  build: {command: build, kind: task, task-timeout: 2s, restart: {policy: on-failure}}\nprofiles:\n  tasks:\n    services:\n      build: {task-timeout: null, restart: {initial-delay: 3s}}\n";
    let task = ConfigLoader::from_str_profile(yaml, "config.yml", Some("tasks")).unwrap();
    assert_eq!(
        task.services["build"].restart.policy,
        RestartPolicyType::OnFailure
    );
    assert_eq!(
        task.services["build"].effective_task_timeout(),
        Some(Duration::from_secs(300))
    );
}

#[test]
fn test_task_validation_rejects_ambiguous_lifecycle_and_completion_edges() {
    for fields in [
        "kind: task, restart: {policy: always}",
        "kind: task, healthcheck: {type: tcp, port: 12345}",
        "kind: task, task-timeout: 0ms",
        "task-timeout: 1s",
        "kind: task, restart-on-dep-recovery: true",
        "kind: task, restart: {policy: on-failure}, limits: {memory: 1B, on-exceed: restart}",
    ] {
        let yaml = format!("version: '1'\nservices:\n  build: {{command: build, {fields}}}");
        let config = ConfigLoader::from_str(&yaml, "config.yml").unwrap();
        assert!(config.validate().is_err(), "{fields}");
    }
    for (kind, condition, valid) in [
        ("task", "completed-successfully", true),
        ("task", "started", false),
        ("service", "completed-successfully", false),
        ("service", "started", true),
    ] {
        let yaml = format!("version: '1'\nservices:\n  build: {{command: build, kind: {kind}}}\n  api:\n    command: api\n    depends-on: [{{service: build, condition: {condition}}}]");
        assert_eq!(
            ConfigLoader::from_str(&yaml, "config.yml")
                .unwrap()
                .validate()
                .is_ok(),
            valid
        );
    }
    for yaml in [
        "kind: task\nunknown: 1",
        "kind: task\nrestart: {unknown: 1}",
        "kind: task\nkind: service",
        "task-timeout: invalid",
        "kind: job",
    ] {
        assert!(
            serde_yaml::from_str::<ServiceConfig>(&format!("command: build\n{yaml}")).is_err(),
            "{yaml}"
        );
    }
}
