use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process {
    child: Child,
    stdout: tempfile::NamedTempFile,
    stderr: tempfile::NamedTempFile,
    lines: usize,
}

impl Process {
    fn spawn(root: &Path, args: &[String]) -> Self {
        let stdout = tempfile::NamedTempFile::new().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
            lines: 0,
        }
    }

    fn finish(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            assert!(
                Instant::now() < deadline,
                "process timeout: {}",
                fs::read_to_string(self.stderr.path()).unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn raw(&mut self, bytes: &[u8]) -> Value {
        self.child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
        self.next()
    }

    fn next(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = fs::read_to_string(self.stdout.path()).unwrap();
            let lines: Vec<_> = text.split_inclusive('\n').collect();
            if let Some(line) = lines.get(self.lines).filter(|s| s.ends_with('\n')) {
                self.lines += 1;
                return serde_json::from_str(line).unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "Agent response timeout: {}",
                fs::read_to_string(self.stderr.path()).unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn request(&mut self, operation: Value) -> Value {
        let id = format!("request-{}", self.lines);
        let mut bytes =
            serde_json::to_vec(&json!({"schema_version": 1, "id": id, "operation": operation}))
                .unwrap();
        bytes.push(b'\n');
        let reply = self.raw(&bytes);
        assert_eq!(reply["id"], id, "{reply}");
        assert_eq!(reply["schema_version"], 1);
        reply
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Project {
    root: tempfile::TempDir,
    _state: tempfile::TempDir,
    options: Vec<String>,
    supervisor: Option<Process>,
}

fn temporary() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("da-")
            .tempdir_in("/tmp")
            .unwrap()
    }
    #[cfg(windows)]
    {
        tempfile::tempdir().unwrap()
    }
}

impl Project {
    fn new(profile: bool) -> Self {
        let root = temporary();
        let state = temporary();
        let command = format!(
            "{} --ignored --exact test_agent_worker --nocapture",
            shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
        );
        fs::write(
            root.path().join("devd.yml"),
            serde_yaml::to_string(&json!({
                "version": "1", "services": {"worker": {"command": command,
                    "env": {"SECRET_ENV": "do-not-export-env"},
                    "restart": {"policy": "never"},
                    "paths": {"CACHE": {"scope": "instance", "path": "cache", "cleanup": true}}}},
                "profiles": {"dev": {"services": {}}}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut options = vec![
            "--state-dir".into(),
            state.path().join("state").to_str().unwrap().into(),
        ];
        if profile {
            options.extend(["--profile".into(), "dev".into()]);
        }
        let mut project = Self {
            root,
            _state: state,
            options,
            supervisor: None,
        };
        project.start();
        project
    }
    fn spawn(&self, args: &[&str]) -> Process {
        Process::spawn(
            self.root.path(),
            &[
                self.options.clone(),
                args.iter().map(|s| s.to_string()).collect(),
            ]
            .concat(),
        )
    }
    fn cli(&self, args: &[&str]) -> Value {
        let mut process = self.spawn(args);
        assert!(
            process.finish(),
            "{}",
            fs::read_to_string(process.stderr.path()).unwrap()
        );
        serde_json::from_slice(&fs::read(process.stdout.path()).unwrap()).unwrap()
    }
    fn start(&mut self) {
        self.supervisor = Some(self.spawn(&["start"]));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut process = self.spawn(&["wait", "--timeout", "100ms", "--json"]);
            if process.finish() {
                break;
            }
            assert!(Instant::now() < deadline, "supervisor startup failed");
        }
    }
    fn stop(&mut self) {
        let mut stop = self.spawn(&["stop"]);
        assert!(stop.finish());
        assert!(self.supervisor.as_mut().unwrap().finish());
        self.supervisor = None;
    }
    fn agent(&self, allow: Option<&str>) -> Process {
        let mut args = vec!["agent", "--stdio"];
        if let Some(allow) = allow {
            args.extend(["--allow", allow]);
        }
        self.spawn(&args)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if self
            .supervisor
            .as_mut()
            .is_some_and(|s| s.child.try_wait().unwrap().is_none())
        {
            let mut stop = self.spawn(&["stop"]);
            let _ = stop.finish();
            let _ = self.supervisor.as_mut().unwrap().finish();
        }
    }
}

fn data(reply: Value) -> Value {
    assert_eq!(reply["ok"], true, "{reply}");
    reply["data"].clone()
}
fn denied(reply: Value, code: &str) {
    assert_eq!(reply["ok"], false, "{reply}");
    assert_eq!(reply["error"]["code"], code, "{reply}");
}
fn target(identity: &Value) -> Value {
    json!({"instance_id": identity["instance_id"], "run_id": identity["run_id"]})
}

#[test]
fn test_agent_default_reads_and_denied_controls_leave_lifecycle_unchanged() {
    let project = Project::new(true);
    let identity = project.cli(&["identity", "--json"]);
    let before = project.cli(&["status", "--json"]);
    let mut agent = project.agent(None);
    let description = data(agent.request(json!({"method": "describe"})));
    assert_eq!(description["allowed_controls"], json!([]));
    assert_eq!(description["identity"]["profile"], "dev");
    assert_eq!(data(agent.request(json!({"method": "identity"}))), identity);
    let state = data(agent.request(json!({"method": "status"})));
    assert_eq!(
        state["services"]["worker"]["pid"],
        before["services"]["worker"]["pid"]
    );
    assert!(state["services"]["worker"].get("last_error").is_none());
    let wait = data(agent.request(json!({"method": "wait", "timeout_ms": 1000})));
    assert_eq!(wait["outcome"], "ready");
    assert_eq!(wait["run_id"], identity["run_id"]);
    let events = data(agent.request(
        json!({"method": "events", "query": {"filter": {}, "tail": 100, "cursor": null}}),
    ));
    assert!(!events["entries"].as_array().unwrap().is_empty());
    assert_eq!(
        data(agent.request(json!({"method": "explain", "service": "worker"})))["service"],
        "worker"
    );
    let export = data(agent.request(json!({"method": "export"})));
    assert!(export.get("logs").is_none());
    assert!(!export.to_string().contains("do-not-export-env"));
    for operation in [
        json!({"method": "restart", "service": "worker", "target": target(&identity)}),
        json!({"method": "stop", "target": target(&identity)}),
        json!({"method": "reload-apply", "plan_id": "invalid", "target": target(&identity)}),
        json!({"method": "clean-apply", "plan_id": "invalid", "target": target(&identity)}),
    ] {
        denied(agent.request(operation), "permission-denied");
    }
    let after_events = data(agent.request(
        json!({"method": "events", "query": {"filter": {}, "tail": 100, "cursor": null}}),
    ));
    assert_eq!(after_events["entries"], events["entries"]);
    drop(agent.child.stdin.take());
    assert!(agent.finish());
    assert_eq!(
        project.cli(&["status", "--json"])["services"]["worker"]["pid"],
        before["services"]["worker"]["pid"]
    );
}

#[test]
fn test_agent_explicit_controls_plans_and_stopped_cleanup_share_existing_rules() {
    let mut project = Project::new(false);
    let identity = project.cli(&["identity", "--json"]);
    let mut agent = project.agent(Some("restart,stop,reload,clean"));
    data(agent.request(json!({"method": "describe"})));
    let mut wrong = target(&identity);
    wrong["instance_id"] = "another-instance".into();
    denied(
        agent.request(json!({"method": "stop", "target": wrong})),
        "wrong-instance",
    );
    wrong = target(&identity);
    wrong["run_id"] = "another-run".into();
    denied(
        agent.request(json!({"method": "restart", "service": "worker", "target": wrong})),
        "stale-run",
    );
    let before = project.cli(&["status", "--json"]);
    let restarted =
        data(agent.request(
            json!({"method": "restart", "service": "worker", "target": target(&identity)}),
        ));
    assert_ne!(
        restarted["event_generation"],
        before["services"]["worker"]["event_generation"]
    );
    let original = fs::read_to_string(project.root.path().join("devd.yml")).unwrap();
    fs::write(
        project.root.path().join("candidate.yml"),
        original.replace("do-not-export-env", "changed-env"),
    )
    .unwrap();
    let plan =
        data(agent.request(json!({"method": "reload-preview", "candidate": "candidate.yml"})));
    assert_eq!(plan["apply_available"], true);
    let applied = data(agent.request(json!({"method": "reload-apply", "candidate": "candidate.yml", "plan_id": plan["plan_id"], "target": target(&identity)})));
    assert_eq!(applied["outcome"], "applied");
    denied(
        agent.request(json!({"method": "clean-preview"})),
        "operation-failed",
    );
    let stopping = data(agent.request(json!({"method": "stop", "target": target(&identity)})));
    assert_eq!(stopping["completed"], false);
    assert!(project.supervisor.as_mut().unwrap().finish());
    project.supervisor = None;
    let plan = data(agent.request(json!({"method": "clean-preview"})));
    let cache = Path::new(identity["state_dir"].as_str().unwrap()).join("runtime/cache");
    fs::write(cache.join("changed"), "changed since plan").unwrap();
    denied(agent.request(json!({"method": "clean-apply", "plan_id": plan["plan_id"], "target": target(&identity)})), "operation-failed");
    assert!(cache.exists());
    let plan = data(agent.request(json!({"method": "clean-preview"})));
    let operation =
        json!({"method": "clean-apply", "plan_id": plan["plan_id"], "target": target(&identity)});
    assert_eq!(data(agent.request(operation.clone()))["outcome"], "applied");
    assert_eq!(data(agent.request(operation))["already_applied"], true);
    assert!(!cache.exists());
}

#[test]
fn test_agent_session_cannot_follow_a_new_run_online_or_during_cleanup() {
    let mut project = Project::new(false);
    let mut agent = project.agent(Some("restart,stop,reload,clean"));
    let identity = data(agent.request(json!({"method": "identity"})));
    project.stop();
    project.start();
    let next = project.cli(&["identity", "--json"]);
    assert_ne!(next["run_id"], identity["run_id"]);
    for operation in [
        json!({"method": "status"}),
        json!({"method": "wait", "timeout_ms": 1000}),
        json!({"method": "restart", "service": "worker", "target": target(&identity)}),
        json!({"method": "stop", "target": target(&identity)}),
        json!({"method": "reload-preview"}),
    ] {
        denied(agent.request(operation), "stale-run");
    }
    project.stop();
    denied(
        agent.request(json!({"method": "clean-preview"})),
        "stale-run",
    );
    assert!(Path::new(next["state_dir"].as_str().unwrap())
        .join("runtime/cache")
        .exists());
    // Replace the stopped endpoint with a genuinely different configuration
    // identity at the same state path; the server must reject even reads.
    let state = Path::new(next["state_dir"].as_str().unwrap());
    fs::rename(state, state.with_file_name("retained-old-state")).unwrap();
    fs::copy(
        project.root.path().join("devd.yml"),
        project.root.path().join("other.yml"),
    )
    .unwrap();
    project
        .options
        .extend(["--config".into(), "other.yml".into()]);
    project.start();
    denied(agent.request(json!({"method": "status"})), "wrong-instance");
    denied(
        agent.request(json!({"method": "stop", "target": target(&identity)})),
        "wrong-instance",
    );
}

#[cfg(unix)]
#[test]
fn test_agent_signal_exits_with_idle_stdin_and_leaves_supervisor_running() {
    let project = Project::new(false);
    let before = project.cli(&["status", "--json"]);
    let mut agent = project.agent(Some("stop"));
    data(agent.request(json!({"method": "describe"})));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(agent.child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert!(agent.finish());
    assert_eq!(
        project.cli(&["status", "--json"])["services"]["worker"]["pid"],
        before["services"]["worker"]["pid"]
    );
}

#[test]
fn test_agent_malformed_inputs_never_grant_controls_and_eof_keeps_services() {
    let project = Project::new(false);
    let mut agent = project.agent(None);
    denied(agent.raw(b"not-json\n"), "invalid-request");
    denied(
        agent.raw(b"{\"schema_version\":2,\"id\":\"v\",\"operation\":{\"method\":\"status\"}}\n"),
        "unsupported-version",
    );
    denied(agent.raw(b"{\"schema_version\":1,\"id\":\"v\",\"allow\":[\"stop\"],\"operation\":{\"method\":\"status\"}}\n"), "invalid-request");
    denied(
        agent.raw(b"{\"schema_version\":1,\"id\":\"v\",\"operation\":{\"method\":\"stop\"}}\n"),
        "invalid-request",
    );
    data(agent.request(json!({"method": "status"})));
    let mut oversized = vec![b' '; 16 * 1024 + 1];
    oversized.push(b'\n');
    denied(agent.raw(&oversized), "request-too-large");
    assert!(agent.finish());
    assert!(project.cli(&["status", "--json"])["services"]["worker"]["pid"].is_number());
}

#[test]
#[ignore = "native service fixture"]
fn test_agent_worker() {
    println!("private application log");
    thread::sleep(Duration::from_secs(120));
}
