use super::*;

fn write(project: &Project, document: &Value) {
    fs::write(
        project.root.path().join("devd.yml"),
        serde_yaml::to_string(document).unwrap(),
    )
    .unwrap();
}

fn service<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
}

#[test]
fn test_coverage_static_is_read_only_and_resolves_profile_without_probing() {
    let project = Project::new(true, false);
    let document = json!({"version":"1", "services":{"worker":{
        "command":"SECRET-missing-command", "env-file":"SECRET-missing-env",
        "requires":[{"type":"file", "path":"SECRET-missing-resource"}],
        "monitor-requires":true,
        "paths":{"CACHE":{"path":"cache", "scope":"instance", "cleanup":true}},
        "healthcheck":{"type":"script", "command":"SECRET-missing-probe"}
    }}, "profiles":{"dev":{"services":{"worker":{"monitor-requires":false}}}}});
    write(&project, &document);
    let report = project.json(&["coverage", "--json"]);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["source"], "effective-configuration");
    assert_eq!(report["profile"], "dev");
    assert!(report["run_id"].is_null());
    assert!(!report.to_string().contains("SECRET"));
    let worker = service(&report, "worker");
    assert_eq!(
        worker["requirements"][0]["runtime_monitor_configured"],
        false
    );
    assert_eq!(worker["paths"][0]["scope"], "instance");
    assert!(worker["runtime_status"].is_null());
    for channel in ["events", "logs", "incidents"] {
        assert_eq!(report["evidence"][channel]["state"], "unknown");
    }
    let text = project.invoke(&["coverage"]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("requires:0") && text.contains("paths:CACHE"));
    assert!(text.contains("requires-not-monitored"));
    assert_eq!(fs::read_dir(project.root.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(project._state.path()).unwrap().count(), 0);
    let unavailable = project.invoke(&["coverage", "--online", "--json"]);
    assert_eq!(unavailable.status.code(), Some(1));
    assert!(unavailable.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unavailable.stderr).contains("no static fallback"));
    assert_eq!(fs::read_dir(project._state.path()).unwrap().count(), 0);
    let mut document = document;
    document["profiles"]["dev"]["services"]["worker"]["monitor-requires"] = json!(true);
    write(&project, &document);
    let report = project.json(&["coverage", "--json"]);
    assert_eq!(
        service(&report, "worker")["requirements"][0]["runtime_monitor_configured"],
        true
    );
    assert!(!report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == "requires-not-monitored"));
}

#[test]
fn test_coverage_rejects_malformed_and_oversized_configuration() {
    let project = Project::new(false, false);
    for yaml in [
        "services: [".to_string(),
        "version: '1'\nservices: {}".to_string(),
        "x".repeat(1024 * 1024 + 1),
    ] {
        fs::write(project.root.path().join("devd.yml"), yaml).unwrap();
        let result = project.invoke(&["coverage", "--json"]);
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert!(!result.stderr.is_empty());
        assert!(!project.root.path().join(".devd").exists());
    }
}

#[test]
fn test_coverage_online_uses_effective_configuration_across_edits_and_reload() {
    let mut project = Project::new(true, false);
    let path = project.root.path().join("devd.yml");
    let mut config: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(project.root.path().join("required"), "private-body").unwrap();
    config["services"]["worker"]["requires"] = json!([{"type":"file", "path":"required"}]);
    config["services"]["worker"]["monitor-requires"] = json!(true);
    write(&project, &config);
    project.gate();
    project.start_with(&[
        "start",
        "--persist-events",
        "--event-max-size",
        "1",
        "--event-keep",
        "1",
    ]);
    project.json(&["wait", "--json"]);
    let before = project.json(&["coverage", "--online", "--json"]);
    fs::copy(&path, project.root.path().join("other.yml")).unwrap();
    let wrong_target = project.invoke(&["coverage", "--online", "--config", "other.yml", "--json"]);
    assert_eq!(wrong_target.status.code(), Some(1));
    assert!(wrong_target.stdout.is_empty());
    assert!(String::from_utf8_lossy(&wrong_target.stderr).contains("does not match"));
    assert_eq!(before["source"], "live-supervisor");
    assert_eq!(before["profile"], "dev");
    assert_eq!(before["evidence"]["events"]["state"], "recording");
    assert_eq!(
        before["evidence"]["events"]["retention"],
        json!({"max_file_bytes":1048576, "archives":1})
    );
    assert_eq!(before["evidence"]["incidents"]["state"], "disabled");
    assert_eq!(before["evidence"]["logs"]["state"], "disabled");
    assert_eq!(
        service(&before, "worker")["monitor_requires_configured"],
        true
    );
    config["services"]["worker"]["monitor-requires"] = json!(false);
    write(&project, &config);
    let static_report = project.json(&["coverage", "--json"]);
    assert_eq!(
        service(&static_report, "worker")["monitor_requires_configured"],
        false
    );
    let current = project.json(&["coverage", "--online", "--json"]);
    assert_eq!(
        service(&current, "worker")["monitor_requires_configured"],
        true
    );
    let plan = project.json(&["reload", "--dry-run", "--json"]);
    let applied = project.json(&[
        "reload",
        "--apply",
        "--plan",
        plan["plan_id"].as_str().unwrap(),
        "--json",
    ]);
    assert_eq!(applied["outcome"], "applied");
    fs::remove_file(path).unwrap();
    let after = project.json(&["coverage", "--online", "--json"]);
    assert_eq!(after["run_id"], before["run_id"]);
    assert_eq!(after["instance_id"], before["instance_id"]);
    assert_eq!(
        service(&after, "worker")["monitor_requires_configured"],
        false
    );
    project.stop();
    assert!(!project
        .invoke(&["coverage", "--online", "--json"])
        .status
        .success());
}

#[test]
fn test_coverage_recorders_are_independent_and_retention_tracks_each_run() {
    let mut project = Project::new(false, false);
    project.gate();
    project.start_with(&["start", "--record-incidents", "--persist-logs"]);
    let first = project.json(&["coverage", "--online", "--json"]);
    assert_eq!(first["evidence"]["events"]["state"], "disabled");
    assert_eq!(first["evidence"]["incidents"]["state"], "recording");
    assert_eq!(first["evidence"]["logs"]["state"], "recording");
    assert_eq!(first["evidence"]["logs"]["retention"]["archives"], 3);
    project.stop();
    project.start_with(&[
        "start",
        "--persist-logs",
        "--log-max-size",
        "1",
        "--log-keep",
        "1",
    ]);
    let second = project.json(&["coverage", "--online", "--json"]);
    assert_ne!(first["run_id"], second["run_id"]);
    assert_eq!(second["evidence"]["incidents"]["state"], "disabled");
    assert_eq!(
        second["evidence"]["logs"]["retention"],
        json!({"max_file_bytes":1048576, "archives":1})
    );
    project.stop();
    // Retained directories do not establish the next run's recording choices.
    project.start();
    let third = project.json(&["coverage", "--online", "--json"]);
    for channel in ["events", "logs", "incidents"] {
        assert_eq!(third["evidence"][channel]["state"], "disabled");
    }
    assert!(third["evidence"]["logs"]["retention"].is_null());
    project.stop();
}
