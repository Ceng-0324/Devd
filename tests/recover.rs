use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(t) = f() {
            return t;
        }
        assert!(Instant::now() < end, "fixture timed out");
        thread::sleep(Duration::from_millis(10));
    }
}
fn invoke(root: &Path, args: &[&str]) -> Output {
    let out = tempfile::tempfile().unwrap();
    let err = tempfile::tempfile().unwrap();
    let mut p = Process(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root)
            .args(args)
            .stdout(out.try_clone().unwrap())
            .stderr(err.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let status = wait(|| p.0.try_wait().unwrap());
    use std::io::{Read, Seek};
    let read = |mut file: fs::File| {
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    };
    Output {
        status,
        stdout: read(out),
        stderr: read(err),
    }
}
fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn rejected(out: Output, expected: &str) {
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(expected),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
struct Project {
    dir: tempfile::TempDir,
    options: Vec<String>,
}
impl Project {
    fn new(profile: bool) -> Self {
        #[cfg(unix)]
        let dir = tempfile::Builder::new()
            .prefix("dr-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(windows)]
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let worker = format!(
            "{} --ignored --exact test_recovery_worker --nocapture",
            shell_words::quote(exe.to_str().unwrap())
        );
        fs::write(dir.path().join("devd.yml"),serde_yaml::to_string(&json!({"version":"1","services":{"worker":{"command":worker,"restart":{"policy":"never"},"paths":{"CACHE":{"scope":"instance","path":"cache","cleanup":true},"SHARED":{"scope":"shared","path":"shared"},"KEEP":{"scope":"instance","path":"keep"}}}},"profiles":{"dev":{"services":{}}}})).unwrap()).unwrap();
        let options = if profile {
            vec![
                "--profile".into(),
                "dev".into(),
                "--state-dir".into(),
                dir.path().join("custom").to_str().unwrap().into(),
            ]
        } else {
            vec![]
        };
        let p = Self { dir, options };
        p.recipe("ok", "5s");
        p
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn state(&self) -> PathBuf {
        if self.options.is_empty() {
            self.path().join(".devd/devd.yml")
        } else {
            self.path().join("custom/profiles/dev")
        }
    }
    fn cache(&self) -> PathBuf {
        self.state().join("runtime/cache")
    }
    fn args<'a>(&'a self, args: &'a [&'a str]) -> Vec<&'a str> {
        self.options
            .iter()
            .map(String::as_str)
            .chain(args.iter().copied())
            .collect()
    }
    fn call(&self, args: &[&str]) -> Output {
        invoke(self.path(), &self.args(args))
    }
    fn recipe(&self, mode: &str, timeout: &str) {
        let command = vec![
            std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
            "--ignored".into(),
            "--exact".into(),
            "test_recovery_command".into(),
            "--nocapture".into(),
            "--".into(),
            mode.into(),
        ];
        let step = json!({"id":"generate","resource":"CACHE","command":command,"timeout":timeout,"ensures":[{"type":"file","path":"result"}]});
        fs::write(
            self.path().join("recipe.yml"),
            serde_yaml::to_string(
                &json!({"version":1,"service":"worker","resources":["CACHE"],"steps":[step]}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    fn start(&self) -> Process {
        let mut p = Process(
            Command::new(env!("CARGO_BIN_EXE_devd"))
                .current_dir(self.path())
                .args(self.args(&["start"]))
                .stdout(fs::File::create(self.path().join("stdout")).unwrap())
                .stderr(fs::File::create(self.path().join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
        wait(|| {
            if let Some(status) = p.0.try_wait().unwrap() {
                panic!(
                    "startup {status}: {}",
                    fs::read_to_string(self.path().join("stderr")).unwrap()
                );
            }
            self.call(&["wait", "--timeout", "100ms", "--json"])
                .status
                .success()
                .then_some(())
        });
        p
    }
    fn stopped(&self) {
        let mut p = self.start();
        assert!(self.call(&["stop"]).status.success());
        assert!(wait(|| p.0.try_wait().unwrap()).success());
    }
    fn preview(&self) -> Value {
        success(self.call(&["recover", "--recipe", "recipe.yml", "--dry-run", "--json"]))
    }
    fn apply(&self, plan: &Value) -> Output {
        self.call(&[
            "recover",
            "--recipe",
            "recipe.yml",
            "--apply",
            "--plan",
            plan["plan_id"].as_str().unwrap(),
            "--json",
        ])
    }
    fn receipts(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state().join("recovery-receipts.json")).unwrap())
            .unwrap()
    }
}

#[test]
fn test_recovery_cli_rebuild_current_owned_resource_and_never_replay() {
    for profile in [false, true] {
        let p = Project::new(profile);
        p.stopped();
        fs::create_dir_all(p.path().join("shared")).unwrap();
        fs::write(p.path().join("shared/user"), "preserve").unwrap();
        for path in ["events", "logs", "snapshots", "runtime/keep"] {
            fs::create_dir_all(p.state().join(path)).unwrap();
            fs::write(p.state().join(path).join("user"), "keep").unwrap();
        }
        fs::remove_dir_all(p.cache()).unwrap();
        let manifest = fs::read(p.state().join("owned-paths.json")).unwrap();
        let plan = p.preview();
        assert_eq!(plan["resources"][0]["present"], false);
        assert_eq!(plan["observation_atomic"], false);
        assert_eq!(
            fs::read(p.state().join("owned-paths.json")).unwrap(),
            manifest
        );
        assert!(!p.cache().exists());
        assert!(!serde_json::to_string(&plan)
            .unwrap()
            .contains("test_recovery_command"));
        let report = success(p.apply(&plan));
        assert_eq!(report["outcome"], "completed");
        assert_eq!(report["steps"][0]["state"], "completed");
        assert_eq!(report["rebuilt"].as_array().unwrap().len(), 1);
        assert!(p.cache().join(".devd-owner.json").exists());
        assert_eq!(
            fs::read_to_string(p.cache().join("result")).unwrap(),
            "once"
        );
        assert_eq!(success(p.apply(&plan))["already_attempted"], true);
        assert_eq!(
            fs::read_to_string(p.cache().join("result")).unwrap(),
            "once"
        );
        rejected(
            p.call(&["recover", "--recipe", "recipe.yml", "--dry-run"]),
            "already attempted",
        );
        assert_eq!(p.receipts()[0]["outcome"], "completed");
        for path in ["events", "logs", "snapshots", "runtime/keep"] {
            assert_eq!(
                fs::read_to_string(p.state().join(path).join("user")).unwrap(),
                "keep"
            );
        }
        assert_eq!(
            fs::read_to_string(p.path().join("shared/user")).unwrap(),
            "preserve"
        );
        // A new run invalidates old receipts/plans, while ordinary start still works.
        p.stopped();
        rejected(p.apply(&plan), "stale");
    }
}

#[test]
fn test_recovery_refuses_active_stale_expired_replaced_and_unregistered_resources() {
    let p = Project::new(false);
    rejected(p.call(&["recover", "--recipe", "recipe.yml"]), "required");
    let mut live = p.start();
    rejected(
        p.call(&["recover", "--recipe", "recipe.yml", "--dry-run"]),
        "active",
    );
    assert!(p.call(&["stop"]).status.success());
    assert!(wait(|| live.0.try_wait().unwrap()).success());
    let plan = p.preview();
    fs::write(p.cache().join("new"), "change").unwrap();
    rejected(p.apply(&plan), "stale");
    let plan = p.preview();
    fs::write(
        p.path().join("recipe.yml"),
        format!(
            "{}\n# new\n",
            fs::read_to_string(p.path().join("recipe.yml")).unwrap()
        ),
    )
    .unwrap();
    rejected(p.apply(&plan), "stale");
    let mut plan = p.preview();
    let id = plan["plan_id"]
        .as_str()
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .0
        .to_owned();
    plan["plan_id"] = json!(format!("{id}:1"));
    rejected(p.apply(&plan), "expired");
    let plan = p.preview();
    fs::rename(p.cache(), p.state().join("old-cache")).unwrap();
    fs::create_dir(p.cache()).unwrap();
    fs::write(p.cache().join("user"), "preserve").unwrap();
    rejected(p.apply(&plan), "replaced");
    assert_eq!(
        fs::read_to_string(p.cache().join("user")).unwrap(),
        "preserve"
    );
    fs::remove_dir_all(p.cache()).unwrap();
    fs::rename(p.state().join("old-cache"), p.cache()).unwrap();
    let recipe_path = p.path().join("recipe.yml");
    let mut recipe: Value = serde_yaml::from_slice(&fs::read(&recipe_path).unwrap()).unwrap();
    recipe["resources"] = json!(["SHARED"]);
    recipe["steps"][0]["resource"] = json!("SHARED");
    fs::write(&recipe_path, serde_yaml::to_string(&recipe).unwrap()).unwrap();
    rejected(
        p.call(&["recover", "--recipe", "recipe.yml", "--dry-run"]),
        "disposable instance",
    );
    recipe["resources"] = json!(["KEEP"]);
    recipe["steps"][0]["resource"] = json!("KEEP");
    fs::write(&recipe_path, serde_yaml::to_string(&recipe).unwrap()).unwrap();
    rejected(
        p.call(&["recover", "--recipe", "recipe.yml", "--dry-run"]),
        "disposable instance",
    );
}

#[test]
fn test_recovery_partial_failure_postconditions_timeout_and_preconditions() {
    for mode in ["fail", "no-result", "sleep", "ok"] {
        let p = Project::new(false);
        p.stopped();
        p.recipe(mode, if mode == "sleep" { "150ms" } else { "5s" });
        let mut recipe: Value =
            serde_yaml::from_slice(&fs::read(p.path().join("recipe.yml")).unwrap()).unwrap();
        let mut next = recipe["steps"][0].clone();
        next["id"] = json!("never");
        recipe["steps"].as_array_mut().unwrap().push(next);
        if mode == "ok" {
            recipe["steps"][0]["requires"] = json!([{"type":"file","path":"absent"}]);
        }
        fs::write(
            p.path().join("recipe.yml"),
            serde_yaml::to_string(&recipe).unwrap(),
        )
        .unwrap();
        let plan = p.preview();
        let out = p.apply(&plan);
        assert!(!out.status.success());
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_ne!(report["outcome"], "completed");
        if mode == "ok" {
            assert_eq!(report["outcome"], "failed");
            assert_eq!(report["steps"][0]["state"], "failed");
            assert!(!p.cache().join("started").exists());
        }
        assert!(report["steps"].as_array().unwrap().len() <= 1);
        assert_eq!(p.receipts().as_array().unwrap().len(), 1);
        let second = p.apply(&plan);
        assert!(!second.status.success());
        let second: Value = serde_json::from_slice(&second.stdout).unwrap();
        assert_eq!(second["already_attempted"], true);
        if mode == "sleep" {
            assert_eq!(report["outcome"], "timed-out");
            assert_eq!(report["steps"][0]["state"], "timed-out");
            p.stopped();
        }
    }
}

#[test]
fn test_recovery_rejects_links_in_resource_conditions_and_recipe_input() {
    let p = Project::new(false);
    p.stopped();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "preserve").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), p.cache().join("linked")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), p.cache().join("linked"))
        .expect("native Windows validation requires symlinks");
    rejected(
        p.call(&["recover", "--recipe", "recipe.yml", "--dry-run"]),
        "link or special",
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("secret")).unwrap(),
        "preserve"
    );
    fs::remove_file(p.cache().join("linked"))
        .or_else(|_| fs::remove_dir(p.cache().join("linked")))
        .unwrap();
    fs::hard_link(p.path().join("recipe.yml"), p.path().join("hard")).unwrap();
    rejected(
        p.call(&["recover", "--recipe", "recipe.yml", "--dry-run"]),
        "single-link",
    );
}

#[test]
fn test_recovery_running_command_holds_lock_and_crash_is_unknown_without_replay() {
    let p = Project::new(false);
    p.stopped();
    p.recipe("sleep", "20s");
    let plan = p.preview();
    let mut recover = Process(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(p.path())
            .args([
                "recover",
                "--recipe",
                "recipe.yml",
                "--apply",
                "--plan",
                plan["plan_id"].as_str().unwrap(),
                "--json",
            ])
            .stdout(fs::File::create(p.path().join("recover-out")).unwrap())
            .stderr(fs::File::create(p.path().join("recover-err")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait(|| p.cache().join("started").exists().then_some(()));
    rejected(p.call(&["start"]), "state");
    rejected(p.call(&["clean", "--dry-run"]), "active");
    recover.0.kill().unwrap();
    recover.0.wait().unwrap();
    // The test owns the orphan fixture; devd never consults this PID for control.
    #[cfg(unix)]
    {
        let pid = fs::read_to_string(p.cache().join("started"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let out = p.apply(&plan);
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["outcome"], "unknown");
    assert_eq!(report["steps"][0]["state"], "unknown");
    assert_eq!(report["already_attempted"], true);
    rejected(p.call(&["start"]), "manual reconciliation");
    rejected(p.call(&["clean", "--dry-run"]), "did not finish safely");
}

#[cfg(unix)]
#[test]
fn test_recovery_cancellation_stops_command_and_retains_receipt() {
    let p = Project::new(false);
    p.stopped();
    p.recipe("sleep", "20s");
    let plan = p.preview();
    let out = tempfile::NamedTempFile::new().unwrap();
    let mut recover = Process(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(p.path())
            .args([
                "recover",
                "--recipe",
                "recipe.yml",
                "--apply",
                "--plan",
                plan["plan_id"].as_str().unwrap(),
                "--json",
            ])
            .stdout(out.reopen().unwrap())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait(|| p.cache().join("started").exists().then_some(()));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(recover.0.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    assert!(!wait(|| recover.0.try_wait().unwrap()).success());
    let report: Value = serde_json::from_slice(&fs::read(out.path()).unwrap()).unwrap();
    assert_eq!(report["outcome"], "cancelled");
    assert_eq!(report["steps"][0]["state"], "cancelled");
    assert_eq!(p.receipts()[0]["outcome"], "cancelled");
    p.stopped();
}

#[test]
#[ignore = "executable supervisor fixture"]
fn test_recovery_worker() {
    thread::sleep(Duration::from_secs(60));
}

#[test]
#[ignore = "executable recovery command fixture"]
fn test_recovery_command() {
    let mode = std::env::args().next_back().unwrap();
    fs::write("started", std::process::id().to_string()).unwrap();
    println!("private-command-output-do-not-record");
    match mode.as_str() {
        "sleep" => thread::sleep(Duration::from_secs(60)),
        "fail" => panic!("deliberate failure"),
        "no-result" => {}
        "replace-next" => {
            let target = fs::read_to_string("next-program").unwrap();
            let source = std::env::current_exe().unwrap();
            fs::remove_file(&target).unwrap();
            fs::copy(source, target).unwrap();
            fs::write("result", "once").unwrap();
        }
        _ => {
            use std::io::Write;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("result")
                .unwrap()
                .write_all(b"once")
                .unwrap();
        }
    }
}

#[test]
fn test_recovery_multistep_success_and_argv_is_literal() {
    let p = Project::new(false);
    p.stopped();
    let path = p.path().join("recipe.yml");
    let mut recipe: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    let literal = "spaces ' quotes $VAR $(touch injected) `touch injected` ; > injected";
    recipe["steps"][0]["command"]
        .as_array_mut()
        .unwrap()
        .push(json!(literal));
    let mut next = recipe["steps"][0].clone();
    next["id"] = json!("second");
    next["requires"] = json!([{"type":"file","path":"result"}]);
    recipe["steps"].as_array_mut().unwrap().push(next);
    fs::write(path, serde_yaml::to_string(&recipe).unwrap()).unwrap();
    let plan = p.preview();
    let out = p.apply(&plan);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("private-command-output"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("private-command-output"));
    let report = success(out);
    assert_eq!(report["steps"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(p.cache().join("result")).unwrap(),
        "onceonce"
    );
    assert!(!p.cache().join("injected").exists());
}

#[test]
fn test_recovery_preparation_rejects_program_and_config_replacement() {
    let p = Project::new(false);
    p.stopped();
    let program = p.path().join("program-copy");
    fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
    let path = p.path().join("recipe.yml");
    let mut recipe: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    recipe["steps"][0]["command"][0] = json!(program);
    fs::write(&path, serde_yaml::to_string(&recipe).unwrap()).unwrap();
    let plan = p.preview();
    let next = p.path().join("program-new");
    fs::copy(std::env::current_exe().unwrap(), &next).unwrap();
    fs::remove_file(&program).unwrap();
    fs::rename(next, &program).unwrap();
    rejected(p.apply(&plan), "stale");
    let plan = p.preview();
    fs::write(
        p.path().join("devd.yml"),
        format!(
            "{}\n# changed\n",
            fs::read_to_string(p.path().join("devd.yml")).unwrap()
        ),
    )
    .unwrap();
    rejected(p.apply(&plan), "stale");
    assert!(!p.cache().join("result").exists());
}

#[test]
fn test_recovery_program_replacement_between_steps_is_never_accepted() {
    let p = Project::new(false);
    p.stopped();
    p.recipe("replace-next", "5s");
    let next_program = p.path().join("next-program-copy");
    fs::copy(std::env::current_exe().unwrap(), &next_program).unwrap();
    fs::write(
        p.cache().join("next-program"),
        next_program.to_str().unwrap(),
    )
    .unwrap();
    let path = p.path().join("recipe.yml");
    let mut recipe: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut next = recipe["steps"][0].clone();
    next["id"] = json!("never");
    next["command"][0] = json!(next_program);
    *next["command"].as_array_mut().unwrap().last_mut().unwrap() = json!("ok");
    recipe["steps"].as_array_mut().unwrap().push(next);
    fs::write(path, serde_yaml::to_string(&recipe).unwrap()).unwrap();
    let plan = p.preview();
    let out = p.apply(&plan);
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["steps"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(p.cache().join("result")).unwrap(),
        "once"
    );
}

#[test]
fn test_recovery_preview_stdout_backpressure_is_bounded() {
    let p = Project::new(false);
    p.stopped();
    let path = p.path().join("recipe.yml");
    let mut recipe: Value = serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    let base = recipe["steps"][0].clone();
    recipe["steps"] = json!([]);
    for i in 0..32 {
        let mut step = base.clone();
        step["id"] = json!(format!("step-{i}"));
        step["ensures"] = Value::Array(
            (0..32)
                .map(|j| json!({"type":"file","path":format!("{j}-{}", "x".repeat(200))}))
                .collect(),
        );
        recipe["steps"].as_array_mut().unwrap().push(step);
    }
    fs::write(path, serde_yaml::to_string(&recipe).unwrap()).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(p.path())
            .args(["recover", "--recipe", "recipe.yml", "--dry-run", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(!wait(|| process.0.try_wait().unwrap()).success());
    assert!(!p.state().join("recovery-receipts.json").exists());
    p.stopped();
}
