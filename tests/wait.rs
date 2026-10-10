use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};

fn temporary() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("dw-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

// File-backed output avoids pipe backpressure; every subprocess is reaped on panic.
struct CliChild {
    child: Child,
    stdout: tempfile::NamedTempFile,
    stderr: tempfile::NamedTempFile,
}

impl CliChild {
    fn spawn(root: &Path, args: &[String]) -> Self {
        let stdout = tempfile::NamedTempFile::new().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root)
            .args(args)
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI timeout: {}",
                fs::read_to_string(self.stderr.path()).unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        };
        Output {
            status,
            stdout: fs::read(self.stdout.path()).unwrap(),
            stderr: fs::read(self.stderr.path()).unwrap(),
        }
    }
}

impl Drop for CliChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Project {
    root: tempfile::TempDir,
    _state: tempfile::TempDir,
    options: Vec<String>,
    supervisor: Option<CliChild>,
}

impl Project {
    fn new(profile: bool, automatic_restart: bool) -> Self {
        let root = temporary();
        let state = temporary();
        let fixture = |name: &str| {
            format!(
                "{} --ignored --exact {name} --nocapture",
                shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
            )
        };
        fs::write(root.path().join("devd.yml"), serde_yaml::to_string(&json!({
            "version": "1",
            "services": {
                "worker": { "command": fixture("test_wait_worker"), "restart": { "policy": "never" } },
                "api": {
                    "command": fixture("test_wait_worker"), "env": { "WAIT_CAN_EXIT": "yes" },
                    "restart": { "policy": if automatic_restart { "on-failure" } else { "never" }, "initial-delay": "50ms", "max-attempts": 3 },
                    "healthcheck": { "type": "script", "command": fixture("test_wait_probe"), "interval": "50ms", "timeout": "3s", "retries": 100000 }
                }
            },
            "profiles": { "dev": { "services": { "api": { "env": { "PROFILE": "dev" } } } } }
        })).unwrap()).unwrap();
        let options = if profile {
            vec![
                "--profile".into(),
                "dev".into(),
                "--state-dir".into(),
                state.path().to_string_lossy().into_owned(),
            ]
        } else {
            Vec::new()
        };
        Self {
            root,
            _state: state,
            options,
            supervisor: None,
        }
    }

    fn spawn(&self, args: &[&str]) -> CliChild {
        let args: Vec<_> = args
            .iter()
            .map(|v| v.to_string())
            .chain(self.options.clone())
            .collect();
        CliChild::spawn(self.root.path(), &args)
    }

    fn invoke(&self, args: &[&str]) -> Output {
        self.spawn(args).finish()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.invoke(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn start(&mut self) {
        self.supervisor = Some(self.spawn(&["start"]));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.supervisor
                    .as_mut()
                    .unwrap()
                    .child
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "supervisor exited at startup: {}",
                fs::read_to_string(self.supervisor.as_ref().unwrap().stderr.path()).unwrap()
            );
            let output = self.invoke(&["status", "--json"]);
            if output.status.success() {
                let state: Value = serde_json::from_slice(&output.stdout).unwrap();
                if state["services"]["api"]["pid"].is_number()
                    && state["services"]["worker"]["pid"].is_number()
                {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "supervisor startup timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn gate(&self) {
        fs::write(self.root.path().join("ready"), "ready").unwrap();
    }

    // Fill the bounded wait pool, then observe Busy as an acceptance handshake.
    // This makes stop/reload/restart tests independent of CLI scheduling speed.
    fn waiting(&self) -> Vec<CliChild> {
        let mut clients: Vec<_> = (0..8)
            .map(|_| self.spawn(&["wait", "api", "--timeout", "30s", "--json"]))
            .collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = self.invoke(&["wait", "api", "--timeout", "100ms", "--json"]);
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            // The short query can win a slot during startup; replace any client
            // rejected by that query before considering the pool established.
            let mut replaced = false;
            for client in &mut clients {
                if client.child.try_wait().unwrap().is_some() {
                    outcome(client, "busy");
                    *client = self.spawn(&["wait", "api", "--timeout", "30s", "--json"]);
                    replaced = true;
                }
            }
            if report["outcome"] == "busy" && !replaced {
                break;
            }
            assert!(report["outcome"] == "timed-out" || report["outcome"] == "busy");
            assert!(Instant::now() < deadline, "waits were not accepted");
        }
        clients
    }

    fn stop(&mut self) {
        if let Some(mut supervisor) = self.supervisor.take() {
            let _ = self.invoke(&["stop"]);
            let output = supervisor.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Some(mut supervisor) = self.supervisor.take() {
            let _ = self.invoke(&["stop"]);
            // Keep cleanup bounded even when a test assertion is unwinding.
            let deadline = Instant::now() + Duration::from_secs(10);
            while supervisor.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn outcome(child: &mut CliChild, expected: &str) -> Value {
    let output = child.finish();
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(report["outcome"], expected, "{report}");
    assert_eq!(
        output.status.code(),
        Some(if expected == "ready" { 0 } else { 1 })
    );
    assert_eq!(report["schema_version"], 1);
    report
}

#[test]
fn test_wait_selection_timeout_profile_and_deleted_configuration() {
    let mut project = Project::new(true, false);
    let unavailable = outcome(&mut project.spawn(&["wait", "--json"]), "unavailable");
    assert!(unavailable["run_id"].is_null());
    assert!(!project.root.path().join(".devd").exists());
    for timeout in ["0ms", "2h", "invalid"] {
        assert_eq!(
            project
                .invoke(&["wait", "--timeout", timeout])
                .status
                .code(),
            Some(2)
        );
    }
    project.start();
    let identity = project.json(&["identity", "--json"]);
    let before = project.json(&["status", "--json"]);
    let worker = outcome(
        &mut project.spawn(&["wait", "worker", "worker", "--json"]),
        "ready",
    );
    assert_eq!(worker["services"].as_object().unwrap().len(), 1);
    assert_eq!(worker["services"]["worker"]["requires_healthy"], false);
    assert_eq!(worker["instance_id"], identity["instance_id"]);
    let timed_out = outcome(
        &mut project.spawn(&["wait", "--timeout", "1s", "--json"]),
        "timed-out",
    );
    assert_eq!(timed_out["blocking"], json!(["api"]));
    assert_eq!(timed_out["services"]["api"]["requires_healthy"], true);
    assert!(timed_out["observed_at"].is_string());
    let unknown = outcome(
        &mut project.spawn(&["wait", "missing", "--json"]),
        "invalid-service",
    );
    assert!(unknown["services"]["missing"]["status"].is_null());
    project.gate();
    fs::remove_file(project.root.path().join("devd.yml")).unwrap();
    let ready = outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    assert_eq!(ready["run_id"], identity["run_id"]);
    assert_eq!(ready["services"]["api"]["status"], "healthy");
    assert_eq!(
        ready["services"]["api"]["pid"],
        before["services"]["api"]["pid"]
    );
    assert_eq!(ready["blocking"], json!([]));
    let text = project.invoke(&["wait"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("Readiness: Ready"));
    project.stop();
}

#[test]
fn test_wait_full_stdout_pipe_has_a_deadline_and_keeps_services_running() {
    let mut project = Project::new(false, false);
    let config = project.root.path().join("devd.yml");
    let mut yaml: Value = serde_yaml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    // Pending actors create a report larger than native pipe buffers without
    // spawning hundreds of processes. The API probe remains gated.
    for index in 0..1500 {
        yaml["services"][format!("pending-{index:04}")] = json!({
            "command": "never-started", "restart": {"policy": "never"},
            "depends-on": [{"service": "api", "condition": "script-ready"}]
        });
    }
    fs::write(config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    project.start();
    let before = project.json(&["status", "--json"]);
    for cancel in [false, true] {
        if cancel && !cfg!(unix) {
            continue;
        }
        let stdout = tempfile::NamedTempFile::new().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(project.root.path())
            .args(["wait", "--timeout", "500ms", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        let mut client = CliChild {
            child,
            stdout,
            stderr,
        };
        let mut pipe = client.child.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            use std::io::Read;
            let mut first = [0];
            let result = pipe.read_exact(&mut first);
            let _ = sender.send((pipe, result));
        });
        let (_pipe, first) = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        first.unwrap();
        reader.join().unwrap();
        let started = Instant::now();
        #[cfg(unix)]
        if cancel {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(client.child.id() as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .unwrap();
        }
        let output = client.finish();
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if cancel {
                "readiness output cancelled"
            } else {
                "readiness output timed out"
            }),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(if cancel { 3 } else { 10 }));
        let after = project.json(&["status", "--json"]);
        assert_eq!(
            before["services"]["worker"]["pid"],
            after["services"]["worker"]["pid"]
        );
        assert_eq!(
            before["services"]["api"]["pid"],
            after["services"]["api"]["pid"]
        );
    }
    project.stop();
}

#[test]
fn test_wait_follows_manual_and_automatic_generations() {
    for automatic in [false, true] {
        let mut project = Project::new(false, automatic);
        project.start();
        let old = project.json(&["status", "--json"]);
        let mut waiting = project.waiting();
        if automatic {
            fs::write(project.root.path().join("exit-once"), "exit").unwrap();
        } else {
            assert!(project.invoke(&["restart", "api"]).status.success());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = project.json(&["status", "--json"]);
            if state["services"]["api"]["pid"].is_number()
                && state["services"]["api"]["event_generation"]
                    != old["services"]["api"]["event_generation"]
            {
                break;
            }
            assert!(Instant::now() < deadline, "restart timed out");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(waiting
            .iter_mut()
            .all(|c| c.child.try_wait().unwrap().is_none()));
        project.gate();
        for client in &mut waiting {
            let ready = outcome(client, "ready");
            assert_eq!(ready["run_id"], old["event_run_id"]);
            assert_ne!(
                ready["services"]["api"]["generation"],
                old["services"]["api"]["event_generation"]
            );
        }
        project.stop();
    }
}

#[test]
fn test_wait_stopped_service_is_terminal_without_stopping_peers() {
    let mut project = Project::new(false, false);
    project.start();
    let mut waiting = project.waiting();
    fs::write(project.root.path().join("exit-once"), "0").unwrap();
    for client in &mut waiting {
        let report = outcome(client, "failed");
        assert_eq!(report["services"]["api"]["status"], "stopped");
        assert_eq!(report["blocking"], json!(["api"]));
    }
    outcome(&mut project.spawn(&["wait", "worker", "--json"]), "ready");
    project.stop();
}

#[test]
fn test_wait_noop_reload_barrier_rejected_plan_and_stop() {
    let mut project = Project::new(false, false);
    project.start();
    let mut waiting = project.waiting();
    let plan = project.json(&["reload", "--dry-run", "--json"]);
    let applied = project.json(&[
        "reload",
        "--apply",
        "--plan",
        plan["plan_id"].as_str().unwrap(),
        "--json",
    ]);
    assert_eq!(applied["outcome"], "applied");
    for child in &mut waiting {
        outcome(child, "reloaded");
    }
    let mut waiting = project.waiting();
    let stale = format!("sha256:{}", "0".repeat(64));
    assert!(!project
        .invoke(&["reload", "--apply", "--plan", &stale])
        .status
        .success());
    assert!(waiting
        .iter_mut()
        .all(|c| c.child.try_wait().unwrap().is_none()));
    // Status and stop remain usable while all eight wait slots are occupied.
    assert!(project.json(&["status", "--json"])["services"]["api"]["pid"].is_number());
    project.stop();
    for child in &mut waiting {
        outcome(child, "stopping");
    }
}

#[cfg(unix)]
#[test]
fn test_wait_sigint_only_cancels_client() {
    use nix::{
        sys::signal::{kill, Signal},
        unistd::Pid,
    };
    let mut project = Project::new(false, false);
    project.start();
    let before = project.json(&["status", "--json"]);
    let mut waiting = project.waiting();
    kill(Pid::from_raw(waiting[0].child.id() as i32), Signal::SIGINT).unwrap();
    let report = outcome(&mut waiting[0], "cancelled");
    assert_eq!(report["blocking"], json!(["api"]));
    assert_eq!(
        project.json(&["status", "--json"])["services"]["api"]["pid"],
        before["services"]["api"]["pid"]
    );
    project.gate();
    for child in &mut waiting[1..] {
        outcome(child, "ready");
    }
    project.stop();
}

#[test]
#[ignore = "subprocess fixture, invoked by readiness tests"]
fn test_wait_worker() {
    let root = PathBuf::from(".");
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if std::env::var_os("WAIT_CAN_EXIT").is_some()
            && root.join("exit-once").exists()
            && !root.join("exited").exists()
        {
            fs::write(root.join("exited"), "yes").unwrap();
            let code = fs::read_to_string(root.join("exit-once"))
                .unwrap()
                .parse()
                .unwrap_or(23);
            std::process::exit(code);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "subprocess fixture, invoked by readiness probes"]
fn test_wait_probe() {
    std::process::exit(if Path::new("ready").exists() { 0 } else { 1 });
}
