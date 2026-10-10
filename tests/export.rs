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
    let mut child = Command::new(env!("CARGO_BIN_EXE_devd"))
        .current_dir(root)
        .args(args)
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
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
