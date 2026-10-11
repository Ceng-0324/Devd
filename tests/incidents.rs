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
        "requires":[{"type":"file","path":"resource"}],"monitor-requires":true}}),
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
    stop(root.path(), &mut child);
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
    thread::sleep(Duration::from_millis(300));
    assert_eq!(artifacts(root.path()).len(), 20);
    success(invoke(root.path(), &["status", "--json"]));
    stop(root.path(), &mut child);
    let records: Vec<_> = artifacts(root.path())
        .iter()
        .map(|p| read_records(p))
        .collect();
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
