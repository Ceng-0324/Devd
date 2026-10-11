use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn temporary() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("devd-impact-")
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
    let mut child = Guard(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args(args)
            .current_dir(root)
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

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_report(root: &Path, args: &[&str]) -> Value {
    let output = invoke(root, args);
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Running {
    child: Child,
    root: std::path::PathBuf,
}
impl Running {
    fn start(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut running = Self {
            child,
            root: root.into(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                running.child.try_wait().unwrap().is_none(),
                "supervisor exited at startup"
            );
            let output = invoke(root, &["status", "--json"]);
            if output.status.success() {
                let status: Value = serde_json::from_slice(&output.stdout).unwrap();
                if status["services"]["worker"]["pid"].is_number() {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "supervisor startup timed out");
            thread::sleep(Duration::from_millis(25));
        }
        running
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = invoke(&self.root, &["stop"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn test_impact_profile_alias_and_no_process_side_effects() {
    let root = temporary();
    fs::write(root.path().join("data"), "secret-content").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("data", root.path().join("alias")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file("data", root.path().join("alias"))
        .expect("Windows symlink privileges required");
    fs::write(root.path().join("devd.yml"), "version: '1'\nservices:\n  db:\n    command: 'not-a-program'\n    requires: [{type: file, path: base-only}]\n    paths: {DATA: {scope: shared, path: alias}}\n  api: {command: 'not-a-program', depends-on: [db]}\n  web: {command: 'not-a-program', depends-on: [api]}\nprofiles:\n  dev:\n    services:\n      db: {requires: [{type: file, path: data}]}\n").unwrap();
    let report = read_report(
        root.path(),
        &[
            "impact",
            "db",
            "--require",
            "0",
            "--profile",
            "dev",
            "--json",
        ],
    );
    assert_eq!(report["scope"], "project");
    assert_eq!(report["profile"], "dev");
    assert_eq!(report["selected"]["declared_path"], "data");
    assert_eq!(report["selected"]["declared_scope"], Value::Null);
    assert_eq!(report["current"]["matches"].as_array().unwrap().len(), 2);
    assert_eq!(
        report["current"]["direct_consumers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(report["current"]["downstream"][0]["service"], "web");
    assert_eq!(
        report["current"]["downstream"][0]["via_services"],
        json!(["api", "web"])
    );
    assert!(!report.to_string().contains("secret-content"));
    assert!(!root.path().join(".devd").exists());
    let text = invoke(root.path(), &["impact", "db", "--path", "DATA"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("binding-recipient"));
}

#[test]
fn test_impact_instance_binding_custom_state_is_observed_without_provisioning() {
    let root = temporary();
    fs::write(root.path().join("devd.yml"), "version: '1'\nservices:\n  db: {command: unused, paths: {DATA: {scope: instance, path: cache, cleanup: true}}}\n  api: {command: unused, depends-on: [db]}\n").unwrap();
    let state = root.path().join("custom-state");
    let result = read_report(
        root.path(),
        &[
            "impact",
            "db",
            "--path",
            "DATA",
            "--state-dir",
            state.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(result["selected"]["declared_scope"], "instance");
    assert!(result["selected"]["requested_path"]
        .as_str()
        .unwrap()
        .contains("custom-state"));
    assert_eq!(
        result["current"]["direct_consumers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!state.exists());
    assert!(!root.path().join(".devd").exists());
}

#[test]
fn test_impact_invalid_selection_fails_without_scanning_or_creating_state() {
    let root = temporary();
    fs::write(
        root.path().join("devd.yml"),
        "version: '1'\nservices:\n  api: {command: unused}\n",
    )
    .unwrap();
    for args in [
        vec!["impact", "api", "--require", "0"],
        vec!["impact", "missing", "--path", "DATA"],
    ] {
        let result = invoke(root.path(), &args);
        assert!(!result.status.success());
    }
    assert!(!root.path().join(".devd").exists());
}

#[test]
fn test_impact_worktree_live_disk_candidate_then_unreachable_stale_record() {
    let root = temporary();
    let peer_parent = temporary();
    let peer = peer_parent.path().join("linked worktree");
    git(root.path(), &["init", "-b", "main"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    git(
        root.path(),
        &["worktree", "add", "--detach", peer.to_str().unwrap()],
    );
    let shared = root.path().join("shared");
    fs::write(&shared, "private").unwrap();
    let current = json!({"version":"1", "services":{"worker":{
        "command":"unused", "requires":[{"type":"file", "path":shared}]}}});
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&current).unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    let command = "sh -c 'exec sleep 120'";
    #[cfg(windows)]
    let command = "powershell.exe -NoProfile -Command 'Start-Sleep -Seconds 120'";
    let peer_doc = json!({"version":"1", "services":{"worker":{
        "command":command, "requires":[{"type":"file", "path":shared}],
        "restart":{"policy":"never"}}}});
    fs::write(
        peer.join("devd.yml"),
        serde_yaml::to_string(&peer_doc).unwrap(),
    )
    .unwrap();
    let running = Running::start(&peer);
    let report = read_report(
        root.path(),
        &[
            "impact",
            "worker",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    let discovery = &report["discovery"];
    assert_eq!(discovery["registry_complete"], true);
    assert_eq!(discovery["impact_complete"], false);
    assert_eq!(discovery["peers"].as_array().unwrap().len(), 1);
    let peer_report = &discovery["peers"][0];
    assert_eq!(peer_report["registry_status"], "live");
    assert_eq!(peer_report["config_source"], "disk-candidate");
    assert_eq!(peer_report["runtime_equivalence"], "unknown");
    assert_eq!(
        peer_report["view"]["direct_consumers"][0]["service"],
        "worker"
    );
    // A disk edit cannot silently become a claim about the live supervisor.
    let mut changed = peer_doc;
    changed["services"]["worker"]["requires"][0]["path"] = json!(peer.join("other"));
    fs::write(
        peer.join("devd.yml"),
        serde_yaml::to_string(&changed).unwrap(),
    )
    .unwrap();
    let edited = read_report(
        root.path(),
        &[
            "impact",
            "worker",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    assert!(edited["discovery"]["peers"][0]["view"]["matches"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        edited["discovery"]["peers"][0]["runtime_equivalence"],
        "unknown"
    );
    fs::write(peer.join("devd.yml"), "invalid: [").unwrap();
    let invalid = read_report(
        root.path(),
        &[
            "impact",
            "worker",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    assert_eq!(invalid["discovery"]["peers"][0]["registry_status"], "live");
    assert!(invalid["discovery"]["peers"][0]["view"].is_null());
    assert!(invalid["discovery"]["peers"][0]["warning"]
        .as_str()
        .unwrap()
        .contains("cannot read valid"));
    fs::remove_file(peer.join("devd.yml")).unwrap();
    let removed = read_report(
        root.path(),
        &[
            "impact",
            "worker",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    assert!(removed["discovery"]["peers"][0]["view"].is_null());
    assert_eq!(removed["discovery"]["impact_complete"], false);
    drop(running);
    let stale = read_report(
        root.path(),
        &[
            "impact",
            "worker",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    assert_eq!(
        stale["discovery"]["peers"][0]["registry_status"],
        "unreachable"
    );
    assert!(stale["discovery"]["peers"][0]["view"].is_null());
    // More valid historical identities than the impact peer budget must be counted,
    // not silently presented as complete coverage.
    use sha2::{Digest, Sha256};
    let index = peer.join(".devd/instances");
    let record_path = fs::read_dir(&index)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record: Value = serde_json::from_slice(&fs::read(record_path).unwrap()).unwrap();
    for number in 0..8 {
        let mut extra = record.clone();
        extra["state_dir"] = json!(peer.join(format!("absent-state-{number}")));
        let key = json!([extra["config"], extra["state_dir"], extra["profile"]]);
        let id = format!("{:x}", Sha256::digest(serde_json::to_vec(&key).unwrap()));
        extra["instance_id"] = json!(id);
        fs::write(
            index.join(format!("{id}.json")),
            serde_json::to_vec(&extra).unwrap(),
        )
        .unwrap();
    }
    let limited = read_report(
        root.path(),
        &[
            "impact",
            "worker",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    assert_eq!(limited["discovery"]["registry_complete"], true);
    assert_eq!(limited["discovery"]["peers"].as_array().unwrap().len(), 8);
    assert_eq!(limited["discovery"]["omitted_peers"], 1);
    assert_eq!(limited["discovery"]["impact_complete"], false);
    assert!(!root.path().join(".devd").exists());
}

#[test]
fn test_corrupt_registry_exposes_incomplete_discovery() {
    let root = temporary();
    fs::write(
        root.path().join("devd.yml"),
        "version: '1'\nservices:\n  api: {command: unused, requires: [{type: file, path: data}]}\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join(".devd/instances")).unwrap();
    let record = root.path().join(".devd/instances/corrupt.json");
    fs::write(&record, "{").unwrap();
    let report = read_report(
        root.path(),
        &[
            "impact",
            "api",
            "--require",
            "0",
            "--scope",
            "worktrees",
            "--json",
        ],
    );
    assert_eq!(report["discovery"]["registry_complete"], false);
    assert_eq!(report["discovery"]["impact_complete"], false);
    assert!(!report["discovery"]["warnings"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(fs::read_to_string(record).unwrap(), "{");
}
