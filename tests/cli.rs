#![cfg(unix)]

mod support;

use std::{fs, process::Stdio, time::Duration};
use support::{failure, success, wait, Project, Supervisor};
const RUNNING: &str = "services:\n  worker:\n    command: sh -c 'echo hello; echo problem >&2; exec sleep 60'\n    restart:\n      policy: never\n";

#[test]
fn test_cli_profiles_isolate_instances_and_keep_controls_after_config_removal() {
    use devd::core::service_manager::RuntimeSnapshot;
    for explicit_state_dir in [false, true] {
        let project = Project::new(
            r#"services:
  worker:
    command: sh -c 'echo "$MODE:$KEEP:$FILE:$PWD"; exec sleep 60'
    env: {MODE: base, KEEP: inherited}
    env-file: base.env
    restart: {policy: never}
profiles:
  dev:
    services:
      worker:
        cwd: development
        env-file: dev.env
        env: {MODE: dev}
  staging:
    services:
      worker:
        cwd: stage
        env-file: null
        env: {MODE: staging, FILE: cleared}
"#,
        );
        fs::write(project.path().join("base.env"), "FILE=base-file\n").unwrap();
        fs::create_dir(project.path().join("development")).unwrap();
        fs::create_dir(project.path().join("stage")).unwrap();
        fs::write(
            project.path().join("development/dev.env"),
            "FILE=dev-file\nMODE=file-value\n",
        )
        .unwrap();
        let invoke = |args: &[&str], profile: Option<&str>| {
            let mut args = args.to_vec();
            if let Some(name) = profile {
                args.extend(["--profile", name]);
            }
            if explicit_state_dir {
                args.extend(["--state-dir", "runtime"]);
            }
            project.invoke(&args)
        };
        let snapshot = |profile| -> RuntimeSnapshot {
            wait(|| {
                let output = invoke(&["status", "--json"], profile);
                if !output.status.success() {
                    return None;
                }
                let snapshot: RuntimeSnapshot = serde_json::from_slice(&output.stdout).unwrap();
                snapshot.services["worker"]
                    .pid
                    .is_some()
                    .then_some(snapshot)
            })
        };
        let mut supervisors = Vec::new();
        for name in [None, Some("dev"), Some("staging")] {
            let mut args = vec!["start"];
            if let Some(name) = name {
                args.extend(["--profile", name]);
            }
            if explicit_state_dir {
                args.extend(["--state-dir", "runtime"]);
            }
            let suffix = name.unwrap_or("base");
            supervisors.push(Supervisor(
                project
                    .command(&args)
                    .stdout(
                        fs::File::create(project.path().join(format!("stdout-{suffix}"))).unwrap(),
                    )
                    .stderr(
                        fs::File::create(project.path().join(format!("stderr-{suffix}"))).unwrap(),
                    )
                    .spawn()
                    .unwrap(),
            ));
        }
        let base = snapshot(None);
        let dev = snapshot(Some("dev"));
        let staging = snapshot(Some("staging"));
        assert_ne!(base.supervisor_pid, dev.supervisor_pid);
        assert_ne!(dev.supervisor_pid, staging.supervisor_pid);
        assert_ne!(base.services["worker"].pid, dev.services["worker"].pid);
        for (profile, prefix, cwd) in [
            (None, "base:inherited:base-file:", ""),
            (Some("dev"), "dev:inherited:dev-file:", "/development"),
            (Some("staging"), "staging:inherited:cleared:", "/stage"),
        ] {
            let expected = format!(
                "{prefix}{}{cwd}",
                fs::canonicalize(project.path()).unwrap().display()
            );
            wait(|| {
                success(invoke(&["logs", "worker"], profile))
                    .contains(&expected)
                    .then_some(())
            });
        }
        failure(invoke(&["start"], Some("dev")), "another supervisor");
        // Controls use startup configuration and profile routing, even if the
        // on-disk document is unreadable or has gone away altogether.
        fs::write(project.path().join("devd.yml"), "version: [").unwrap();
        success(invoke(&["restart", "worker"], Some("dev")));
        let restarted = snapshot(Some("dev"));
        assert_eq!(restarted.services["worker"].restart_count, 1);
        assert_ne!(restarted.services["worker"].pid, dev.services["worker"].pid);
        wait(|| {
            (success(invoke(&["logs", "worker"], Some("dev")))
                .matches("dev:inherited:dev-file:")
                .count()
                == 2)
                .then_some(())
        });
        fs::remove_file(project.path().join("devd.yml")).unwrap();
        success(invoke(&["stop"], Some("dev")));
        supervisors[1].finish(true);
        failure(
            invoke(&["status"], Some("dev")),
            "no reachable devd supervisor",
        );
        assert_eq!(
            snapshot(None).services["worker"].pid,
            base.services["worker"].pid
        );
        assert_eq!(
            snapshot(Some("staging")).services["worker"].pid,
            staging.services["worker"].pid
        );
        success(invoke(&["stop"], None));
        success(invoke(&["stop"], Some("staging")));
        supervisors[0].finish(true);
        supervisors[2].finish(true);
        let root = project.path().join(if explicit_state_dir {
            "runtime"
        } else {
            ".devd/devd.yml"
        });
        for subdir in ["", "profiles/dev", "profiles/staging"] {
            let saved: RuntimeSnapshot =
                serde_json::from_slice(&fs::read(root.join(subdir).join("services.json")).unwrap())
                    .unwrap();
            assert!(saved.services["worker"].pid.is_none());
        }
    }
}

#[test]
fn test_cli_profile_validation_graph_and_init_boundaries() {
    let project = Project::new("services:\n  worker: {command: sleep 60}\nprofiles:\n  dev:\n    services:\n      db: {command: sleep 60}\n      worker: {depends-on: [db]}\n");
    let text = success(project.invoke(&["--profile", "dev", "check"]));
    assert!(text.contains("(profile: dev)"));
    assert!(success(project.invoke(&["graph", "--profile", "dev"])).contains("worker -> db"));
    assert!(!success(project.invoke(&["graph"])).contains("worker -> db"));
    for command in ["start", "check", "graph"] {
        failure(
            project.invoke(&[command, "--profile", "missing"]),
            "unknown profile 'missing'",
        );
    }
    for command in ["start", "check", "graph", "stop", "status", "logs"] {
        failure(
            project.invoke(&[command, "--profile", "../dev"]),
            "invalid profile name",
        );
    }
    failure(
        project.invoke(&["init", "--profile", "dev", "--config", "new.yml"]),
        "--profile is not supported by init",
    );
    assert!(!project.path().join("new.yml").exists());
    assert!(!project.path().join(".devd").exists());
}

#[test]
fn test_cli_profile_names_remain_case_sensitive_on_all_filesystems() {
    let project = Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\nprofiles: {dev: {}, Dev: {}}\n");
    let mut supervisors = Vec::new();
    let mut pids = Vec::new();
    for name in ["dev", "Dev"] {
        supervisors.push(Supervisor(
            project
                .command(&["start", "--profile", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        ));
        pids.push(wait(|| {
            let output = project.invoke(&["status", "--json", "--profile", name]);
            if !output.status.success() {
                return None;
            }
            let state: devd::core::service_manager::RuntimeSnapshot =
                serde_json::from_slice(&output.stdout).unwrap();
            state.services["worker"].pid
        }));
    }
    assert_ne!(pids[0], pids[1]);
    success(project.invoke(&["stop", "--profile", "dev"]));
    supervisors[0].finish(true);
    success(project.invoke(&["status", "--profile", "Dev"]));
    success(project.invoke(&["stop", "--profile", "Dev"]));
    supervisors[1].finish(true);
    assert!(project
        .path()
        .join(".devd/devd.yml/profiles/dev/services.json")
        .is_file());
    assert!(project
        .path()
        .join(".devd/devd.yml/profiles/~44ev/services.json")
        .is_file());
}

#[test]
fn test_cli_dependency_recovery_restarts_opted_in_service_and_retains_logs() {
    let project = Project::new("services:\n  db:\n    command: sleep 60\n    restart: {policy: never}\n  worker:\n    command: sh -c 'echo worker-started; exec sleep 60'\n    depends-on: [db]\n    restart-on-dep-recovery: true\n    restart: {policy: on-failure, initial-delay: 20ms, max-attempts: 3}\n  unchanged:\n    command: sleep 60\n    depends-on: [db]\n    restart: {policy: never}\n");
    success(project.invoke(&["check"]));
    let mut supervisor = project.start();
    let initial = wait(|| {
        project
            .snapshot()
            .filter(|s| s.services.values().all(|s| s.pid.is_some()))
    });
    wait(|| {
        success(project.invoke(&["logs", "worker"]))
            .contains("worker-started")
            .then_some(())
    });
    success(project.invoke(&["restart", "db"]));
    let recovered = wait(|| {
        project.snapshot().filter(|s| {
            s.services["worker"].restart_count == 1 && s.services["worker"].pid.is_some()
        })
    });
    assert_ne!(
        initial.services["worker"].pid,
        recovered.services["worker"].pid
    );
    assert_eq!(
        initial.services["unchanged"].pid,
        recovered.services["unchanged"].pid
    );
    wait(|| {
        (success(project.invoke(&["logs", "worker"]))
            .matches("worker-started")
            .count()
            == 2)
            .then_some(())
    });
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    let saved: devd::core::service_manager::RuntimeSnapshot = serde_json::from_slice(
        &fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap(),
    )
    .unwrap();
    assert!(saved
        .services
        .values()
        .all(|s| s.pid.is_none() && s.resources.is_none()));
    assert_eq!(saved.services["worker"].restart_count, 1);
}

#[test]
fn test_cli_dependency_recovery_rejects_conflicting_policy_before_start() {
    let project = Project::new("services:\n  db: {command: sleep 60}\n  worker:\n    command: sleep 60\n    depends-on: [db]\n    restart-on-dep-recovery: true\n    restart: {policy: never}\n");
    for command in ["check", "start"] {
        failure(
            project.invoke(&[command]),
            "requires an automatic restart policy",
        );
    }
    assert!(!project.path().join(".devd").exists());
}

#[test]
fn test_cli_resource_samples_follow_busy_process_restart_and_shutdown() {
    let project = Project::new("services:\n  worker:\n    command: sh -c 'while :; do :; done'\n    restart: {policy: never}\n");
    let mut supervisor = project.start();
    let first = project.running();
    let sampled = wait(|| {
        project.snapshot().filter(|s| {
            s.services["worker"]
                .resources
                .as_ref()
                .is_some_and(|r| r.cpu_percent.is_some_and(|cpu| cpu > 0.0))
        })
    });
    let state = &sampled.services["worker"];
    let resources = state.resources.as_ref().unwrap();
    assert!(resources.memory_bytes > 0);
    assert!(resources.sampled_at >= state.started_at.unwrap());
    let text = success(project.invoke(&["status"]));
    assert!(text.contains("CPU %\tRSS MiB"), "{text}");
    let row: Vec<_> = text
        .lines()
        .find(|line| line.starts_with("worker\t"))
        .unwrap()
        .split('\t')
        .collect();
    assert!(row[4].parse::<f32>().unwrap() > 0.0);
    assert!(row[5].parse::<f64>().unwrap() >= 0.0);

    success(project.invoke(&["restart", "worker"]));
    let restarted = project.running();
    assert_ne!(
        first.services["worker"].pid,
        restarted.services["worker"].pid
    );
    assert_eq!(restarted.services["worker"].restart_count, 1);
    if let Some(resources) = &restarted.services["worker"].resources {
        assert!(resources.sampled_at >= restarted.services["worker"].started_at.unwrap());
    }
    wait(|| {
        project.snapshot().filter(|s| {
            s.services["worker"]
                .resources
                .as_ref()
                .is_some_and(|r| r.cpu_percent.is_some_and(|cpu| cpu > 0.0))
        })
    });
    // The persisted snapshot receives samples through the same state store.
    wait(|| {
        let saved: devd::core::service_manager::RuntimeSnapshot = serde_json::from_slice(
            &fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap(),
        )
        .unwrap();
        (saved.services["worker"].restart_count == 1
            && saved.services["worker"].resources.is_some())
        .then_some(())
    });
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    let saved: devd::core::service_manager::RuntimeSnapshot = serde_json::from_slice(
        &fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap(),
    )
    .unwrap();
    assert!(saved.services["worker"].pid.is_none());
    assert!(saved.services["worker"].resources.is_none());
}

#[test]
fn test_cli_socket_readiness_resolves_against_service_directory() {
    use devd::core::service_manager::ServiceState;
    let project = Project::new("services:\n  provider:\n    command: sleep 60\n    cwd: service\n    restart:\n      policy: never\n    healthcheck:\n      type: socket\n      path: health.sock\n      interval: 50ms\n      retries: 1000\n  worker:\n    command: sleep 60\n    depends-on:\n      - service: provider\n        condition: socket-ready\n");
    fs::create_dir(project.path().join("service")).unwrap();
    success(project.invoke(&["check"]));
    let mut supervisor = project.start();
    let before = wait(|| {
        project
            .snapshot()
            .filter(|s| s.services["provider"].consecutive_failures > 0)
    });
    assert_eq!(before.services["worker"].status, ServiceState::Pending);
    assert!(before.services["worker"].pid.is_none());
    let _listener =
        std::os::unix::net::UnixListener::bind(project.path().join("service/health.sock")).unwrap();
    let after = project.running();
    assert_eq!(after.services["provider"].status, ServiceState::Healthy);
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_help_validation_graph_and_failures() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\n  api:\n    command: sleep 60\n    depends-on: [worker]\n");
    assert!(success(project.invoke(&["--help"])).contains("restart"));
    assert!(success(project.invoke(&["check"])).contains("Configuration valid"));
    let graph = success(project.invoke(&["graph"]));
    assert!(graph.contains("api -> worker (Started)"), "{graph}");
    assert!(graph.contains("1: worker\n  2: api"));
    assert!(!project.path().join(".devd").exists());
    failure(
        project.invoke(&["init"]),
        "existing configuration will not be replaced",
    );
    for command in ["top", "wat"] {
        failure(project.invoke(&[command]), "unrecognized subcommand");
    }
    for arguments in [
        &["status"][..],
        &["stop"],
        &["logs"],
        &["restart", "worker"],
    ] {
        failure(project.invoke(arguments), "no reachable devd supervisor");
    }
    fs::write(project.path().join("devd.yml"), "services: [").unwrap();
    for command in ["check", "graph", "start"] {
        failure(project.invoke(&[command]), "error:");
    }
}

#[test]
fn test_cli_init_creates_valid_config_and_preserves_existing_file() {
    let project = Project::new("services:\n  old:\n    command: sleep 60\n");
    let path = project.path().join("starter.yml");
    let filename = path.to_str().unwrap();
    let output = success(project.invoke(&[
        "init",
        "--config",
        filename,
        "--service",
        "api",
        "--command",
        "sleep 60",
    ]));
    assert!(output.contains("Created"));
    let generated = fs::read_to_string(&path).unwrap();
    assert!(generated.contains("api:"));
    assert!(
        success(project.invoke(&["check", "--config", filename])).contains("Configuration valid")
    );
    failure(
        project.invoke(&["init", "--config", filename]),
        "existing configuration will not be replaced",
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), generated);
    failure(
        project.invoke(&["init", "--config", filename, "--service", "bad name"]),
        "service names",
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), generated);
    assert!(!project.path().join(".devd").exists());
}

#[test]
fn test_cli_lifecycle_logs_restart_duplicate_and_config_changes() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    let first = project.running().services["worker"].pid.unwrap();
    assert!(success(project.invoke(&["status"])).contains(&first.to_string()));
    wait(|| {
        success(project.invoke(&["logs", "worker"]))
            .contains("problem")
            .then_some(())
    });
    assert!(success(project.invoke(&["logs"])).contains("hello"));
    assert_eq!(
        success(project.invoke(&["logs", "--tail", "1"]))
            .lines()
            .count(),
        1
    );
    failure(project.invoke(&["logs", "absent"]), "unknown service");
    failure(project.invoke(&["logs", "--tail", "0"]), "invalid value");
    let colored = project.invoke(&["logs", "--color", "always"]);
    assert!(success(colored).contains('\u{1b}'));
    failure(project.invoke(&["restart", "absent"]), "unknown service");
    failure(project.invoke(&["start"]), "another supervisor");
    assert!(success(project.invoke(&["restart", "worker"])).contains("Restarted worker"));
    let snapshot = project.running();
    assert_ne!(snapshot.services["worker"].pid.unwrap(), first);
    assert_eq!(snapshot.services["worker"].restart_count, 1);
    fs::write(project.path().join("devd.yml"), "invalid: [").unwrap();
    success(project.invoke(&["status"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    assert!(!project.path().join(".devd/devd.yml/control.sock").exists());
    failure(project.invoke(&["status"]), "no reachable devd supervisor");
    failure(project.invoke(&["stop"]), "no reachable devd supervisor");
    failure(project.invoke(&["logs"]), "no reachable devd supervisor");
}

#[test]
fn test_cli_init_preserves_multiline_commands_and_yaml_scalar_names() {
    let project = Project::new(RUNNING);
    let path = project.path().join("starter.yml");
    let filename = path.to_str().unwrap();
    let command = "sh -c 'printf \"key: value\\n\";\necho \"quoted # text\"'";
    success(project.invoke(&[
        "init",
        "--config",
        filename,
        "--service",
        "true",
        "--command",
        command,
    ]));
    let generated: devd::config::DevdConfig =
        serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(generated.services["true"].command, command);
    success(project.invoke(&["check", "--config", filename]));
    let output = success(project.invoke(&["start", "--config", filename]));
    assert!(output.contains("key: value"), "{output}");
    assert!(output.contains("quoted # text"), "{output}");
}

#[test]
fn test_cli_follow_logs_replays_tail_then_streams_until_shutdown() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    project.running();
    wait(|| {
        success(project.invoke(&["logs", "--tail", "2"]))
            .contains("problem")
            .then_some(())
    });
    let output = project.path().join("follow.log");
    let mut follower = Supervisor(
        project
            .command(&["logs", "worker", "--follow", "--tail", "2"])
            .stdout(fs::File::create(&output).unwrap())
            .stderr(fs::File::create(project.path().join("follow.err")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait(|| {
        fs::read_to_string(&output)
            .unwrap()
            .contains("problem")
            .then_some(())
    });
    success(project.invoke(&["restart", "worker"]));
    wait(|| {
        let text = fs::read_to_string(&output).unwrap();
        (text.matches("hello").count() == 2 && text.matches("problem").count() == 2).then_some(())
    });
    failure(
        project.invoke(&["logs", "absent", "--follow"]),
        "unknown service",
    );
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    follower.finish(true);
}

#[test]
fn test_cli_relative_cwd_env_file_and_deleted_config() {
    let project = Project::new("services:\n  worker:\n    command: sh -c 'echo $MARKER; pwd; exec sleep 60'\n    cwd: service\n    env-file: local.env\n    restart: {policy: never}\n");
    fs::create_dir(project.path().join("service")).unwrap();
    fs::write(
        project.path().join("service/local.env"),
        "MARKER=relative-path-ok\n",
    )
    .unwrap();
    let mut command = project.command(&["start"]);
    command
        .current_dir("/tmp")
        .arg("--config")
        .arg(project.path().join("devd.yml"))
        .stdout(fs::File::create(project.path().join("stdout")).unwrap())
        .stderr(Stdio::piped());
    let mut supervisor = Supervisor(command.spawn().unwrap());
    project.running();
    wait(|| {
        success(project.invoke(&["logs"]))
            .contains("relative-path-ok")
            .then_some(())
    });
    assert!(success(project.invoke(&["logs"])).contains("/service"));
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_spawn_failure_and_unsupported_config() {
    let project = Project::new("services:\n  worker:\n    command: /no/such/devd-test-program\n    restart: {policy: never}\n");
    failure(project.invoke(&["start"]), "services failed");
    fs::write(
        project.path().join("devd.yml"),
        "version: '1'\nservices:\n  worker:\n    command: sleep 60\n    restart: {backoff: exponential}\n",
    )
    .unwrap();
    success(project.invoke(&["check"]));
    fs::write(
        project.path().join("devd.yml"),
        "version: '1'\nservices:\n  worker:\n    command: \"sh '\"\n",
    )
    .unwrap();
    failure(project.invoke(&["check"]), "failed to parse command");
}

#[test]
fn test_cli_rejects_ignored_settings_before_creating_runtime_state() {
    for (service, message) in [
        (
            "command: touch should-not-exist\n    limits: {memory: 1GB}",
            "resource limits are not implemented",
        ),
        (
            "command: touch should-not-exist\n    limits: {}",
            "resource limits are not implemented",
        ),
        (
            "command: touch should-not-exist\n    restart: {max-attempt: 5}",
            "unknown field",
        ),
        (
            "command: touch should-not-exist\n    depends-on: [{service: db, timeout: 1s}]",
            "did not match any variant",
        ),
    ] {
        let project = Project::new(&format!("services:\n  api:\n    {service}\n"));
        for command in ["check", "graph", "start"] {
            failure(project.invoke(&[command]), message);
        }
        assert!(!project.path().join(".devd").exists());
        assert!(!project.path().join("should-not-exist").exists());
    }
}

#[test]
fn test_cli_restart_stopped_service_and_failure() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\n    restart: {policy: never}\n  once:\n    command: sh -c 'echo once; exit 0'\n    restart: {policy: never}\n");
    let mut supervisor = project.start();
    project.running();
    wait(|| {
        project.snapshot().filter(|s| {
            s.services["once"].status == devd::core::service_manager::ServiceState::Stopped
        })
    });
    // A short-lived command may be observed running or already exited.
    let output = project.invoke(&["restart", "once"]);
    if !output.status.success() {
        failure(output, "exited before restart completed");
    }
    wait(|| {
        project
            .snapshot()
            .filter(|s| s.services["once"].restart_count == 1)
    });
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_restart_failure_reports_cause_and_cleans_stack() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\n    env-file: local.env\n    restart: {policy: never}\n");
    fs::write(project.path().join("local.env"), "VALUE=ok\n").unwrap();
    let mut supervisor = project.start();
    let pid = project.running().services["worker"].pid.unwrap();
    fs::remove_file(project.path().join("local.env")).unwrap();
    failure(project.invoke(&["restart", "worker"]), "local.env");
    supervisor.finish(false);
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

#[test]
fn test_cli_malformed_oversized_and_idle_clients_do_not_block_control() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
    };
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    project.running();
    let path = project.path().join(".devd/devd.yml/control.sock");
    let _idle = UnixStream::connect(&path).unwrap();
    for bytes in [vec![0, 0, 0, 1, b'!'], vec![255, 255, 255, 255]] {
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(&bytes).unwrap();
        let mut length = [0; 4];
        client.read_exact(&mut length).unwrap();
        let mut response = vec![0; u32::from_be_bytes(length) as usize];
        client.read_exact(&mut response).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response).unwrap()["result"],
            "error"
        );
    }
    success(project.invoke(&["status"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_signals_null_output_and_stale_endpoint() {
    for signal in [
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
    ] {
        let project = Project::new(RUNNING);
        let state_dir = project.path().join(".devd/devd.yml");
        fs::create_dir_all(&state_dir).unwrap();
        let stale = std::os::unix::net::UnixListener::bind(state_dir.join("control.sock")).unwrap();
        drop(stale);
        let mut command = project.command(&["start"]);
        let mut supervisor = Supervisor(
            command
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let pid = project.running().services["worker"].pid.unwrap();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(supervisor.0.id() as i32), signal)
            .unwrap();
        supervisor.finish(true);
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        );
        assert!(!state_dir.join("control.sock").exists());
    }
}

#[test]
fn test_cli_idle_followers_release_slots_and_leave_control_available() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        path::Path,
    };
    fn subscribe(path: &Path) -> (UnixStream, serde_json::Value) {
        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let request = br#"{"command":"follow-logs","service":null,"tail":1}"#;
        stream
            .write_all(&(request.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(request).unwrap();
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        assert!(length < 4096);
        let mut response = vec![0; length];
        stream.read_exact(&mut response).unwrap();
        (stream, serde_json::from_slice(&response).unwrap())
    }
    let project =
        Project::new("services:\n  worker:\n    command: sleep 60\n    restart: {policy: never}\n");
    let mut supervisor = project.start();
    project.running();
    let path = project.path().join(".devd/devd.yml/control.sock");
    let mut followers = Vec::new();
    for _ in 0..16 {
        let (stream, response) = subscribe(&path);
        assert_eq!(response["result"], "logs");
        followers.push(stream);
    }
    let (excess, response) = subscribe(&path);
    assert_eq!(response["result"], "error");
    assert!(response["data"]
        .as_str()
        .unwrap()
        .contains("too many log followers"));
    drop(excess);
    success(project.invoke(&["status"]));
    // No service output can wake a leaked subscriber; EOF must release it.
    for _ in 0..40 {
        drop(followers.pop());
        followers.push(wait(|| {
            let (stream, response) = subscribe(&path);
            (response["result"] == "logs").then_some(stream)
        }));
    }
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    for mut stream in followers {
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
}

#[test]
fn test_cli_broken_and_stalled_output_shut_down_without_hanging() {
    for broken in [false, true] {
        let project = Project::new("services:\n  worker:\n    command: sh -c 'while :; do echo noisy-output; done'\n    restart: {policy: never}\n");
        let mut command = project.command(&["start"]);
        let mut supervisor = Supervisor(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        project.running();
        if broken {
            drop(supervisor.0.stdout.take());
        } else {
            // Leave the pipe unread while the producer fills it.
            wait(|| {
                success(project.invoke(&["logs", "--tail", "1000"]))
                    .lines()
                    .count()
                    .eq(&1000)
                    .then_some(())
            });
            success(project.invoke(&["stop"]));
        }
        supervisor.finish(!broken);
    }
}

#[test]
fn test_cli_explicit_state_directory_and_non_socket_preservation() {
    let project = Project::new(RUNNING);
    let state = project.path().join("runtime");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("control.sock"), "keep this file").unwrap();
    failure(
        project.invoke(&["start", "--state-dir", "runtime"]),
        "refusing to replace",
    );
    assert_eq!(
        fs::read_to_string(state.join("control.sock")).unwrap(),
        "keep this file"
    );
    fs::remove_file(state.join("control.sock")).unwrap();
    let mut command = project.command(&["start", "--state-dir", "runtime"]);
    let mut supervisor = Supervisor(
        command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait(|| {
        project
            .invoke(&["status", "--state-dir", "runtime"])
            .status
            .success()
            .then_some(())
    });
    failure(project.invoke(&["status"]), "no reachable devd supervisor");
    success(project.invoke(&["stop", "--state-dir", "runtime"]));
    supervisor.finish(true);
}
