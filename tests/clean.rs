use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_some() {
            return;
        }
        #[cfg(unix)]
        {
            use nix::{
                sys::signal::{kill, Signal},
                unistd::Pid,
            };
            let _ = kill(Pid::from_raw(self.0.id() as i32), Signal::SIGTERM);
        }
        #[cfg(windows)]
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn invoke(root: &Path, args: &[&str]) -> Output {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    let mut process = Process(
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
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI timed out: {args:?}");
        thread::sleep(Duration::from_millis(10));
    };
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
    serde_json::from_slice(&output.stdout).unwrap()
}

fn rejected(output: Output, message: &str) {
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(message), "{error}");
}

fn project() -> tempfile::TempDir {
    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("dc-")
        .tempdir_in("/tmp")
        .unwrap();
    #[cfg(windows)]
    let root = tempfile::tempdir().unwrap();
    let command = format!(
        "{} --ignored --exact test_clean_worker --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    );
    fs::write(root.path().join("devd.yml"), serde_yaml::to_string(&json!({
        "version": "1", "services": {"worker": {"command": command,
            "paths": {"CACHE": {"scope": "instance", "path": "cache", "cleanup": true},
                "KEEP": {"scope": "instance", "path": "keep"}, "SHARED": {"scope": "shared", "path": "shared"}},
            "restart": {"policy": "never"}}},
        "profiles": {"dev": {"services": {}}}
    })).unwrap()).unwrap();
    root
}

fn start(root: &Path, options: &[&str]) -> Process {
    let process = Process(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .current_dir(root)
            .arg("start")
            .args(options)
            .stdout(fs::File::create(root.join("stdout")).unwrap())
            .stderr(fs::File::create(root.join("stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = invoke(
            root,
            &[options, &["wait", "--timeout", "100ms", "--json"]].concat(),
        );
        if output.status.success() {
            return process;
        }
        assert!(
            Instant::now() < deadline,
            "startup failed: {}",
            fs::read_to_string(root.join("stderr")).unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn stop(root: &Path, process: &mut Process, options: &[&str]) {
    assert!(invoke(root, &[options, &["stop"]].concat())
        .status
        .success());
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        assert!(Instant::now() < deadline, "shutdown timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn test_clean_cli_plans_are_current_explicit_and_idempotent() {
    let root = project();
    let root = root.path();
    let state = root.join(".devd/devd.yml");
    let cache = state.join("runtime/cache");
    rejected(invoke(root, &["clean"]), "required");
    rejected(invoke(root, &["clean", "--apply"]), "--plan");
    let mut process = start(root, &[]);
    assert!(cache.join(".devd-owner.json").exists());
    rejected(invoke(root, &["clean", "--dry-run"]), "active");
    // Cleanup authorization changes cannot be smuggled into a live reload.
    let original = fs::read_to_string(root.join("devd.yml")).unwrap();
    fs::write(
        root.join("candidate.yml"),
        original.replace("cleanup: true", "cleanup: false"),
    )
    .unwrap();
    let reload = success(invoke(
        root,
        &[
            "reload",
            "--dry-run",
            "--candidate",
            "candidate.yml",
            "--json",
        ],
    ));
    assert_eq!(reload["apply_available"], false);
    rejected(
        invoke(
            root,
            &[
                "reload",
                "--apply",
                "--candidate",
                "candidate.yml",
                "--plan",
                reload["plan_id"].as_str().unwrap(),
            ],
        ),
        "fixed for a supervisor run",
    );
    stop(root, &mut process, &[]);
    for relative in ["runtime/keep", "logs", "events", "snapshots"] {
        fs::create_dir_all(state.join(relative)).unwrap();
        fs::write(state.join(relative).join("sentinel"), "preserve").unwrap();
    }
    fs::create_dir(root.join("shared")).unwrap();
    fs::write(root.join("shared/user"), "shared data").unwrap();
    fs::write(cache.join("generated"), "disposable data").unwrap();
    let journal = fs::read(state.join("owned-paths.json")).unwrap();
    let plan = success(invoke(root, &["clean", "--dry-run", "--json"]));
    assert_eq!(plan["resources"].as_array().unwrap().len(), 1);
    assert_eq!(plan["schema_version"], 1);
    assert_eq!(fs::read(state.join("owned-paths.json")).unwrap(), journal);
    let id = plan["plan_id"].as_str().unwrap();
    fs::write(cache.join("new"), "new file").unwrap();
    rejected(invoke(root, &["clean", "--apply", "--plan", id]), "stale");
    let plan = success(invoke(root, &["clean", "--dry-run", "--json"]));
    fs::write(
        root.join("devd.yml"),
        format!("{original}\n# changed since preview\n"),
    )
    .unwrap();
    rejected(
        invoke(
            root,
            &[
                "clean",
                "--apply",
                "--plan",
                plan["plan_id"].as_str().unwrap(),
            ],
        ),
        "stale",
    );
    let plan = success(invoke(root, &["clean", "--dry-run", "--json"]));
    let mut process = start(root, &[]);
    stop(root, &mut process, &[]);
    rejected(
        invoke(
            root,
            &[
                "clean",
                "--apply",
                "--plan",
                plan["plan_id"].as_str().unwrap(),
            ],
        ),
        "stale",
    );
    let plan = success(invoke(root, &["clean", "--dry-run", "--json"]));
    let apply = [
        "clean",
        "--apply",
        "--plan",
        plan["plan_id"].as_str().unwrap(),
        "--json",
    ];
    assert_eq!(success(invoke(root, &apply))["outcome"], "applied");
    assert!(!cache.exists());
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("user"), "new user directory").unwrap();
    assert_eq!(success(invoke(root, &apply))["already_applied"], true);
    rejected(invoke(root, &["start"]), "refusing to adopt");
    assert!(cache.join("user").exists());
    for relative in ["runtime/keep", "logs", "events", "snapshots"] {
        assert_eq!(
            fs::read_to_string(state.join(relative).join("sentinel")).unwrap(),
            "preserve"
        );
    }
    assert_eq!(
        fs::read_to_string(root.join("shared/user")).unwrap(),
        "shared data"
    );
}

#[test]
fn test_clean_profile_custom_state_and_links_preserve_external_data() {
    let root = project();
    let external = tempfile::tempdir().unwrap();
    let state_arg = external.path().join("custom");
    let options = [
        "--profile",
        "dev",
        "--state-dir",
        state_arg.to_str().unwrap(),
    ];
    let mut process = start(root.path(), &options);
    stop(root.path(), &mut process, &options);
    let identity = success(invoke(
        root.path(),
        &[&options[..], &["clean", "--dry-run", "--json"]].concat(),
    ));
    let state = std::path::PathBuf::from(identity["state_dir"].as_str().unwrap());
    let cache = state.join("runtime/cache");
    assert!(cache.exists(), "{}", state.display());
    let outside = external.path().join("user");
    fs::write(&outside, "protected").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, cache.join("link")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside, cache.join("link"))
        .expect("native Windows CI must support symlinks");
    rejected(
        invoke(
            root.path(),
            &[&options[..], &["clean", "--dry-run"]].concat(),
        ),
        "link or special file",
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "protected");
    fs::remove_file(cache.join("link")).unwrap();
    let plan = success(invoke(
        root.path(),
        &[&options[..], &["clean", "--dry-run", "--json"]].concat(),
    ));
    assert_eq!(plan["profile"], "dev");
    assert_eq!(
        success(invoke(
            root.path(),
            &[
                &options[..],
                &[
                    "clean",
                    "--apply",
                    "--plan",
                    plan["plan_id"].as_str().unwrap(),
                    "--json"
                ]
            ]
            .concat()
        ))["outcome"],
        "applied"
    );
    assert!(!cache.exists());
    assert!(outside.exists());
}

#[test]
#[ignore = "subprocess fixture for native cleanup lifecycle tests"]
fn test_clean_worker() {
    assert!(Path::new(&std::env::var_os("CACHE").unwrap())
        .join(".devd-owner.json")
        .exists());
    thread::sleep(Duration::from_secs(60));
}

#[test]
fn test_clean_refuses_abrupt_supervisor_death_without_signalling_historical_pids() {
    let root = project();
    let mut process = start(root.path(), &[]);
    let snapshot = success(invoke(root.path(), &["status", "--json"]));
    assert!(snapshot["services"]["worker"]["pid"].is_number());
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    // The test owns this fixture process; devd clean must never do this from
    // persisted PIDs. Windows already closes the service's Job on parent death.
    #[cfg(unix)]
    {
        use nix::{
            sys::signal::{killpg, Signal},
            unistd::Pid,
        };
        let _ = killpg(
            Pid::from_raw(snapshot["services"]["worker"]["pid"].as_u64().unwrap() as i32),
            Signal::SIGTERM,
        );
    }
    rejected(
        invoke(root.path(), &["clean", "--dry-run"]),
        "did not finish safely",
    );
    rejected(invoke(root.path(), &["start"]), "manual reconciliation");
    assert!(root
        .path()
        .join(".devd/devd.yml/runtime/cache/.devd-owner.json")
        .exists());
}

#[cfg(windows)]
#[test]
fn test_clean_windows_partial_failure_can_be_previewed_and_retried() {
    let root = project();
    let mut process = start(root.path(), &[]);
    stop(root.path(), &mut process, &[]);
    let cache = root.path().join(".devd/devd.yml/runtime/cache");
    let protected = cache.join("read-only");
    fs::write(&protected, "read-only failure fixture").unwrap();
    let mut permissions = fs::metadata(&protected).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&protected, permissions).unwrap();
    let plan = success(invoke(root.path(), &["clean", "--dry-run", "--json"]));
    let output = invoke(
        root.path(),
        &[
            "clean",
            "--apply",
            "--plan",
            plan["plan_id"].as_str().unwrap(),
            "--json",
        ],
    );
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "partial");
    let mut permissions = fs::metadata(&protected).unwrap().permissions();
    permissions.set_readonly(false);
    fs::set_permissions(&protected, permissions).unwrap();
    let plan = success(invoke(root.path(), &["clean", "--dry-run", "--json"]));
    assert_eq!(
        success(invoke(
            root.path(),
            &[
                "clean",
                "--apply",
                "--plan",
                plan["plan_id"].as_str().unwrap(),
                "--json"
            ]
        ))["outcome"],
        "applied"
    );
    assert!(!cache.exists());
}
