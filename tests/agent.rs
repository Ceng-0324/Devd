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
        let mut project = Self::prepare(temporary(), profile);
        project.start();
        project
    }

    fn prepare(root: tempfile::TempDir, profile: bool) -> Self {
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
        Self {
            root,
            _state: state,
            options,
            supervisor: None,
        }
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
fn test_agent_cleanup_protects_config_relative_shared_paths_from_foreign_cwd() {
    let mut project = Project::new(true);
    let foreign = temporary();
    let foreign_cwd = foreign.path().join("nested");
    fs::create_dir(&foreign_cwd).unwrap();
    let config = project.root.path().join("devd.yml").canonicalize().unwrap();
    let mut arguments = project.options.clone();
    arguments.extend([
        "--config".into(),
        config.to_str().unwrap().into(),
        "agent".into(),
        "--stdio".into(),
        "--allow".into(),
        "clean".into(),
    ]);
    let mut agent = Process::spawn(&foreign_cwd, &arguments);
    let identity = data(agent.request(json!({"method": "identity"})));
    project.stop();
    let plan = data(agent.request(json!({"method": "clean-preview"})));
    let state = Path::new(identity["state_dir"].as_str().unwrap());
    let cache = state.join("runtime/cache");
    fs::write(cache.join("sentinel"), "shared data").unwrap();
    let journal = fs::read(state.join("owned-paths.json")).unwrap();
    let shared = Path::new("..").join(
        cache
            .strip_prefix(config.parent().unwrap().parent().unwrap())
            .unwrap(),
    );
    let mut yaml: Value = serde_yaml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    yaml["services"]["worker"]["paths"]["SHARED"] = json!({"scope": "shared", "path": shared});
    fs::write(&config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    for operation in [
        json!({"method": "clean-preview"}),
        json!({"method": "clean-apply", "plan_id": plan["plan_id"], "target": target(&identity)}),
    ] {
        let reply = agent.request(operation);
        denied(reply.clone(), "operation-failed");
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("overlaps a shared mapping"),
            "{reply}"
        );
    }
    assert_eq!(
        fs::read_to_string(cache.join("sentinel")).unwrap(),
        "shared data"
    );
    assert_eq!(fs::read(state.join("owned-paths.json")).unwrap(), journal);
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
fn test_adapters_signal_exits_with_idle_stdin_and_leaves_supervisor_running() {
    let project = Project::new(false);
    let before = project.cli(&["status", "--json"]);
    for adapter in ["agent", "mcp"] {
        let mut process = project.spawn(&[adapter, "--stdio", "--allow", "stop"]);
        if adapter == "mcp" {
            process.initialize("2025-11-25");
        } else {
            data(process.request(json!({"method":"describe"})));
        }
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(process.child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        assert!(process.finish());
    }
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
    let listener = if let Ok(path) = std::env::var("M8_BINDING_REPORT") {
        let listener = std::net::TcpListener::bind(std::env::var("API_ADDR").unwrap()).unwrap();
        let bindings = json!({"cache": std::env::var("CACHE").unwrap(),
            "shared": std::env::var("SHARED").unwrap(), "address": std::env::var("API_ADDR").unwrap()});
        fs::write(path, serde_json::to_vec(&bindings).unwrap()).unwrap();
        Some(listener)
    } else {
        None
    };
    println!("private application log");
    if let Some(listener) = listener {
        for stream in listener.incoming() {
            drop(stream.unwrap());
        }
    } else {
        thread::sleep(Duration::from_secs(120));
    }
}

#[test]
fn test_two_worktrees_keep_agent_controls_reports_and_cleanup_isolated() {
    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let first = temporary();
    let second = temporary();
    git(first.path(), &["init", "-b", "main"]);
    git(
        first.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    git(
        first.path(),
        &[
            "worktree",
            "add",
            "--detach",
            second.path().to_str().unwrap(),
        ],
    );
    let shared = temporary();
    fs::write(shared.path().join("sentinel"), "shared across worktrees").unwrap();
    let mut a = Project::prepare(first, true);
    let mut b = Project::prepare(second, true);
    for project in [&mut a, &mut b] {
        let config = project.root.path().join("devd.yml");
        let mut yaml: Value = serde_yaml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        yaml["services"]["worker"]["paths"]["SHARED"] =
            json!({"scope": "shared", "path": shared.path()});
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        yaml["services"]["worker"]["ports"] = json!({"API_ADDR": address});
        yaml["services"]["worker"]["healthcheck"] = json!({"type": "tcp", "port": address.port(), "interval": "100ms", "timeout": "1s", "retries": 100});
        yaml["services"]["worker"]["env"]["M8_BINDING_REPORT"] =
            json!(project.root.path().join("bindings.json"));
        fs::write(config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
        drop(reservation);
        project.start();
    }
    let ia = a.cli(&["identity", "--json"]);
    let ib = b.cli(&["identity", "--json"]);
    assert_ne!(ia["instance_id"], ib["instance_id"]);
    assert_ne!(ia["state_dir"], ib["state_dir"]);
    assert_eq!(ia["git"]["common_dir"], ib["git"]["common_dir"]);
    for project in [&a, &b] {
        let mut discovered =
            Process::spawn(project.root.path(), &["instances".into(), "--json".into()]);
        assert!(discovered.finish());
        let report: Value =
            serde_json::from_slice(&fs::read(discovered.stdout.path()).unwrap()).unwrap();
        assert_eq!(report["entries"].as_array().unwrap().len(), 2);
        assert!(report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["status"] == "live"));
        let deadline = Instant::now() + Duration::from_secs(10);
        let bindings: Value = loop {
            if let Ok(bytes) = fs::read(project.root.path().join("bindings.json")) {
                if let Ok(value) = serde_json::from_slice(&bytes) {
                    break value;
                }
            }
            assert!(Instant::now() < deadline, "binding report missing");
            thread::sleep(Duration::from_millis(10));
        };
        let identity = project.cli(&["identity", "--json"]);
        assert_eq!(
            Path::new(bindings["cache"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            Path::new(identity["state_dir"].as_str().unwrap())
                .join("runtime/cache")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            Path::new(bindings["shared"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            shared.path().canonicalize().unwrap()
        );
        let address: std::net::SocketAddr = bindings["address"].as_str().unwrap().parse().unwrap();
        std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    }
    let before_b = b.cli(&["status", "--json"]);
    let mut agent = a.agent(Some("restart,stop,reload,clean"));
    data(agent.request(json!({"method": "identity"})));
    denied(
        agent.request(json!({"method": "stop", "target": target(&ib)})),
        "wrong-instance",
    );
    data(agent.request(json!({"method": "restart", "service": "worker", "target": target(&ia)})));
    assert_eq!(
        data(agent.request(json!({"method": "wait", "timeout_ms": 10000})))["outcome"],
        "ready"
    );
    let original = fs::read_to_string(a.root.path().join("devd.yml")).unwrap();
    fs::write(
        a.root.path().join("candidate.yml"),
        original.replace("do-not-export-env", "m8-reloaded"),
    )
    .unwrap();
    let plan =
        data(agent.request(json!({"method": "reload-preview", "candidate": "candidate.yml"})));
    assert_eq!(data(agent.request(json!({"method": "reload-apply", "candidate": "candidate.yml", "plan_id": plan["plan_id"], "target": target(&ia)})))["outcome"], "applied");
    let report = data(agent.request(json!({"method": "export"})));
    assert_eq!(report["identity"]["run_id"], ia["run_id"]);
    assert_eq!(report["explanations"]["worker"]["conclusion"], "healthy");
    let events = report["events"]["entries"].as_array().unwrap();
    assert!(events
        .iter()
        .any(|event| event["data"]["type"] == "reload-finished"));
    assert!(events.iter().all(|event| event["run_id"] == ia["run_id"]));
    a.stop();
    let plan = data(agent.request(json!({"method": "clean-preview"})));
    assert_eq!(
        data(agent.request(
            json!({"method": "clean-apply", "plan_id": plan["plan_id"], "target": target(&ia)})
        ))["outcome"],
        "applied"
    );
    assert!(!Path::new(ia["state_dir"].as_str().unwrap())
        .join("runtime/cache")
        .exists());
    assert!(Path::new(ib["state_dir"].as_str().unwrap())
        .join("runtime/cache")
        .exists());
    assert_eq!(
        fs::read_to_string(shared.path().join("sentinel")).unwrap(),
        "shared across worktrees"
    );
    let after_b = b.cli(&["status", "--json"]);
    for field in ["pid", "event_generation", "restart_count"] {
        assert_eq!(
            before_b["services"]["worker"][field],
            after_b["services"]["worker"][field]
        );
    }
    assert_eq!(b.cli(&["wait", "--json"])["outcome"], "ready");
    a.start();
    denied(
        agent.request(json!({"method": "stop", "target": target(&ia)})),
        "stale-run",
    );
    let current = a.cli(&["identity", "--json"]);
    assert_eq!(current["instance_id"], ia["instance_id"]);
    assert_ne!(current["run_id"], ia["run_id"]);
    let mut new_agent = a.agent(None);
    assert_eq!(
        data(new_agent.request(json!({"method": "identity"})))["run_id"],
        current["run_id"]
    );
    a.stop();
    b.stop();
}

// Both adapters deliberately use this same real-supervisor fixture.
impl Process {
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = format!("rpc-{}", self.lines);
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .unwrap();
        bytes.push(b'\n');
        let reply = self.raw(&bytes);
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }
    fn notify(&mut self, method: &str, params: Value) {
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","method":method,"params":params})
        )
        .unwrap();
    }
    fn initialize(&mut self, version: &str) -> Value {
        let reply = self.rpc("initialize", json!({"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"devd-native-tests","version":"1"}}));
        assert!(
            reply["result"]["capabilities"]["tools"].is_object(),
            "{reply}"
        );
        self.notify("notifications/initialized", json!({}));
        reply
    }
    fn tool(&mut self, name: &str, args: Value) -> Value {
        self.rpc("tools/call", json!({"name":name,"arguments":args}))
    }
}
fn mcp_data(reply: Value) -> Value {
    assert!(reply.get("error").is_none(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
fn mcp_denied(reply: Value, code: &str) {
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    let data: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["code"], code, "{data}");
}

#[test]
fn test_mcp_discovery_readonly_strict_arguments_and_protocol_errors() {
    let project = Project::new(true);
    let before = project.cli(&["status", "--json"]);
    let mut mcp = project.spawn(&["mcp", "--stdio"]);
    assert_eq!(
        mcp.initialize("2099-01-01")["result"]["protocolVersion"],
        "2025-11-25"
    );
    let catalog = mcp.rpc("tools/list", json!({}));
    assert_eq!(catalog["result"]["tools"].as_array().unwrap().len(), 14);
    let identity = mcp_data(mcp.tool("devd_identity", json!({})));
    assert_eq!(identity["profile"], "dev");
    for (name, args) in [
        ("devd_status", json!({})),
        ("devd_events", json!({"tail":10})),
        ("devd_explain", json!({"service":"worker"})),
        (
            "devd_wait",
            json!({"services":["worker"],"timeout_ms":2000}),
        ),
        ("devd_export", json!({})),
    ] {
        assert!(mcp_data(mcp.tool(name, args)).is_object());
    }
    mcp_denied(
        mcp.tool("devd_incidents_list", json!({})),
        "operation-failed",
    );
    for (name, args) in [
        (
            "devd_restart",
            json!({"target":target(&identity),"service":"worker"}),
        ),
        ("devd_stop", json!({"target":target(&identity)})),
        (
            "devd_reload_apply",
            json!({"target":target(&identity),"plan_id":"bad"}),
        ),
        (
            "devd_clean_apply",
            json!({"target":target(&identity),"plan_id":"bad"}),
        ),
    ] {
        mcp_denied(mcp.tool(name, args), "permission-denied");
    }
    for (name, args) in [
        ("devd_status", json!({"allow":["stop"]})),
        ("devd_stop", json!({})),
        ("devd_wait", json!({"services":42})),
        ("devd_export", json!({"include_logs":"true"})),
        ("devd_events", json!({"tail":1001})),
        ("devd_incidents_show", json!({"id":"invalid"})),
        ("devd_status", json!({"method":"stop"})),
        ("devd_diagnose", json!({"path":"/etc/passwd"})),
    ] {
        assert_eq!(mcp.tool(name, args)["error"]["code"], -32602);
    }
    assert_eq!(mcp.raw(b"not-json\n")["error"]["code"], -32700);
    assert_eq!(mcp.rpc("unknown", json!({}))["error"]["code"], -32601);
    assert_eq!(
        project.cli(&["status", "--json"])["services"]["worker"]["pid"],
        before["services"]["worker"]["pid"]
    );
    drop(mcp.child.stdin.take());
    assert!(mcp.finish());
}

#[test]
fn test_mcp_grants_stale_targets_reload_plans_and_stopped_clean() {
    let mut project = Project::new(false);
    let mut mcp = project.spawn(&["mcp", "--stdio", "--allow", "restart,stop,reload,clean"]);
    assert_eq!(
        mcp.initialize("2025-06-18")["result"]["protocolVersion"],
        "2025-06-18"
    );
    let identity = mcp_data(mcp.tool("devd_identity", json!({})));
    let mut wrong = target(&identity);
    wrong["run_id"] = "old".into();
    mcp_denied(
        mcp.tool("devd_restart", json!({"target":wrong,"service":"worker"})),
        "stale-run",
    );
    let mut wrong = target(&identity);
    wrong["instance_id"] = "other".into();
    mcp_denied(
        mcp.tool("devd_stop", json!({"target":wrong})),
        "wrong-instance",
    );
    let plan = mcp_data(mcp.tool("devd_reload_preview", json!({})));
    let before = project.cli(&["status", "--json"])["services"]["worker"]["pid"].clone();
    mcp_data(mcp.tool(
        "devd_restart",
        json!({"target":target(&identity),"service":"worker"}),
    ));
    project.cli(&["wait", "--timeout", "5s", "--json"]);
    assert_ne!(
        project.cli(&["status", "--json"])["services"]["worker"]["pid"],
        before
    );
    let stale = mcp.tool(
        "devd_reload_apply",
        json!({"target":target(&identity),"plan_id":plan["plan_id"]}),
    );
    assert!(
        serde_json::from_str::<Value>(stale["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("stale")
    );
    mcp_denied(stale, "operation-failed");
    let plan = mcp_data(mcp.tool("devd_reload_preview", json!({})));
    mcp_data(mcp.tool(
        "devd_reload_apply",
        json!({"target":target(&identity),"plan_id":plan["plan_id"]}),
    ));
    mcp_data(mcp.tool("devd_stop", json!({"target":target(&identity)})));
    assert!(project.supervisor.as_mut().unwrap().finish());
    project.supervisor = None;
    let plan = mcp_data(mcp.tool("devd_clean_preview", json!({})));
    mcp_data(mcp.tool(
        "devd_clean_apply",
        json!({"target":target(&identity),"plan_id":plan["plan_id"]}),
    ));
    assert!(!project.root.path().join("cache").exists());
    project.start();
    mcp_denied(mcp.tool("devd_status", json!({})), "stale-run");
    mcp_denied(
        mcp.tool(
            "devd_restart",
            json!({"target":target(&identity),"service":"worker"}),
        ),
        "stale-run",
    );
    drop(mcp.child.stdin.take());
    assert!(mcp.finish());
}

#[test]
fn test_mcp_offline_report_is_isolated_and_rejects_control_grants() {
    let mut project = Project::new(false);
    let report = project.root.path().join("report.json");
    let mut export = project.spawn(&["export", "--output", report.to_str().unwrap()]);
    assert!(export.finish());
    project.stop();
    fs::remove_file(project.root.path().join("devd.yml")).unwrap();
    let mut mcp = Process::spawn(
        project.root.path(),
        &[
            "mcp".into(),
            "--stdio".into(),
            "--report".into(),
            report.to_str().unwrap().into(),
        ],
    );
    mcp.initialize("2025-11-25");
    assert_eq!(
        mcp.rpc("tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let diagnosis = mcp_data(mcp.tool("devd_diagnose", json!({})));
    assert_eq!(diagnosis["source"], "imported-report");
    assert_eq!(
        mcp.tool("devd_diagnose", json!({"path":"/etc/passwd"}))["error"]["code"],
        -32602
    );
    assert_eq!(mcp.tool("devd_stop", json!({}))["error"]["code"], -32602);
    drop(mcp.child.stdin.take());
    assert!(mcp.finish());
    let mut denied = Process::spawn(
        project.root.path(),
        &[
            "mcp".into(),
            "--stdio".into(),
            "--report".into(),
            report.to_str().unwrap().into(),
            "--allow".into(),
            "stop".into(),
        ],
    );
    assert!(!denied.finish());
}

#[test]
fn test_mcp_wait_cancellation_eof_and_input_admission_do_not_stop_services() {
    let mut project = Project::prepare(temporary(), false);
    let path = project.root.path().join("devd.yml");
    let mut yaml: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    yaml["services"]["worker"]["healthcheck"] =
        json!({"type":"tcp","port":port,"interval":"100ms","timeout":"50ms","retries":1});
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    project.supervisor = Some(project.spawn(&["start"]));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut identity = project.spawn(&["identity", "--json"]);
        if identity.finish() {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    let mut mcp = project.spawn(&["mcp", "--stdio"]);
    mcp.initialize("2025-11-25");
    // More cancellations than the outstanding request limit must retire their
    // slots; ping must remain responsive while wait is in progress.
    for n in 0..12 {
        let id = format!("waiting-{n}");
        writeln!(mcp.child.stdin.as_mut().unwrap(), "{}", json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"devd_wait","arguments":{"timeout_ms":60000}}})).unwrap();
        assert!(mcp.rpc("ping", json!({}))["result"].is_object());
        mcp.notify("notifications/cancelled", json!({"requestId":id}));
        assert!(mcp.rpc("ping", json!({}))["result"].is_object());
    }
    writeln!(mcp.child.stdin.as_mut().unwrap(), "{}", json!({"jsonrpc":"2.0","id":"eof-wait","method":"tools/call","params":{"name":"devd_wait","arguments":{"timeout_ms":60000}}})).unwrap();
    drop(mcp.child.stdin.take());
    assert!(mcp.finish());
    assert!(project.cli(&["identity", "--json"])["run_id"].is_string());
    let mut large = project.spawn(&["mcp", "--stdio"]);
    large.initialize("2025-11-25");
    large
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&vec![b'x'; 16 * 1024 + 1])
        .unwrap();
    assert!(large.finish());
    let mut duplicate = project.spawn(&["mcp", "--stdio"]);
    duplicate.initialize("2025-11-25");
    let line = json!({"jsonrpc":"2.0","id":"same","method":"tools/call","params":{"name":"devd_wait","arguments":{"timeout_ms":60000}}});
    writeln!(duplicate.child.stdin.as_mut().unwrap(), "{line}\n{line}").unwrap();
    assert!(duplicate.finish());
    assert!(project.cli(&["identity", "--json"])["run_id"].is_string());
}
