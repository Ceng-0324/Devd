use serde_json::Value;
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

fn invoke(root: &Path, args: &[&str]) -> Output {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_devd"))
        .args(args)
        .current_dir(root)
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI timed out: {args:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: fs::read(stdout.path()).unwrap(),
        stderr: fs::read(stderr.path()).unwrap(),
    }
}

fn json(root: &Path, args: &[&str]) -> Value {
    let output = invoke(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn configuration(root: &Path) {
    #[cfg(unix)]
    let command = "sh -c 'exec sleep 120'";
    #[cfg(windows)]
    let command = "powershell.exe -NoProfile -Command 'Start-Sleep -Seconds 120'";
    fs::write(root.join("devd.yml"), serde_yaml::to_string(&serde_json::json!({
        "version": "1", "services": {"worker": {"command": command, "restart": {"policy":"never"}}},
        "profiles": {"dev": {"services": {"worker": {"env": {"MODE":"dev"}}}}}
    })).unwrap()).unwrap();
}

struct Running {
    child: Child,
    root: PathBuf,
    arguments: Vec<String>,
}
impl Running {
    fn start(root: &Path, args: &[&str]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .args(args)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut running = Self {
            child,
            root: root.into(),
            arguments: args.iter().map(|v| v.to_string()).collect(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                running.child.try_wait().unwrap().is_none(),
                "supervisor exited at startup"
            );
            let mut query = vec!["status", "--json"];
            query.extend(args);
            let output = invoke(root, &query);
            if output.status.success() {
                let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
                if snapshot["services"]["worker"]["pid"].is_number() {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "startup timed out");
            thread::sleep(Duration::from_millis(20));
        }
        running
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let mut args = vec!["stop"];
        args.extend(self.arguments.iter().map(String::as_str));
        let _ = invoke(&self.root, &args);
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_instances_live_profiles_custom_state_and_restart_identity() {
    let root = temporary();
    let state = temporary();
    configuration(root.path());
    assert_eq!(
        json(root.path(), &["instances", "--json"])["entries"],
        serde_json::json!([])
    );
    assert!(!root.path().join(".devd").exists());
    let first = Running::start(root.path(), &[]);
    let identity = json(root.path(), &["identity", "--json"]);
    assert!(!invoke(root.path(), &["start"]).status.success());
    assert_eq!(json(root.path(), &["identity", "--json"]), identity);
    let snapshot = json(root.path(), &["status", "--json"]);
    assert_eq!(identity["run_id"], snapshot["event_run_id"]);
    assert_eq!(identity["supervisor_pid"], snapshot["supervisor_pid"]);
    let args = [
        "--profile",
        "dev",
        "--state-dir",
        state.path().to_str().unwrap(),
    ];
    let second = Running::start(root.path(), &args);
    let report = json(root.path(), &["instances", "--json"]);
    let entries = report["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry["status"] == "live"));
    assert_ne!(
        entries[0]["identity"]["instance_id"],
        entries[1]["identity"]["instance_id"]
    );
    assert!(entries
        .iter()
        .any(|entry| entry["identity"]["profile"] == "dev"));
    drop(second);
    drop(first);
    let report = json(root.path(), &["instances", "--json"]);
    assert!(report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["status"] == "unreachable"));
    let third = Running::start(root.path(), &[]);
    let replacement = json(root.path(), &["identity", "--json"]);
    assert_eq!(identity["instance_id"], replacement["instance_id"]);
    assert_ne!(identity["run_id"], replacement["run_id"]);
    fs::remove_file(root.path().join("devd.yml")).unwrap();
    assert_eq!(json(root.path(), &["identity", "--json"]), replacement);
    assert!(json(root.path(), &["instances", "--json"])["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["status"] == "live"));
    assert!(
        String::from_utf8(invoke(root.path(), &["instances"]).stdout)
            .unwrap()
            .contains("unreachable")
    );
    drop(third);
}

#[test]
fn test_instances_git_worktrees_detached_nested_config_and_branch_snapshot() {
    let root = temporary();
    let linked_parent = temporary();
    let linked = linked_parent.path().join("work tree");
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
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    let nested = root.path().join("nested");
    fs::create_dir(&nested).unwrap();
    configuration(&nested);
    configuration(&linked);
    let a = Running::start(&nested, &[]);
    let b = Running::start(&linked, &[]);
    let first = json(&nested, &["identity", "--json"]);
    let second = json(&linked, &["identity", "--json"]);
    assert_eq!(first["git"]["common_dir"], second["git"]["common_dir"]);
    assert_eq!(first["git"]["branch_at_start"], "main");
    assert!(second["git"]["branch_at_start"].is_null());
    assert_eq!(
        first["git"]["commit_at_start"],
        second["git"]["commit_at_start"]
    );
    git(root.path(), &["switch", "-c", "changed"]);
    assert_eq!(json(&nested, &["identity", "--json"]), first);
    for directory in [root.path(), linked.as_path(), nested.as_path()] {
        let report = json(directory, &["instances", "--json"]);
        assert_eq!(report["entries"].as_array().unwrap().len(), 2);
        assert!(report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["status"] == "live"));
    }
    drop(b);
    drop(a);
}

#[test]
fn test_instances_corrupt_and_stale_records_are_read_only() {
    let root = temporary();
    configuration(root.path());
    let running = Running::start(root.path(), &[]);
    let mut identity = json(root.path(), &["identity", "--json"]);
    let index = root.path().join(".devd/instances");
    let record = index.join(format!(
        "{}.json",
        identity["instance_id"].as_str().unwrap()
    ));
    identity["run_id"] = uuid::Uuid::new_v4().to_string().into();
    let bytes = serde_json::to_vec(&identity).unwrap();
    fs::write(&record, &bytes).unwrap();
    fs::write(index.join("broken.json"), "not json").unwrap();
    let report = json(root.path(), &["instances", "--json"]);
    assert_eq!(report["entries"][0]["status"], "identity-mismatch");
    assert_eq!(report["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(fs::read(record).unwrap(), bytes);
    assert_eq!(
        fs::read_to_string(index.join("broken.json")).unwrap(),
        "not json"
    );
    assert!(!invoke(root.path(), &["instances", "--profile", "dev"])
        .status
        .success());
    drop(running);
}

#[test]
fn test_instances_unborn_git_and_registration_failure_before_spawn() {
    let root = temporary();
    git(root.path(), &["init", "-b", "main"]);
    configuration(root.path());
    let running = Running::start(root.path(), &[]);
    let identity = json(root.path(), &["identity", "--json"]);
    assert_eq!(identity["git"]["branch_at_start"], "main");
    assert!(identity["git"]["commit_at_start"].is_null());
    drop(running);

    let invalid = temporary();
    configuration(invalid.path());
    let path = invalid.path().join("devd.yml");
    let mut config: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    #[cfg(unix)]
    let command = "sh -c 'touch spawned; exec sleep 120'";
    #[cfg(windows)]
    let command = "powershell.exe -NoProfile -Command 'New-Item spawned; Start-Sleep -Seconds 120'";
    config["services"]["worker"]["command"] = command.into();
    fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
    fs::create_dir(invalid.path().join(".devd")).unwrap();
    fs::write(invalid.path().join(".devd/instances"), "user file").unwrap();
    let output = invoke(invalid.path(), &["start"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot register project instance"));
    assert!(!invalid.path().join("spawned").exists());
    assert_eq!(
        fs::read_to_string(invalid.path().join(".devd/instances")).unwrap(),
        "user file"
    );
    let report = json(invalid.path(), &["instances", "--json"]);
    assert_eq!(report["complete"], false);
    assert_eq!(report["warnings"].as_array().unwrap().len(), 1);
}
