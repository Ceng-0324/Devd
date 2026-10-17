//! Native-process tests: the recorder observes failures without owning recovery.
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn temporary() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("di-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

struct Supervisor(Child);
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[track_caller]
fn wait<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "incident condition timed out");
        thread::sleep(Duration::from_millis(15));
    }
}

fn invoke(root: &Path, args: &[&str]) -> Output {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut process = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root)
            .args(args)
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let status = wait(|| process.0.try_wait().unwrap());
    Output {
        status,
        stdout: fs::read(stdout.path()).unwrap(),
        stderr: fs::read(stderr.path()).unwrap(),
    }
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
}

fn start(root: &Path, record: bool) -> Supervisor {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devd"));
    command
        .current_dir(root)
        .arg("start")
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.join("stderr")).unwrap());
    if record {
        command.arg("--record-incidents");
    }
    let mut child = Supervisor(command.spawn().unwrap());
    wait(|| {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(root.join("stderr")).unwrap()
        );
        invoke(root, &["identity", "--json"])
            .status
            .success()
            .then_some(())
    });
    child
}

fn stop(root: &Path, child: &mut Supervisor) {
    success(invoke(root, &["stop"]));
    assert!(
        wait(|| child.0.try_wait().unwrap()).success(),
        "{}",
        fs::read_to_string(root.join("stderr")).unwrap()
    );
}

fn fixture() -> String {
    format!(
        "{} --ignored --exact test_incident_worker --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    )
}

fn write_config(root: &Path, services: Value) {
    fs::write(
        root.join("devd.yml"),
        serde_yaml::to_string(&json!({"version":"1","services":services})).unwrap(),
    )
    .unwrap();
}

fn artifacts(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join(".devd/devd.yml/incidents"))
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
                .then_some(path)
        })
        .collect()
}

fn read_records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.len() <= 2 * 1024 * 1024);
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn first_capture(root: &Path) -> PathBuf {
    wait(|| {
        artifacts(root).into_iter().find(|p| {
            read_records(p)
                .first()
                .is_some_and(|r| r["record"] == "begin")
        })
    })
}

#[test]
#[ignore = "subprocess fixture for incident collection"]
fn test_incident_worker() {
    println!("SECRET-APPLICATION-LOG");
    thread::sleep(Duration::from_secs(120));
}

#[test]
fn test_incidents_default_off_and_manual_control_do_not_trigger() {
    let root = temporary();
    write_config(
        root.path(),
        json!({"worker":{"command":fixture(),"restart":{"policy":"never"}}}),
    );
    for enabled in [false, true] {
        let mut child = start(root.path(), enabled);
        success(invoke(root.path(), &["wait", "--timeout", "15s"]));
        success(invoke(root.path(), &["restart", "worker"]));
        stop(root.path(), &mut child);
        assert!(artifacts(root.path()).is_empty());
        assert_eq!(
            root.path().join(".devd/devd.yml/incidents").exists(),
            enabled
        );
        assert!(!root.path().join(".devd/devd.yml/logs").exists());
        assert!(!root.path().join(".devd/devd.yml/events").exists());
    }
}

#[test]
fn test_incidents_capture_deleted_target_chain_and_deadline_without_recovery() {
    let root = temporary();
    let target = root.path().join("target");
    fs::write(&target, "SECRET-FILE").unwrap();
    let link = root.path().join("resource");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).expect("symlink privileges required");
    write_config(
        root.path(),
        json!({"worker":{"command":fixture(),"env":{"PRIVATE":"SECRET-ENV"},"restart":{"policy":"never"},
        "requires":[{"type":"file","path":"resource"}],"monitor-requires":true},
        "api":{"command":fixture(),"depends-on":["worker"],"restart":{"policy":"never"}},
        "jobs":{"command":fixture(),"depends-on":["worker"],"restart":{"policy":"never"}},
        "web":{"command":fixture(),"depends-on":["api","jobs"],"restart":{"policy":"never"}}}),
    );
    let mut child = start(root.path(), true);
    success(invoke(root.path(), &["wait", "--timeout", "15s"]));
    let before = success(invoke(root.path(), &["status", "--json"]));
    fs::remove_file(&target).unwrap();
    let artifact = first_capture(root.path());
    wait(|| {
        read_records(&artifact)
            .last()
            .is_some_and(|r| r["record"] == "end")
            .then_some(())
    });
    let records = read_records(&artifact);
    assert_eq!(
        records[0]["data"]["capture"]["trigger"]["data"]["type"],
        "path-condition-changed"
    );
    assert_eq!(records[1]["record"], "resources");
    assert_eq!(
        records[1]["data"]["observations"][0]["inspection"]["failure"],
        "dangling-symlink"
    );
    assert_eq!(records.last().unwrap()["data"]["reason"], "deadline");
    let all = fs::read_to_string(&artifact).unwrap();
    assert!(!all.contains("SECRET"));
    assert!(!all.contains("test_incident_worker"));
    let after = success(invoke(root.path(), &["status", "--json"]));
    assert_eq!(
        before["services"]["worker"]["pid"],
        after["services"]["worker"]["pid"]
    );
    assert_eq!(after["services"]["worker"]["restart_count"], 0);
    let id = artifact.file_stem().unwrap().to_str().unwrap();
    let listed = success(invoke(root.path(), &["incidents", "list", "--json"]));
    assert_eq!(listed["entries"][0]["id"], id);
    assert_eq!(listed["entries"][0]["trigger"], "path-condition-changed");
    let shown = success(invoke(root.path(), &["incidents", "show", id, "--json"]));
    assert_eq!(
        shown["incident"]["resources"]["observations"][0]["inspection"]["failure"],
        "dangling-symlink"
    );
    assert!(shown["incident"]["declared_impact"]
        .as_str()
        .unwrap()
        .contains("recorded-declared-relationships"));
    let relationships = &shown["incident"]["resources"]["relationships"];
    assert_eq!(relationships["dependencies"].as_array().unwrap().len(), 4);
    assert_eq!(
        relationships["impacts"][0]["view"]["downstream"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let tree = invoke(root.path(), &["incidents", "show", id]);
    assert!(tree.status.success());
    let text = String::from_utf8(tree.stdout).unwrap();
    assert!(
        text.contains("[ref]")
            && text.contains("resource observation #0")
            && text.contains("not proven causation")
    );
    assert!(!invoke(root.path(), &["incidents", "list", "--stored"])
        .status
        .success());
    success(invoke(
        root.path(),
        &["export", "--incident", id, "--output", "live.json"],
    ));
    success(invoke(
        root.path(),
        &[
            "export",
            "--incident",
            id,
            "--include-logs",
            "--output",
            "logs.json",
        ],
    ));
    assert!(!fs::read_to_string(root.path().join("live.json"))
        .unwrap()
        .contains("SECRET"));
    let with_logs: Value =
        serde_json::from_slice(&fs::read(root.path().join("logs.json")).unwrap()).unwrap();
    assert_eq!(with_logs["schema_version"], 3);
    assert_eq!(
        with_logs["incident"]["log_attribution"],
        "selected-current-run-not-incident-window"
    );
    assert!(with_logs["incident"]["current_run_logs"]
        .to_string()
        .contains("SECRET-APPLICATION-LOG"));
    assert!(with_logs["state"].as_object().unwrap().is_empty());
    assert_eq!(with_logs["source"], "stored");
    stop(root.path(), &mut child);
    fs::remove_file(root.path().join("devd.yml")).unwrap();
    // A now-repaired target cannot rewrite the captured dangling-link evidence.
    fs::write(&target, "REPAIRED-SECRET").unwrap();
    let offline = success(invoke(
        root.path(),
        &["incidents", "show", id, "--stored", "--json"],
    ));
    assert_eq!(offline["incident"], shown["incident"]);
    success(invoke(
        root.path(),
        &[
            "export",
            "--stored",
            "--incident",
            id,
            "--output",
            "offline.json",
        ],
    ));
    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("offline.json")).unwrap()).unwrap();
    assert_eq!(
        report["incident"]["recorded"]["incident"],
        shown["incident"]
    );
    assert!(report["explanations"]["worker"]["complete"] == false);
    let before = fs::read(root.path().join("offline.json")).unwrap();
    assert!(!invoke(
        root.path(),
        &[
            "export",
            "--stored",
            "--incident",
            id,
            "--output",
            "offline.json"
        ]
    )
    .status
    .success());
    assert_eq!(fs::read(root.path().join("offline.json")).unwrap(), before);
    assert_eq!(artifacts(root.path()).len(), 1);
}

#[test]
fn test_incidents_stop_and_reload_end_followup_explicitly() {
    for reload in [false, true] {
        let root = temporary();
        fs::write(root.path().join("required"), "").unwrap();
        let services = json!({"worker":{"command":fixture(),"restart":{"policy":"never"},
            "requires":[{"type":"file","path":"required"}],"monitor-requires":true}});
        write_config(root.path(), services.clone());
        let mut child = start(root.path(), true);
        success(invoke(root.path(), &["wait", "--timeout", "15s"]));
        fs::remove_file(root.path().join("required")).unwrap();
        let artifact = first_capture(root.path());
        if reload {
            fs::write(root.path().join("required"), "").unwrap();
            let mut next = services;
            next["worker"]["env"] = json!({"RELOAD":"SECRET-RELOAD"});
            write_config(root.path(), next);
            let plan = success(invoke(root.path(), &["reload", "--dry-run", "--json"]));
            success(invoke(
                root.path(),
                &[
                    "reload",
                    "--apply",
                    "--plan",
                    plan["plan_id"].as_str().unwrap(),
                    "--json",
                ],
            ));
            wait(|| {
                read_records(&artifact)
                    .last()
                    .is_some_and(|r| r["record"] == "end")
                    .then_some(())
            });
        }
        stop(root.path(), &mut child);
        assert_eq!(
            read_records(&artifact).last().unwrap()["data"]["reason"],
            if reload { "reload" } else { "stopping" }
        );
        assert!(!fs::read_to_string(&artifact).unwrap().contains("SECRET"));
    }
}

#[test]
fn test_incidents_failure_storm_retention_and_incomplete_killed_capture() {
    let root = temporary();
    write_config(
        root.path(),
        json!({"missing":{"command":"devd-incident-no-such-executable", "restart":{"policy":"on-failure","initial-delay":"30ms","max-delay":"30ms","max-attempts":5000}}}),
    );
    let mut child = start(root.path(), true);
    wait(|| (artifacts(root.path()).len() == 20).then_some(()));
    success(invoke(root.path(), &["status", "--json"]));
    let coverage = wait(|| {
        let report = success(invoke(root.path(), &["coverage", "--online", "--json"]));
        (report["evidence"]["incidents"]["retention_evicted_this_recorder"]
            .as_u64()
            .unwrap()
            > 0)
        .then_some(report)
    });
    assert_eq!(coverage["evidence"]["incidents"]["state"], "recording");
    assert!(coverage["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == "incident-retention-evicted"));
    stop(root.path(), &mut child);
    // Rotation removes the oldest artifact before creating its replacement.
    // Assert the settled count after shutdown drains the writer, not in that
    // valid 19-file window while the failure storm is still running.
    let records: Vec<_> = artifacts(root.path())
        .iter()
        .map(|p| read_records(p))
        .collect();
    assert_eq!(records.len(), 20);
    assert!(records.iter().all(|r| r.last().unwrap()["record"] == "end"));
    assert!(records
        .iter()
        .all(|r| r[0]["data"]["capture"]["trigger"]["generation"].is_number()));
    // No service process can spawn here, so killing this supervisor leaves no
    // running fixture behind. Persisted beginnings cannot promise a future tail.
    let root = temporary();
    write_config(
        root.path(),
        json!({"missing":{"command":"devd-incident-no-such-executable", "restart":{"policy":"on-failure","initial-delay":"10s","max-delay":"10s","max-attempts":100}}}),
    );
    let mut child = start(root.path(), true);
    let artifact = first_capture(root.path());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let records = read_records(&artifact);
    assert_eq!(records[0]["record"], "begin");
    assert!(records.iter().all(|r| r["record"] != "end"));
}

#[test]
fn test_incidents_writer_failure_disables_recording_without_stopping_services() {
    let root = temporary();
    write_config(
        root.path(),
        json!({
            "worker":{"command":fixture(),"restart":{"policy":"never"}},
            "missing":{"command":"devd-incident-no-such-executable", "restart":{"policy":"on-failure","initial-delay":"10s","max-delay":"10s","max-attempts":100}}
        }),
    );
    let mut child = start(root.path(), true);
    let artifact = first_capture(root.path());
    let before = success(invoke(
        root.path(),
        &["wait", "worker", "--timeout", "15s", "--json"],
    ));
    assert!(!before.is_null());
    fs::hard_link(&artifact, root.path().join("extra-link")).unwrap();
    wait(|| {
        fs::read_to_string(root.path().join("stderr"))
            .unwrap()
            .contains("incident recording disabled")
            .then_some(())
    });
    assert!(child.0.try_wait().unwrap().is_none());
    let coverage = success(invoke(root.path(), &["coverage", "--online", "--json"]));
    assert_eq!(coverage["evidence"]["incidents"]["state"], "failed");
    assert_eq!(coverage["evidence"]["events"]["state"], "disabled");
    assert!(coverage["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == "incidents-failed"));
    success(invoke(root.path(), &["wait", "worker", "--timeout", "2s"]));
    assert!(read_records(&artifact).iter().all(|r| r["record"] != "end"));
    stop(root.path(), &mut child);
}

#[test]
fn test_incidents_unsafe_storage_prevents_service_start() {
    let root = temporary();
    write_config(
        root.path(),
        json!({"worker":{"command":fixture(),"restart":{"policy":"never"}}}),
    );
    let state = root.path().join(".devd/devd.yml");
    fs::create_dir_all(&state).unwrap();
    let target = root.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, state.join("incidents")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&target, state.join("incidents"))
        .expect("symlink privileges required");
    let output = invoke(root.path(), &["start", "--record-incidents"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open incident storage"));
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    assert!(!state.join("services.json").exists());
}

#[test]
fn test_incident_queries_isolate_runs_and_report_partial_unsafe_and_unsupported_files() {
    let root = temporary();
    write_config(
        root.path(),
        json!({"missing":{"command":"devd-incident-no-such-executable", "restart":{"policy":"on-failure","initial-delay":"10s","max-delay":"10s","max-attempts":100}}}),
    );
    let mut first = start(root.path(), true);
    let artifact = first_capture(root.path());
    let id = artifact.file_stem().unwrap().to_str().unwrap();
    let run1 = success(invoke(root.path(), &["identity", "--json"]))["run_id"].clone();
    stop(root.path(), &mut first);
    let original = fs::read(&artifact).unwrap();
    let mut second = start(root.path(), true);
    let run2 = success(invoke(root.path(), &["identity", "--json"]))["run_id"].clone();
    assert_ne!(run1, run2);
    let list = success(invoke(root.path(), &["incidents", "list", "--json"]));
    assert!(list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["identity"]["run_id"] == run2));
    assert!(!invoke(root.path(), &["incidents", "show", id, "--json"])
        .status
        .success());
    stop(root.path(), &mut second);
    let list = success(invoke(
        root.path(),
        &["incidents", "list", "--stored", "--json"],
    ));
    assert!(list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["identity"]["run_id"] == run1));
    let inspect = || {
        success(invoke(
            root.path(),
            &["incidents", "show", id, "--stored", "--json"],
        ))
    };
    let mut records = read_records(&artifact);
    records[0]["data"]["capture"]["schema_version"] = json!(999);
    fs::write(&artifact, format!("{}\n", records[0])).unwrap();
    assert_eq!(
        inspect()["incident"]["collection_status"],
        "unsupported-schema"
    );
    let begin = original.split_inclusive(|b| *b == b'\n').next().unwrap();
    fs::write(&artifact, [begin, b"{incomplete"].concat()).unwrap();
    let partial = inspect();
    assert_eq!(partial["incident"]["collection_status"], "incomplete");
    assert!(!partial["incident"]["beginning"].is_null());
    assert!(partial["incident"]["diagnostics"]
        .to_string()
        .contains("unfinished-jsonl-tail"));
    fs::write(&artifact, b"{broken}\n").unwrap();
    assert!(inspect()["incident"]["diagnostics"]
        .to_string()
        .contains("malformed"));
    fs::write(&artifact, &original).unwrap();
    fs::hard_link(&artifact, root.path().join("linked-artifact")).unwrap();
    assert!(inspect()["incident"]["diagnostics"]
        .to_string()
        .contains("unsafe-or-unreadable"));
    fs::remove_file(root.path().join("linked-artifact")).unwrap();
    fs::write(&artifact, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    assert!(inspect()["incident"]["diagnostics"]
        .to_string()
        .contains("exceeds-2-MiB"));
    fs::remove_file(&artifact).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("stderr"), &artifact).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(root.path().join("stderr"), &artifact)
        .expect("symlink privileges required");
    assert!(inspect()["incident"]["diagnostics"]
        .to_string()
        .contains("unsafe-or-unreadable"));
    assert!(!invoke(
        root.path(),
        &["incidents", "show", "../../stderr", "--stored"]
    )
    .status
    .success());
    assert!(!invoke(
        root.path(),
        &[
            "export",
            "--stored",
            "--incident",
            id,
            "--run",
            "latest",
            "--output",
            "invalid.json"
        ]
    )
    .status
    .success());
    assert!(!root.path().join("invalid.json").exists());
}

#[test]
fn test_incident_stored_selection_respects_profile_custom_state_and_missing_yaml() {
    let root = temporary();
    let yaml = json!({"version":"1","services":{"broken":{"command":"devd-incident-no-such-executable","restart":{"policy":"never"}}},
        "profiles":{"test":{"services":{}}}});
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&yaml).unwrap(),
    )
    .unwrap();
    for profile in [false, true] {
        let mut args = vec!["--state-dir", "custom"];
        if profile {
            args.extend(["--profile", "test"]);
        }
        let mut start = args.clone();
        start.extend(["start", "--record-incidents"]);
        assert!(!invoke(root.path(), &start).status.success());
        let mut list = args.clone();
        list.extend(["incidents", "list", "--stored", "--json"]);
        let report = success(invoke(root.path(), &list));
        assert_eq!(report["entries"].as_array().unwrap().len(), 1, "{report}");
        assert_eq!(
            report["entries"][0]["identity"]["profile"],
            if profile { json!("test") } else { Value::Null }
        );
        let id = report["entries"][0]["id"].as_str().unwrap();
        if profile {
            fs::remove_file(root.path().join("devd.yml")).unwrap();
            let mut export = args.clone();
            export.extend([
                "export",
                "--stored",
                "--incident",
                id,
                "--include-logs",
                "--output",
                "profile.json",
            ]);
            success(invoke(root.path(), &export));
            let saved: Value =
                serde_json::from_slice(&fs::read(root.path().join("profile.json")).unwrap())
                    .unwrap();
            assert_eq!(
                saved["incident"]["unassigned_stored_logs"]["availability"],
                "missing"
            );
            assert_eq!(
                saved["incident"]["log_attribution"],
                "unknown-no-run-id-in-stored-logs"
            );
        }
    }
}
