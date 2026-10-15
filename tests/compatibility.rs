//! Frozen release output guards reader compatibility independently of today's writer.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

fn invoke(root: &Path, args: &[&str], expected: i32) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_devd"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn test_published_v09_reports_remain_historical_and_comparable_without_project() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat/v0.9");
    let manifest: Value =
        serde_json::from_slice(&fs::read(fixtures.join("manifest.json")).unwrap()).unwrap();
    let root = tempfile::tempdir().unwrap();
    for (name, schema) in [("live.json", 1), ("stored.json", 2), ("incident.json", 3)] {
        let bytes = fs::read(fixtures.join(name)).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            manifest["reports"][name].as_str().unwrap()
        );
        fs::write(root.path().join(name), &bytes).unwrap();
        let diagnosis = invoke(root.path(), &["diagnose", name, "--json"], 0);
        assert_eq!(diagnosis["input_schema_version"], schema);
        assert_eq!(
            diagnosis["current_health"],
            "not-inspected-historical-evidence-only"
        );
        if schema == 1 {
            // v0.9 had no kind field; v1 defaults historical states to service.
            assert_eq!(diagnosis["recorded_state"]["worker"]["kind"], "service");
        }
        let comparison = invoke(root.path(), &["compare", name, name, "--json"], 0);
        assert_eq!(comparison["source"], "offline-report-comparison");
        assert!(comparison["comparability"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field["field"] == "platform" && field["status"] == "unknown"));
    }
    let mixed = invoke(
        root.path(),
        &["compare", "live.json", "incident.json", "--json"],
        0,
    );
    assert!(mixed["comparability"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["field"] == "observation-scope" && f["status"] == "unknown"));
    let mut future: Value =
        serde_json::from_slice(&fs::read(fixtures.join("live.json")).unwrap()).unwrap();
    future["schema_version"] = json!(999);
    fs::write(
        root.path().join("future.json"),
        serde_json::to_vec(&future).unwrap(),
    )
    .unwrap();
    invoke(root.path(), &["diagnose", "future.json", "--json"], 1);
    assert_eq!(
        fs::read_dir(root.path()).unwrap().count(),
        4,
        "offline readers created state"
    );
}

#[test]
fn test_legacy_yaml_aliases_profiles_and_default_permissions_still_apply() {
    let root = tempfile::tempdir().unwrap();
    // Old public snake_case aliases and shorthand dependencies remain accepted.
    let config = json!({"version":"1", "services":{
        "db":{"command":"not-executed", "restart":{"policy":"never"}},
        "api":{"command":"not-executed", "cwd":".", "env_file":"not-read",
            "depends_on":["db"], "restart":{"initial_delay":"1s","max_attempts":3},
            "requires":[{"type":"file","path":"not-inspected"}]}
    }, "profiles":{"dev":{"services":{"api":{"env":{"MODE":"dev"}}}}}});
    fs::write(
        root.path().join("devd.yml"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let coverage = invoke(root.path(), &["coverage", "--profile", "dev", "--json"], 0);
    assert_eq!(coverage["profile"], "dev");
    for service in coverage["services"].as_array().unwrap() {
        assert_eq!(service["kind"], "service");
        assert_eq!(service["monitor_requires_configured"], false);
    }
    for channel in ["events", "logs", "incidents"] {
        assert_eq!(coverage["evidence"][channel]["state"], "unknown");
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
