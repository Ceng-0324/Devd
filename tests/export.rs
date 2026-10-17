use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};

fn temporary() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("de-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

fn invoke(root: &Path, args: &[&str]) -> Output {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root)
            .args(args)
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI timed out: {args:?}");
        thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: fs::read(stdout.path()).unwrap(),
        stderr: fs::read(stderr.path()).unwrap(),
    }
}

fn transferred_report(root: &Path, filename: &str) -> Value {
    let recipient = temporary();
    fs::copy(root.join(filename), recipient.path().join("evidence.json")).unwrap();
    let output = invoke(recipient.path(), &["diagnose", "evidence.json", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_dir(recipient.path()).unwrap().count(),
        1,
        "diagnosis created state"
    );
    let diagnosis: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(diagnosis["source"], "imported-report");
    assert_eq!(
        diagnosis["current_health"],
        "not-inspected-historical-evidence-only"
    );
    diagnosis
}

#[test]
fn test_compare_cli_preserves_gap_metadata_and_truncated_evidence() {
    use serde_json::json;
    let root = temporary();
    let original: Value =
        serde_json::from_str(include_str!("fixtures/compat/v0.9/live.json")).unwrap();
    let compare = |left: &Value, right: &Value| {
        for (file, report) in [("left.json", left), ("right.json", right)] {
            fs::write(root.path().join(file), serde_json::to_vec(report).unwrap()).unwrap();
        }
        let output = invoke(
            root.path(),
            &["compare", "left.json", "right.json", "--json"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let baseline = compare(&original, &original);
    assert_eq!(baseline["left"]["complete"], true);
    assert_eq!(baseline["right"]["complete"], true);
    assert!(!baseline["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["category"] == "evidence-gap"));

    let mut truncated = original.clone();
    truncated["events"]["entries"][0]["truncated"] = json!(true);
    let result = compare(&original, &truncated);
    assert_eq!(result["left"]["complete"], true);
    assert_eq!(result["right"]["complete"], false);
    assert_eq!(
        result["changes"], baseline["changes"],
        "truncation must not imply an operation failure"
    );
    let both = compare(&truncated, &truncated);
    assert_eq!(both["left"]["complete"], false);
    assert_eq!(both["right"]["complete"], false);

    let mut left = original.clone();
    left["events"]["gaps"] = json!([{"reason":"recovered-tail","bytes":12}]);
    for (gaps, omitted, key) in [
        (
            json!([{"reason":"unfinished-record","bytes":12}]),
            0,
            "unfinished-record",
        ),
        (
            json!([{"reason":"recovered-tail","bytes":24}]),
            0,
            "recovered-tail/payload",
        ),
        (
            json!([{"reason":"recovered-tail","bytes":12}, {"reason":"recovered-tail","bytes":12}]),
            0,
            "recovered-tail",
        ),
        (left["events"]["gaps"].clone(), 4, "omitted-gaps"),
    ] {
        let mut right = left.clone();
        right["events"]["gaps"] = gaps;
        right["events"]["omitted_gaps"] = json!(omitted);
        let result = compare(&left, &right);
        assert!(
            result["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["category"] == "evidence-gap" && c["key"] == key),
            "{result}"
        );
        assert_eq!(result["left"]["complete"], false);
        assert_eq!(result["right"]["complete"], false);
    }
    assert_eq!(
        fs::read_dir(root.path()).unwrap().count(),
        2,
        "offline comparison created state"
    );
}

#[test]
fn test_transferred_diagnose_and_compare_report_truncated_and_lifecycle_evidence() {
    use serde_json::json;

    let root = temporary();
    let left: Value = serde_json::from_str(include_str!("fixtures/compat/v0.9/live.json")).unwrap();
    let mut right = left.clone();
    right["state"]["worker"]["restart_count"] = json!(2);
    right["state"]["worker"]["event_generation"] = json!(3);
    for event in right["events"]["entries"].as_array_mut().unwrap() {
        if event["service"] == "worker" {
            event["truncated"] = json!(true);
        }
    }

    for (name, report) in [("left.json", &left), ("right.json", &right)] {
        fs::write(root.path().join(name), serde_json::to_vec(report).unwrap()).unwrap();
    }
    let diagnosis = invoke(root.path(), &["diagnose", "right.json", "--json"]);
    assert!(
        diagnosis.status.success(),
        "{}",
        String::from_utf8_lossy(&diagnosis.stderr)
    );
    let diagnosis: Value = serde_json::from_slice(&diagnosis.stdout).unwrap();
    assert_eq!(diagnosis["explanations"]["worker"]["complete"], false);
    assert!(diagnosis["explanations"]["worker"]["details"]
        .as_array()
        .unwrap()
        .iter()
        .any(|detail| detail.as_str().unwrap().contains("内容被截断")));

    let comparison = invoke(
        root.path(),
        &["compare", "left.json", "right.json", "--json"],
    );
    assert!(
        comparison.status.success(),
        "{}",
        String::from_utf8_lossy(&comparison.stderr)
    );
    let comparison: Value = serde_json::from_slice(&comparison.stdout).unwrap();
    let changes = comparison["changes"].as_array().unwrap();
    for field in ["restart_count", "event_generation"] {
        assert!(changes.iter().any(|change| {
            change["category"] == "state"
                && change["key"] == format!("worker.{field}")
                && change["status"] == "different"
        }));
    }
    assert_eq!(comparison["right"]["complete"], false);
}

#[test]
fn test_diagnose_cli_rejects_bad_reports_and_instance_options_without_state() {
    let root = temporary();
    let path = root.path().join("report.json");
    for content in [
        r#"{"schema_version":999}"#.to_owned(),
        r#"{"schema_version":1"#.to_owned(),
        format!("{}0{}", "[".repeat(150), "]".repeat(150)),
    ] {
        fs::write(&path, content).unwrap();
        let out = invoke(root.path(), &["diagnose", "report.json", "--json"]);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.contains(&0x1b));
    }
    for (option, value) in [
        ("--config", "missing.yml"),
        ("--profile", "dev"),
        ("--state-dir", "missing-state"),
    ] {
        let out = invoke(root.path(), &[option, value, "diagnose", "report.json"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("diagnose reads only its report"));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&path, root.path().join("linked.json")).unwrap();
        let out = invoke(root.path(), &["diagnose", "linked.json"]);
        assert!(!out.status.success());
        fs::remove_file(root.path().join("linked.json")).unwrap();
        nix::unistd::mkfifo(&root.path().join("pipe"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let out = invoke(root.path(), &["diagnose", "pipe"]);
        assert!(!out.status.success());
        fs::remove_file(root.path().join("pipe")).unwrap();
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

struct Supervisor(Child);
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn test_export_live_report_is_bounded_private_and_never_overwrites() {
    let root = temporary();
    let fixture = format!(
        "{} --ignored --exact test_export_worker --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    );
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&serde_json::json!({
            "version": "1",
            "services": {"worker": {
                "command": fixture,
                "env": {"PRIVATE_TOKEN": "SECRET-ENV-VALUE"},
                "restart": {"policy": "never"}
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root.path())
            .arg("start")
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "supervisor exited: {}",
            fs::read_to_string(stderr.path()).unwrap()
        );
        let output = invoke(root.path(), &["logs", "worker"]);
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("SECRET-LOG-VALUE")
        {
            break;
        }
        assert!(Instant::now() < deadline, "worker log did not appear");
        thread::sleep(Duration::from_millis(30));
    }
    let before: Value =
        serde_json::from_slice(&invoke(root.path(), &["identity", "--json"]).stdout).unwrap();
    fs::remove_file(root.path().join("devd.yml")).unwrap();
    let exported = invoke(root.path(), &["export", "--output", "diagnostic.json"]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let raw = fs::read_to_string(root.path().join("diagnostic.json")).unwrap();
    let report: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["identity"]["instance_id"], before["instance_id"]);
    assert_eq!(report["identity"]["run_id"], before["run_id"]);
    assert_eq!(report["events"]["context"]["run_id"], before["run_id"]);
    assert!(report["state"]["worker"]["pid"].is_number());
    assert_eq!(
        report["state"]["worker"]["event_generation"],
        report["explanations"]["worker"]["generation"]
    );
    assert!(report["events"]["cursor"]["next_sequence"].is_number());
    assert!(report["stable_during_capture"].is_boolean());
    assert!(report["logs"].is_null());
    assert!(!raw.contains("SECRET-LOG-VALUE"));
    assert!(!raw.contains("SECRET-ENV-VALUE"));
    assert!(!raw.contains("PRIVATE_TOKEN"));
    assert!(report["state"]["worker"].get("last_error").is_none());
    let second = invoke(root.path(), &["export", "--output", "diagnostic.json"]);
    assert!(!second.status.success());
    assert_eq!(
        raw,
        fs::read_to_string(root.path().join("diagnostic.json")).unwrap()
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("diagnostic.json", root.path().join("linked.json")).unwrap();
        assert!(!invoke(root.path(), &["export", "--output", "linked.json"])
            .status
            .success());
        assert_eq!(
            raw,
            fs::read_to_string(root.path().join("diagnostic.json")).unwrap()
        );
    }
    assert!(invoke(
        root.path(),
        &["export", "--include-logs", "--output", "with-logs.json"]
    )
    .status
    .success());
    let with_logs = fs::read_to_string(root.path().join("with-logs.json")).unwrap();
    assert!(with_logs.contains("SECRET-LOG-VALUE"));
    let logs: Value = serde_json::from_str(&with_logs).unwrap();
    assert_eq!(logs["log_tail_limit"], 200);
    assert_eq!(logs["logs"].as_array().unwrap().len(), 200);
    assert!(!with_logs.contains("EARLIEST-LOG-VALUE"));
    assert!(!logs.to_string().contains("SECRET-ENV-VALUE"));
    let recipient = temporary();
    fs::copy(
        root.path().join("diagnostic.json"),
        recipient.path().join("left.json"),
    )
    .unwrap();
    fs::copy(
        root.path().join("with-logs.json"),
        recipient.path().join("right.json"),
    )
    .unwrap();
    let compared = invoke(
        recipient.path(),
        &["compare", "left.json", "right.json", "--json"],
    );
    assert!(
        compared.status.success(),
        "{}",
        String::from_utf8_lossy(&compared.stderr)
    );
    let comparison: Value = serde_json::from_slice(&compared.stdout).unwrap();
    assert_eq!(comparison["source"], "offline-report-comparison");
    assert_eq!(comparison["left"]["input_schema_version"], 1);
    assert!(comparison["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["category"] == "log-attachment"));
    assert!(!comparison.to_string().contains("/missing"));
    assert!(invoke(root.path(), &["stop"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    while supervisor.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "supervisor stop timed out");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !invoke(root.path(), &["export", "--output", "stopped.json"])
            .status
            .success()
    );
    assert!(!root.path().join("stopped.json").exists());
}

#[test]
#[ignore = "subprocess fixture for diagnostic export"]
fn test_export_worker() {
    println!("EARLIEST-LOG-VALUE");
    for index in 0..205 {
        println!("log entry {index}");
    }
    println!("SECRET-LOG-VALUE");
    thread::sleep(Duration::from_secs(120));
}

#[test]
fn test_export_and_explain_follow_recovered_generation() {
    let root = temporary();
    let executable = root.path().join(if cfg!(windows) {
        "worker.exe"
    } else {
        "worker"
    });
    let fixture = format!(
        "{} --ignored --exact test_export_worker --nocapture",
        shell_words::quote(executable.to_str().unwrap())
    );
    fs::write(root.path().join("devd.yml"), serde_yaml::to_string(&serde_json::json!({
        "version": "1", "services": {"worker": { "command": fixture,
            "restart": {"policy": "on-failure", "initial-delay": "100ms", "max-delay": "100ms", "max-attempts": 100}
        }}
    })).unwrap()).unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root.path())
            .args(["start", "--persist-events"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = invoke(root.path(), &["events", "--json"]);
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("spawn-failed")
        {
            break;
        }
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "supervisor exited"
        );
        assert!(Instant::now() < deadline, "missing spawn failure");
        thread::sleep(Duration::from_millis(20));
    }
    // Publish a usable executable atomically after a real failed spawn.
    let staged = root.path().join("staged-worker");
    fs::copy(std::env::current_exe().unwrap(), &staged).unwrap();
    fs::rename(staged, &executable).unwrap();
    let ready = invoke(root.path(), &["wait", "--timeout", "15s", "--json"]);
    assert!(
        ready.status.success(),
        "{}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let exported = invoke(root.path(), &["export", "--output", "recovered.json"]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("recovered.json")).unwrap()).unwrap();
    let explanation = &report["explanations"]["worker"];
    assert_eq!(explanation["conclusion"], "running");
    assert_eq!(
        explanation["generation"],
        report["state"]["worker"]["event_generation"]
    );
    assert!(report["events"].to_string().contains("spawn-failed")); // History is retained.
    let live: Value =
        serde_json::from_slice(&invoke(root.path(), &["explain", "worker", "--json"]).stdout)
            .unwrap();
    assert_eq!(live["conclusion"], "running");
    assert_eq!(live["generation"], explanation["generation"]);
    let diagnosis = transferred_report(root.path(), "recovered.json");
    assert_eq!(diagnosis["input_schema_version"], 1);
    assert_eq!(diagnosis["explanations"]["worker"]["conclusion"], "running");
    assert_eq!(
        diagnosis["explanations"]["worker"]["generation"],
        explanation["generation"]
    );
    assert!(invoke(root.path(), &["stop"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    while supervisor.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "stop timed out");
        thread::sleep(Duration::from_millis(20));
    }
    let stored: Value = serde_json::from_slice(
        &invoke(root.path(), &["explain", "worker", "--stored", "--json"]).stdout,
    )
    .unwrap();
    assert_eq!(stored["conclusion"], "manually-stopped");
    assert_eq!(stored["generation"], explanation["generation"]);
}

fn read_report(root: &Path, file: &str) -> Value {
    serde_json::from_slice(&fs::read(root.join(file)).unwrap()).unwrap()
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_offline_export_explicit_runs_missing_yaml_historical_state_and_unassigned_logs() {
    let root = temporary();
    let fixture = format!(
        "{} --ignored --exact test_export_worker --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    );
    fs::write(root.path().join("devd.yml"), serde_yaml::to_string(&serde_json::json!({
        "version":"1", "services":{"worker":{"command":fixture,"env":{"PRIVATE":"SECRET-ENV"},"restart":{"policy":"never"}}},
        "profiles":{"dev":{"services":{}}}
    })).unwrap()).unwrap();
    let state = root.path().join("custom");
    let common = ["--profile", "dev", "--state-dir", state.to_str().unwrap()];
    let mut runs = Vec::new();
    for _ in 0..2 {
        let mut child = Supervisor(
            Command::new(env!("CARGO_BIN_EXE_devd"))
                .current_dir(root.path())
                .args(common)
                .args(["start", "--persist-events", "--persist-logs"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(child.0.try_wait().unwrap().is_none());
            let mut args = common.to_vec();
            args.extend(["logs", "worker"]);
            let output = invoke(root.path(), &args);
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("SECRET-LOG-VALUE")
            {
                break;
            }
            assert!(Instant::now() < deadline, "no fixture logs");
            thread::sleep(Duration::from_millis(20));
        }
        let mut args = common.to_vec();
        args.extend(["identity", "--json"]);
        let identity: Value = serde_json::from_slice(&invoke(root.path(), &args).stdout).unwrap();
        runs.push(identity["run_id"].as_str().unwrap().to_owned());
        let mut args = common.to_vec();
        args.extend([
            "export",
            "--stored",
            "--run",
            "latest",
            "--output",
            "active.json",
        ]);
        assert!(!invoke(root.path(), &args).status.success());
        assert!(!root.path().join("active.json").exists());
        let mut args = common.to_vec();
        args.push("stop");
        assert_success(invoke(root.path(), &args));
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
    }
    fs::remove_file(root.path().join("devd.yml")).unwrap();
    let export = |run: &str, file: &str, logs: bool| {
        let mut args = common.to_vec();
        args.extend(["export", "--stored", "--run", run, "--output", file]);
        if logs {
            args.push("--include-logs");
        }
        invoke(root.path(), &args)
    };
    assert_success(export(&runs[0], "old.json", false));
    let old = read_report(root.path(), "old.json");
    assert_eq!(old["schema_version"], 2);
    assert_eq!(old["source"], "stored");
    assert_eq!(old["identity"], Value::Null);
    assert_eq!(old["events"]["context"]["run_id"], runs[0]);
    assert!(old["events"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["run_id"] == runs[0]));
    assert_eq!(
        old["stored"]["historical_state"]["availability"],
        "different-run"
    );
    assert_eq!(old["stored"]["historical_state"]["state"], Value::Null);
    assert_eq!(old["state"], serde_json::json!({}));
    assert_eq!(old["stored"]["live_state"], "unavailable-offline");
    assert_eq!(old["stored"]["selection"]["profile"], "dev");
    assert!(old["stored"]["unassigned_logs"].is_null());
    let diagnosis = transferred_report(root.path(), "old.json");
    assert_eq!(diagnosis["input_schema_version"], 2);
    assert_eq!(diagnosis["timeline"]["context"]["run_id"], runs[0]);
    assert!(diagnosis["recorded_state"].as_object().unwrap().is_empty());
    assert_eq!(
        diagnosis["stored_run_coverage"]["state_availability"],
        "different-run"
    );
    assert!(!old.to_string().contains("SECRET-LOG-VALUE"));
    assert!(!old.to_string().contains("SECRET-ENV"));
    assert_success(export("latest", "latest.json", true));
    let latest = read_report(root.path(), "latest.json");
    assert_eq!(latest["events"]["context"]["run_id"], runs[1]);
    assert_eq!(
        latest["stored"]["historical_state"]["availability"],
        "retained-matching-run"
    );
    assert_eq!(latest["stored"]["historical_state"]["stale"], true);
    assert_eq!(latest["stored"]["begin_retained"], true);
    assert_eq!(latest["stored"]["end_retained"], true);
    assert!(latest["logs"].is_null());
    let logs = &latest["stored"]["unassigned_logs"];
    assert_eq!(logs["run_attribution"], "unknown-no-run-id-in-log-format");
    assert_eq!(logs["records"]["entries"].as_array().unwrap().len(), 200);
    assert!(logs.to_string().contains("SECRET-LOG-VALUE"));
    assert!(latest["explanations"]["worker"]["status"].is_null());
    assert_eq!(
        latest["explanations"]["worker"]["conclusion"],
        "manually-stopped"
    );
    let diagnosis = transferred_report(root.path(), "latest.json");
    assert_eq!(
        diagnosis["explanations"]["worker"]["conclusion"],
        "manually-stopped"
    );
    assert_eq!(diagnosis["log_attachments"][0]["entries"], 200);
    assert_eq!(
        diagnosis["recorded_state"]["worker"],
        latest["stored"]["historical_state"]["state"]["worker"]
    );
    assert!(!diagnosis.to_string().contains("SECRET-LOG-VALUE"));
    assert!(!export("latest", "latest.json", true).status.success());
    assert_eq!(latest, read_report(root.path(), "latest.json"));
    assert!(
        !export(&uuid::Uuid::new_v4().to_string(), "unknown.json", false)
            .status
            .success()
    );
    let state_path = Path::new(
        latest["stored"]["historical_state"]["source"]
            .as_str()
            .unwrap(),
    );
    fs::remove_file(state_path).unwrap();
    assert_success(export("latest", "missing.json", false));
    assert_eq!(
        read_report(root.path(), "missing.json")["stored"]["historical_state"]["availability"],
        "missing"
    );
    fs::write(state_path, "broken SECRET-STATE").unwrap();
    assert_success(export("latest", "invalid.json", false));
    let invalid = read_report(root.path(), "invalid.json");
    assert_eq!(
        invalid["stored"]["historical_state"]["availability"],
        "invalid"
    );
    assert!(!invalid.to_string().contains("SECRET-STATE"));
    fs::write(
        state_path,
        serde_json::to_vec(&serde_json::json!({"supervisor_pid":123,"services":{}})).unwrap(),
    )
    .unwrap();
    assert_success(export("latest", "legacy.json", false));
    assert_eq!(
        read_report(root.path(), "legacy.json")["stored"]["historical_state"]["availability"],
        "unknown-run"
    );
}

fn offline_fixture(root: &Path) -> std::path::PathBuf {
    use devd::{
        core::events::{storage::EventStorage, EventContext, EventData, LifecycleEvent},
        storage::StorageOptions,
    };
    use std::io::Write;
    let state = root.join(".devd/devd.yml");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("services.json.lock"), "").unwrap();
    let context = EventContext {
        run_id: uuid::Uuid::new_v4().to_string(),
        state_path: state.join("services.json"),
        profile: None,
    };
    let storage = EventStorage::open(
        &state.join("events"),
        StorageOptions::default(),
        context.clone(),
    )
    .unwrap();
    drop(storage);
    let event = LifecycleEvent {
        schema_version: 1,
        run_id: context.run_id.clone(),
        sequence: 0,
        timestamp: chrono::Utc::now(),
        elapsed: Duration::ZERO,
        service: Some("api".into()),
        generation: Some(0),
        cause: None,
        truncated: false,
        data: EventData::Starting,
    };
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(state.join("events/current.jsonl"))
        .unwrap();
    for record in [
        serde_json::json!({"record":"event","data":event}),
        serde_json::json!({"record":"end","data":{"next_sequence":1}}),
    ] {
        writeln!(
            file,
            "{}",
            serde_json::json!({"schema_version":1,"context":context,"record":record})
        )
        .unwrap();
    }
    state
}

#[test]
fn test_offline_export_incomplete_tails_and_unsafe_files() {
    let root = temporary();
    let state = offline_fixture(root.path());
    let export = |file: &str| {
        invoke(
            root.path(),
            &[
                "export",
                "--stored",
                "--run",
                "latest",
                "--include-logs",
                "--output",
                file,
            ],
        )
    };
    let current = state.join("events/current.jsonl");
    let content = fs::read_to_string(&current).unwrap();
    let lines: Vec<_> = content.lines().collect();
    fs::write(
        &current,
        format!("{}\n{{partial", lines[..lines.len() - 1].join("\n")),
    )
    .unwrap();
    assert_success(export("incomplete.json"));
    let report = read_report(root.path(), "incomplete.json");
    assert_eq!(report["stored"]["end_retained"], false);
    assert_eq!(report["stored"]["event_coverage_complete"], false);
    assert_eq!(
        report["stored"]["unassigned_logs"]["availability"],
        "missing"
    );
    assert!(report["events"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["reason"] == "unfinished-record"));
    assert!(report["events"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["reason"] == "incomplete-run"));
    assert!(report["explanations"]["api"]["status"].is_null());
    let diagnosis = transferred_report(root.path(), "incomplete.json");
    assert_eq!(
        diagnosis["stored_run_coverage"]["end_reported_retained"],
        false
    );
    assert_eq!(diagnosis["explanations"]["api"]["complete"], false);
    fs::remove_file(&current).unwrap();
    let target = root.path().join("private-events");
    fs::write(&target, &content).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &current).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &current).expect("symlink privileges required");
    assert!(!export("unsafe.json").status.success());
    assert!(!root.path().join("unsafe.json").exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), content);
    fs::remove_file(&current).unwrap();
    fs::create_dir(&current).unwrap();
    assert!(!export("directory.json").status.success());
    fs::remove_dir(&current).unwrap();
    #[cfg(unix)]
    {
        nix::unistd::mkfifo(&current, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(!export("fifo.json").status.success());
        fs::remove_file(&current).unwrap();
    }
    fs::write(&current, content).unwrap();
    let lock = fs::File::open(state.join("events/.lock")).unwrap();
    lock.try_lock().unwrap();
    assert!(!export("locked.json").status.success());
    lock.unlock().unwrap();
    let historical = state.join("services.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &historical).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &historical).unwrap();
    assert!(!export("unsafe-state.json").status.success());
}

#[test]
fn test_offline_export_requires_explicit_valid_selector_and_leaves_no_state() {
    let root = temporary();
    for args in [
        vec!["export", "--stored", "--output", "a.json"],
        vec!["export", "--run", "latest", "--output", "a.json"],
        vec![
            "export",
            "--stored",
            "--run",
            "not-a-uuid",
            "--output",
            "a.json",
        ],
        vec![
            "export", "--stored", "--run", "latest", "--output", "a.json",
        ],
    ] {
        assert!(!invoke(root.path(), &args).status.success());
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn test_offline_export_signal_cancels_scan_without_publishing() {
    use std::io::Write;
    let root = temporary();
    let state = offline_fixture(root.path());
    let path = state.join("events/current.jsonl");
    let original = fs::read_to_string(&path).unwrap();
    let begin = original.lines().next().unwrap();
    let mut gap: Value = serde_json::from_str(begin).unwrap();
    gap["record"] =
        serde_json::json!({"record":"gap","data":{"reason":"recovered-tail","bytes":1}});
    let mut file = fs::File::create(&path).unwrap();
    writeln!(file, "{begin}").unwrap();
    let line = format!("{gap}\n");
    for _ in 0..100_000 {
        file.write_all(line.as_bytes()).unwrap();
    }
    drop(file);
    let mut child = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root.path())
            .args([
                "export",
                "--stored",
                "--run",
                "latest",
                "--output",
                "cancelled.json",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let probe = fs::File::open(state.join("services.json.lock")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if probe.try_lock().is_err() {
            break;
        }
        probe.unlock().unwrap();
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "export completed before read lease"
        );
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.0.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "cancelled reader did not exit");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    assert!(!root.path().join("cancelled.json").exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    probe.try_lock().unwrap();
    probe.unlock().unwrap();
}
