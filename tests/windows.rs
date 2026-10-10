#![cfg(windows)]

#[path = "support/http.rs"]
mod http;

use devd::{
    config::{HealthCheck, ServiceConfig},
    core::{health_check::HealthChecker, process_manager::ManagedProcess},
};
use std::{
    ffi::OsStr,
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT},
    System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
};

fn alive(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_PARAMETER as i32)
        );
        return false;
    }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    result == WAIT_TIMEOUT
}

fn fixture_command() -> String {
    format!(
        "{} --ignored --exact test_windows_fixture --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    )
}

fn config(root: &Path, role: &str) -> ServiceConfig {
    let mut config: ServiceConfig = serde_yaml::from_str("command: placeholder").unwrap();
    config.command = fixture_command();
    config.cwd = Some(root.into());
    config.env.insert("DEVD_TEST_ROLE".into(), role.into());
    config
        .env
        .insert("DEVD_TEST_ROOT".into(), root.to_string_lossy().into_owned());
    config
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Windows fixture timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn descendants(root: &Path) -> Vec<u32> {
    fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            if path.extension() == Some(OsStr::new("pid")) {
                fs::read_to_string(path).ok()?.parse().ok()
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn test_windows_job_cleans_descendants_on_stop_restart_and_drop() {
    let root = tempfile::tempdir().unwrap();
    let mut process = ManagedProcess::spawn("tree", &config(root.path(), "tree"))
        .await
        .unwrap();
    let first = process.pid().unwrap();
    wait_until(|| !descendants(root.path()).is_empty()).await;
    let second = process.restart(Duration::ZERO).await.unwrap();
    assert_ne!(first, second);
    assert!(!alive(first));
    wait_until(|| descendants(root.path()).len() == 2).await;
    process.stop(Duration::ZERO).await.unwrap();
    assert!(!alive(second));
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
    process.restart(Duration::ZERO).await.unwrap();
    wait_until(|| descendants(root.path()).len() == 3).await;
    drop(process);
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
}

#[tokio::test]
async fn test_windows_script_cleanup_on_completion_timeout_and_cancellation() {
    for role in ["probe", "tree"] {
        let root = tempfile::tempdir().unwrap();
        let service = config(root.path(), role);
        let health: HealthCheck = serde_yaml::from_value(
            serde_yaml::to_value(serde_json::json!({
                "type": "script", "command": fixture_command(), "timeout": "3s"
            }))
            .unwrap(),
        )
        .unwrap();
        let checker = HealthChecker::for_service(&health, &service).unwrap();
        let result = checker.probe().await;
        assert_eq!(result.is_healthy(), role == "probe");
        assert!(!descendants(root.path()).is_empty());
        for pid in descendants(root.path()) {
            wait_until(|| !alive(pid)).await;
        }
    }
    let root = tempfile::tempdir().unwrap();
    let service = config(root.path(), "tree");
    let health: HealthCheck = serde_yaml::from_value(
        serde_yaml::to_value(serde_json::json!({
            "type": "script", "command": fixture_command(), "timeout": "30s"
        }))
        .unwrap(),
    )
    .unwrap();
    let checker = HealthChecker::for_service(&health, &service).unwrap();
    let task = tokio::spawn(async move { checker.probe().await });
    wait_until(|| !descendants(root.path()).is_empty()).await;
    task.abort();
    let _ = task.await;
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
}

struct Supervisor(Child);
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    use std::io::{Read, Seek};
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args(args)
            .args(["--color", "never"])
            .current_dir(root)
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI timed out: {args:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    stdout.rewind().unwrap();
    stderr.rewind().unwrap();
    let mut output = std::process::Output {
        status,
        stdout: vec![],
        stderr: vec![],
    };
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    output
}

#[test]
fn test_windows_init_and_stalled_terminal_do_not_block_shutdown() {
    let root = tempfile::tempdir().unwrap();
    assert!(cli(root.path(), &["init"]).status.success());
    assert!(!cli(root.path(), &["init"]).status.success());
    assert!(fs::read_to_string(root.path().join("devd.yml"))
        .unwrap()
        .contains("powershell.exe"));
    assert!(cli(root.path(), &["check"]).status.success());
    let service = config(root.path(), "flood");
    fs::write(root.path().join("devd.yml"), serde_yaml::to_string(&serde_json::json!({
        "version": "1", "services": {"worker": { "command": service.command, "env": service.env }}
    })).unwrap()).unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !root.path().join("flood-ready").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    // Never drain foreground stdout. The service has already produced more
    // than a Windows pipe can hold, while status/stop must remain responsive.
    assert!(cli(root.path(), &["status"]).status.success());
    assert!(cli(root.path(), &["stop"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = supervisor.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shutdown blocked on foreground stdout"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn test_windows_runtime_path_monitor_records_failure_and_recovery_without_restart() {
    use devd::core::{
        events::{query::EventBatch, EventData},
        path_requirements::PathRequirementFailureKind::Missing,
    };
    let root = tempfile::tempdir().unwrap();
    let service = config(root.path(), "tree");
    let input = root.path().join("input");
    fs::write(&input, "private file contents").unwrap();
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&serde_json::json!({
            "version": "1", "services": {"worker": {
                "command": service.command, "cwd": service.cwd, "env": service.env,
                "restart": {"policy": "never"},
                "requires": [{"type": "file", "path": "input"}], "monitor-requires": true
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args(["start", "--persist-events", "--color", "never"])
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    wait_until(|| !descendants(root.path()).is_empty()).await;
    let first: serde_json::Value =
        serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
    fs::remove_file(&input).unwrap();
    for expected in [Some(Missing), None] {
        wait_until(|| {
            let output = cli(
                root.path(),
                &[
                    "events",
                    "worker",
                    "--json",
                    "--type",
                    "path-condition-changed",
                ],
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let batch: EventBatch = serde_json::from_slice(&output.stdout).unwrap();
            batch.entries.iter().any(|event| {
                matches!(&event.data,
                EventData::PathConditionChanged { evidence } if evidence.failure == expected)
            })
        })
        .await;
        if expected.is_some() {
            fs::write(&input, "recovered").unwrap();
        }
    }
    let current: serde_json::Value =
        serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
    assert!(first["services"]["worker"]["pid"].is_number(), "{first}");
    assert_eq!(
        current["services"]["worker"]["pid"],
        first["services"]["worker"]["pid"]
    );
    assert_eq!(current["services"]["worker"]["restart_count"], 0);
    assert!(cli(root.path(), &["restart", "worker"]).status.success());
    let replacement: serde_json::Value =
        serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
    let generation = replacement["services"]["worker"]["event_generation"]
        .as_u64()
        .unwrap();
    assert_ne!(
        replacement["services"]["worker"]["event_generation"],
        first["services"]["worker"]["event_generation"]
    );
    fs::remove_file(&input).unwrap();
    wait_until(|| {
        let output = cli(
            root.path(),
            &[
                "events",
                "worker",
                "--json",
                "--type",
                "path-condition-changed",
            ],
        );
        let batch: EventBatch = serde_json::from_slice(&output.stdout).unwrap();
        batch
            .entries
            .iter()
            .any(|event| event.generation == Some(generation))
    })
    .await;
    assert!(cli(root.path(), &["stop"]).status.success());
    wait_until(|| supervisor.0.try_wait().unwrap().is_some()).await;
    fs::write(&input, "restore after stopping").unwrap();
    let output = cli(
        root.path(),
        &[
            "events",
            "--stored",
            "--json",
            "--type",
            "path-condition-changed",
        ],
    );
    assert!(output.status.success());
    let batch: EventBatch = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(batch.entries.len(), 3);
    let complete: EventBatch =
        serde_json::from_slice(&cli(root.path(), &["events", "--stored", "--json"]).stdout)
            .unwrap();
    for stopped in complete
        .entries
        .iter()
        .filter(|event| matches!(event.data, EventData::ServiceStopRequested { .. }))
    {
        assert!(!complete
            .entries
            .iter()
            .any(|event| event.generation == stopped.generation
                && event.sequence > stopped.sequence
                && matches!(event.data, EventData::PathConditionChanged { .. })));
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private file contents"));
    assert!(cli(root.path(), &["explain", "worker", "--stored"])
        .status
        .success());
}

#[tokio::test]
async fn test_windows_reload_preview_and_apply_preserve_unrelated_job() {
    use devd::core::{
        reload::{ChangeKind, ReloadPlan},
        service_manager::RuntimeSnapshot,
    };
    let root = tempfile::tempdir().unwrap();
    let service = config(root.path(), "tree");
    let mut document = serde_json::json!({
        "version": "1", "services": {"worker": {
            "command": service.command, "cwd": service.cwd, "env": service.env,
            "restart": {"policy": "never"}
        }}
    });
    document["services"]["isolated"] = serde_json::json!({
        "command": service.command, "cwd": service.cwd,
        "env": {"DEVD_TEST_ROLE": "leaf"}, "restart": {"policy": "never"}
    });
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&document).unwrap(),
    )
    .unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args(["start", "--color", "never"])
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    wait_until(|| !descendants(root.path()).is_empty()).await;
    wait_until(|| {
        let output = cli(root.path(), &["status", "--json"]);
        serde_json::from_slice::<RuntimeSnapshot>(&output.stdout)
            .is_ok_and(|snapshot| snapshot.services.values().all(|state| state.pid.is_some()))
    })
    .await;
    let before: RuntimeSnapshot =
        serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
    let preview = |args: &[&str]| -> ReloadPlan {
        let output = cli(root.path(), args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let baseline = preview(&["reload", "--dry-run", "--json"]);
    assert_eq!(baseline.base_config_id, baseline.candidate_config_id);
    document["services"]["worker"]["env"]["TOKEN"] = "candidate-secret".into();
    fs::write(
        root.path().join("candidate.yml"),
        serde_yaml::to_string(&document).unwrap(),
    )
    .unwrap();
    let changed = preview(&[
        "reload",
        "--dry-run",
        "--candidate",
        "candidate.yml",
        "--json",
    ]);
    assert_eq!(changed.services["worker"].change, ChangeKind::Modified);
    assert_eq!(changed.services["worker"].changed_fields, ["env"]);
    assert_eq!(changed.base_config_id, baseline.base_config_id);
    assert!(changed.apply_available);
    assert!(!serde_json::to_string(&changed)
        .unwrap()
        .contains("candidate-secret"));
    document["services"]["worker"]["healthcheck"] =
        serde_json::json!({"type": "socket", "path": "unsupported.sock"});
    fs::write(
        root.path().join("candidate.yml"),
        serde_yaml::to_string(&document).unwrap(),
    )
    .unwrap();
    let invalid = cli(
        root.path(),
        &["reload", "--dry-run", "--candidate", "candidate.yml"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("not supported on Windows"));
    let after: RuntimeSnapshot =
        serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
    assert!(before.services["worker"].pid.is_some());
    assert_eq!(before.services["worker"].pid, after.services["worker"].pid);
    assert_eq!(
        before.services["worker"].event_generation,
        after.services["worker"].event_generation
    );
    assert!(descendants(root.path()).into_iter().all(alive));
    // Apply must dispose of the old Windows Job before starting its replacement.
    let old_descendants = descendants(root.path());
    document["services"]["worker"]
        .as_object_mut()
        .unwrap()
        .remove("healthcheck");
    fs::write(
        root.path().join("candidate.yml"),
        serde_yaml::to_string(&document).unwrap(),
    )
    .unwrap();
    let current = preview(&[
        "reload",
        "--dry-run",
        "--candidate",
        "candidate.yml",
        "--json",
    ]);
    let output = cli(
        root.path(),
        &[
            "reload",
            "--apply",
            "--plan",
            &current.plan_id,
            "--candidate",
            "candidate.yml",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: devd::core::reload::ReloadReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.outcome, devd::core::reload::ReloadOutcome::Applied);
    assert_eq!(report.stopped, ["worker"]);
    assert_eq!(report.ready, ["worker"]);
    for pid in old_descendants {
        wait_until(|| !alive(pid)).await;
    }
    let applied: RuntimeSnapshot =
        serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
    assert_eq!(
        applied.services["isolated"].pid,
        before.services["isolated"].pid
    );
    assert_eq!(
        applied.services["isolated"].event_generation,
        before.services["isolated"].event_generation
    );
    assert!(alive(applied.services["isolated"].pid.unwrap()));
    assert_ne!(
        applied.services["worker"].event_generation,
        before.services["worker"].event_generation
    );
    assert_eq!(
        preview(&[
            "reload",
            "--dry-run",
            "--candidate",
            "candidate.yml",
            "--json"
        ])
        .base_config_id,
        current.candidate_config_id
    );
    assert!(cli(root.path(), &["stop"]).status.success());
    wait_until(|| supervisor.0.try_wait().unwrap().is_some()).await;
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
}

#[tokio::test]
async fn test_windows_reload_rejects_stale_input_and_cleans_jobs_on_failure_or_stop() {
    use devd::core::{
        reload::{ReloadOutcome, ReloadPlan, ReloadReport},
        service_manager::RuntimeSnapshot,
    };

    for interrupt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let service = config(root.path(), "tree");
        let mut document = serde_json::json!({
            "version": "1", "services": {"worker": {
                "command": service.command, "cwd": service.cwd, "env": service.env,
                "restart": {"policy": "never"}
            }}
        });
        let write = |name: &str, value: &serde_json::Value| {
            fs::write(
                root.path().join(name),
                serde_yaml::to_string(value).unwrap(),
            )
            .unwrap();
        };
        write("devd.yml", &document);
        let mut supervisor = Supervisor(
            Command::new(env!("CARGO_BIN_EXE_devd"))
                .args(["start", "--persist-events", "--color", "never"])
                .current_dir(root.path())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        wait_until(|| !descendants(root.path()).is_empty()).await;
        let before: RuntimeSnapshot =
            serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
        let old_pid = before.services["worker"].pid.unwrap();
        document["services"]["worker"]["env"]["REVISION"] = "next".into();
        write("candidate.yml", &document);
        let preview = || -> ReloadPlan {
            let output = cli(
                root.path(),
                &[
                    "reload",
                    "--dry-run",
                    "--candidate",
                    "candidate.yml",
                    "--json",
                ],
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        };
        let apply = |id: &str| {
            cli(
                root.path(),
                &[
                    "reload",
                    "--apply",
                    "--plan",
                    id,
                    "--candidate",
                    "candidate.yml",
                    "--json",
                ],
            )
        };
        let stale = preview();
        document["services"]["worker"]["env"]["REVISION"] = "changed-again".into();
        write("candidate.yml", &document);
        let output = apply(&stale.plan_id);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("stale"));
        document["services"]["worker"]["healthcheck"] =
            serde_json::json!({"type": "socket", "path": "invalid.sock"});
        write("candidate.yml", &document);
        assert!(!apply(&stale.plan_id).status.success());
        let unchanged: RuntimeSnapshot =
            serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
        assert_eq!(unchanged.services["worker"].pid, Some(old_pid));
        assert_eq!(
            unchanged.services["worker"].event_generation,
            before.services["worker"].event_generation
        );

        let mock = http::HttpMock::start();
        mock.api.set(200);
        document["services"]["worker"]
            .as_object_mut()
            .unwrap()
            .remove("healthcheck");
        if interrupt {
            // A test-owned listener holds the port while the HTTP gate stays closed.
            document["services"]["worker"]["healthcheck"] = serde_json::json!({
                "type": "http", "url": format!("{}/database", mock.url),
                "interval": "20ms", "timeout": "1s", "retries": 10000
            });
            document["services"]["child"] = serde_json::json!({
                "command": service.command, "cwd": service.cwd,
                "env": {"DEVD_TEST_ROLE": "leaf"},
                "depends-on": [{"service": "worker", "condition": "http-ready"}],
                "restart": {"policy": "never"}
            });
        } else {
            document["services"]["broken"] = serde_json::json!({
                "command": "missing-devd-test-program.exe",
                "depends-on": [{"service": "worker", "condition": "started"}],
                "restart": {"policy": "always", "max-attempts": 10000}
            });
        }
        write("candidate.yml", &document);
        let plan = preview();
        let request_root = root.path().to_owned();
        let id = plan.plan_id.clone();
        let request = std::thread::spawn(move || {
            cli(
                &request_root,
                &[
                    "reload",
                    "--apply",
                    "--plan",
                    &id,
                    "--candidate",
                    "candidate.yml",
                    "--json",
                ],
            )
        });
        if interrupt {
            wait_until(|| mock.database.requests() > 0).await;
            assert!(!cli(root.path(), &["restart", "worker"]).status.success());
            let competing = apply(&plan.plan_id);
            assert!(!competing.status.success());
            assert!(String::from_utf8_lossy(&competing.stderr).contains("already in progress"));
            let snapshot: RuntimeSnapshot =
                serde_json::from_slice(&cli(root.path(), &["status", "--json"]).stdout).unwrap();
            assert!(snapshot.services["child"].pid.is_none());
            assert!(cli(root.path(), &["stop"]).status.success());
        }
        let output = request.join().unwrap();
        assert!(!output.status.success(), "reload must not succeed");
        let report: ReloadReport = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report.outcome,
            if interrupt {
                ReloadOutcome::Interrupted
            } else {
                ReloadOutcome::Failed
            }
        );
        assert!(report.config_committed);
        assert_eq!(report.stopped, ["worker"]);
        assert!(report.started.contains(&"worker".to_string()));
        if interrupt {
            assert!(!report.started.contains(&"child".to_string()));
        } else {
            assert!(report.ready.contains(&"worker".to_string()));
            assert!(report.failure.is_some());
        }
        wait_until(|| supervisor.0.try_wait().unwrap().is_some()).await;
        assert!(!alive(old_pid));
        for pid in descendants(root.path()) {
            wait_until(|| !alive(pid)).await;
        }
        let output = cli(root.path(), &["events", "--stored", "--json"]);
        assert!(output.status.success());
        let batch: devd::core::events::query::EventBatch =
            serde_json::from_slice(&output.stdout).unwrap();
        assert!(batch.entries.iter().any(|event| matches!(&event.data,
            devd::core::events::EventData::ReloadFinished { outcome, .. } if *outcome == report.outcome)));
    }
}

#[test]
fn test_windows_cli_control_persistence_and_supervisor_death_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let service = config(root.path(), "tree");
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&serde_json::json!({
            "version": "1", "services": {"worker": {
                "command": service.command, "cwd": service.cwd, "env": service.env
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let output = cli(root.path(), &["check"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args([
                "start",
                "--persist-logs",
                "--persist-events",
                "--color",
                "never",
            ])
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = cli(root.path(), &["status", "--json"]);
        if output.status.success() && !descendants(root.path()).is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!cli(root.path(), &["start"]).status.success());
    let output = cli(root.path(), &["events", "worker", "--json"]);
    assert!(output.status.success());
    let events: devd::core::events::query::EventBatch =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        events.persistence,
        Some(devd::core::events::query::PersistenceState::Recording)
    );
    assert!(!events.entries.is_empty());
    assert!(cli(root.path(), &["restart", "worker"]).status.success());
    assert!(cli(root.path(), &["snapshot", "save", "before-stop"])
        .status
        .success());
    fs::remove_file(root.path().join("devd.yml")).unwrap();
    assert!(cli(root.path(), &["stop"]).status.success());
    let stop_deadline = Instant::now() + Duration::from_secs(15);
    while supervisor.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < stop_deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cli(root.path(), &["logs", "--stored"]).status.success());
    let output = cli(root.path(), &["events", "--stored", "--json"]);
    assert!(output.status.success());
    let events: devd::core::events::query::EventBatch =
        serde_json::from_slice(&output.stdout).unwrap();
    assert!(events.gaps.is_empty());
    assert!(events.entries.iter().any(|event| matches!(
        event.data,
        devd::core::events::EventData::SupervisorStopped { .. }
    )));
    assert!(cli(
        root.path(),
        &[
            "snapshot",
            "restore",
            "before-stop",
            "--output",
            "restored.yml"
        ]
    )
    .status
    .success());
    assert!(!cli(
        root.path(),
        &[
            "snapshot",
            "restore",
            "before-stop",
            "--output",
            "restored.yml"
        ]
    )
    .status
    .success());
    for pid in descendants(root.path()) {
        assert!(!alive(pid));
    }
    fs::rename(
        root.path().join("restored.yml"),
        root.path().join("devd.yml"),
    )
    .unwrap();
    let count = descendants(root.path()).len();
    supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .current_dir(root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while descendants(root.path()).len() == count {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    supervisor.0.kill().unwrap();
    supervisor.0.wait().unwrap();
    let cleanup_deadline = Instant::now() + Duration::from_secs(15);
    for pid in descendants(root.path()) {
        while alive(pid) {
            assert!(Instant::now() < cleanup_deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

// Invoked as a real child process by the tests above. The Windows CI runs the
// parent tests; this ignored entry is only the deterministic workload helper.
#[test]
#[ignore]
#[allow(clippy::zombie_processes)] // Deliberately orphan a descendant to test Job cleanup.
fn test_windows_fixture() {
    let role = std::env::var("DEVD_TEST_ROLE").unwrap();
    if role == "bindings" {
        let root = std::env::var("DEVD_TEST_ROOT").unwrap();
        let output = std::env::var("DEVD_TEST_OUTPUT").unwrap();
        let values = serde_json::json!({
            "address": std::env::var("API_ADDR").unwrap(),
            "data": std::env::var("API_DATA").unwrap(),
        });
        fs::write(
            Path::new(&root).join(output),
            serde_json::to_vec(&values).unwrap(),
        )
        .unwrap();
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if role == "flood" {
        // The collector uses a broadcast channel, independent of terminal
        // writes. Reaching this marker proves it drained beyond pipe capacity.
        // If it blocks, fail the test instead of issuing stop before the flood.
        for _ in 0..4096 {
            println!("{}", "x".repeat(1024));
        }
        fs::write(
            Path::new(&std::env::var("DEVD_TEST_ROOT").unwrap()).join("flood-ready"),
            "ready",
        )
        .unwrap();
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if role != "leaf" {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "test_windows_fixture",
                "--nocapture",
            ])
            .env("DEVD_TEST_ROLE", "leaf")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let root = std::env::var("DEVD_TEST_ROOT").unwrap();
        fs::write(
            Path::new(&root).join(format!("{}.pid", std::process::id())),
            child.id().to_string(),
        )
        .unwrap();
        println!("fixture spawned descendant {}", child.id());
        if role == "probe" {
            return;
        }
    }
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn test_windows_bindings_reach_owner_and_direct_dependent() {
    let root = tempfile::tempdir().unwrap();
    let mut worker = config(root.path(), "bindings");
    worker
        .env
        .insert("DEVD_TEST_OUTPUT".into(), "worker-bindings.json".into());
    let mut child = config(root.path(), "bindings");
    child
        .env
        .insert("DEVD_TEST_OUTPUT".into(), "child-bindings.json".into());
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&serde_json::json!({
            "version": "1",
            "services": {
                "worker": {"command": worker.command, "env": worker.env,
                    "ports": {"API_ADDR": "127.0.0.1:31001"},
                    "paths": {"API_DATA": {"scope": "instance", "path": "worker"}},
                    "restart": {"policy": "never"}},
                "child": {"command": child.command, "env": child.env,
                    "depends-on": ["worker"], "restart": {"policy": "never"}}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    for file in ["worker-bindings.json", "child-bindings.json"] {
        wait_until(|| {
            fs::read(root.path().join(file))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some()
        })
        .await;
    }
    let worker: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("worker-bindings.json")).unwrap())
            .unwrap();
    let child: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("child-bindings.json")).unwrap())
            .unwrap();
    assert_eq!(worker, child);
    assert_eq!(worker["address"], "127.0.0.1:31001");
    assert_eq!(
        Path::new(worker["data"].as_str().unwrap()),
        fs::canonicalize(root.path())
            .unwrap()
            .join(".devd/devd.yml/runtime/worker")
    );
    assert!(cli(root.path(), &["stop"]).status.success());
    wait_until(|| supervisor.0.try_wait().unwrap().is_some()).await;
}
