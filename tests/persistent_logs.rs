#![cfg(unix)]

mod support;

use std::{fs, os::unix::fs::symlink, process::Stdio};

use devd::core::service_manager::{RuntimeSnapshot, ServiceState};
use nix::{sys::signal::kill, unistd::Pid};
use support::{failure, success, wait, Project, Supervisor};

const RUNNING: &str =
    "services:\n  worker:\n    command: sh workload.sh worker\n    restart: {policy: never}\n";

fn project() -> Project {
    let project = Project::new(RUNNING);
    fs::write(
        project.path().join("workload.sh"),
        include_str!("fixtures/workload.sh"),
    )
    .unwrap();
    project
}

fn start(project: &Project, args: &[&str]) -> Supervisor {
    Supervisor(
        project
            .command(args)
            .stdout(Stdio::null())
            .stderr(fs::File::create(project.path().join("persistent-stderr")).unwrap())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn test_cli_persistence_is_opt_in_and_survives_runs_shutdown_and_config_removal() {
    let project = project();
    let mut supervisor = project.start();
    project.running();
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    let directory = project.path().join(".devd/devd.yml/logs");
    assert!(!directory.exists());
    failure(
        project.invoke(&["logs", "--stored"]),
        "cannot read stored logs",
    );
    assert!(!directory.exists());
    for _ in 0..2 {
        let mut supervisor = start(
            &project,
            &[
                "start",
                "--persist-logs",
                "--log-max-size",
                "1",
                "--log-keep",
                "2",
            ],
        );
        project.running();
        wait(|| {
            success(project.invoke(&["logs", "worker"]))
                .contains("stderr-ready")
                .then_some(())
        });
        failure(
            project.invoke(&["logs", "--stored"]),
            "stored logs are in use",
        );
        failure(
            project.invoke(&["start", "--persist-logs"]),
            "another supervisor",
        );
        success(project.invoke(&["stop"]));
        supervisor.finish(true);
    }
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    let logs = success(project.invoke(&["logs", "worker", "--stored"]));
    assert_eq!(logs.matches("stdout-ready").count(), 2);
    assert_eq!(logs.matches("stderr-ready").count(), 2);
    assert_eq!(logs.matches("worker stopped").count(), 2);
    let filtered = success(project.invoke(&[
        "logs",
        "worker",
        "--stored",
        "--level",
        "error",
        "--since",
        "5m",
        "--grep",
        "stderr-ready",
        "--tail",
        "1",
    ]));
    assert_eq!(filtered.lines().count(), 1);
    assert!(filtered.contains("[ERROR] worker stderr-ready"));
    assert!(success(project.invoke(&["logs", "absent", "--stored"])).is_empty());
}

#[test]
fn test_cli_stored_logs_are_isolated_by_profile_and_explicit_state_directory() {
    let project = Project::new("services:\n  worker:\n    command: sh -c 'echo $MODE; exec sleep 60'\n    env: {MODE: base}\nprofiles:\n  dev:\n    services:\n      worker:\n        env: {MODE: development}\n  Dev:\n    services:\n      worker:\n        env: {MODE: capitalized}\n");
    let mut supervisors = Vec::new();
    for profile in [None, Some("dev"), Some("Dev")] {
        let mut args = vec!["start", "--persist-logs", "--state-dir", "runtime"];
        if let Some(profile) = profile {
            args.extend(["--profile", profile]);
        }
        supervisors.push(start(&project, &args));
        let mut args = vec!["logs", "--state-dir", "runtime"];
        if let Some(profile) = profile {
            args.extend(["--profile", profile]);
        }
        wait(|| {
            let output = project.invoke(&args);
            (output.status.success() && !output.stdout.is_empty()).then_some(())
        });
    }
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    for ((profile, expected), supervisor) in [
        (None, "base"),
        (Some("dev"), "development"),
        (Some("Dev"), "capitalized"),
    ]
    .into_iter()
    .zip(&mut supervisors)
    {
        let mut args = vec!["stop", "--state-dir", "runtime"];
        if let Some(profile) = profile {
            args.extend(["--profile", profile]);
        }
        success(project.invoke(&args));
        supervisor.finish(true);
        let mut args = vec!["logs", "--stored", "--state-dir", "runtime"];
        if let Some(profile) = profile {
            args.extend(["--profile", profile]);
        }
        let logs = success(project.invoke(&args));
        assert_eq!(logs.lines().count(), 1);
        assert!(logs.trim_end().ends_with(expected), "{logs}");
    }
}

#[test]
fn test_cli_persistence_rejects_invalid_options_before_side_effects() {
    let project = project();
    for args in [
        vec!["start", "--log-max-size", "1"],
        vec!["start", "--log-keep", "1"],
        vec!["start", "--persist-logs", "--log-max-size", "0"],
        vec!["start", "--persist-logs", "--log-max-size", "1025"],
        vec!["start", "--persist-logs", "--log-keep", "0"],
        vec!["start", "--persist-logs", "--log-keep", "101"],
        vec!["logs", "--stored", "--follow"],
    ] {
        assert!(!project.invoke(&args).status.success(), "{args:?}");
        assert!(!project.path().join(".devd").exists());
    }
    failure(
        project.invoke(&["logs", "--stored"]),
        "cannot read stored logs",
    );
    assert!(!project.path().join(".devd").exists());
}

#[test]
fn test_cli_persistence_rotates_and_drains_on_natural_service_exit() {
    let project = Project::new(
        "services:\n  worker:\n    command: sh emit.sh\n    restart: {policy: never}\n",
    );
    fs::write(project.path().join("emit.sh"), "i=0\nwhile [ $i -lt 90 ]; do\n  printf '%16000s\\n' data\n  i=$((i + 1))\n  sleep 0.005\ndone\nprintf 'final-without-newline'\n").unwrap();
    let mut supervisor = start(
        &project,
        &[
            "start",
            "--persist-logs",
            "--log-max-size",
            "1",
            "--log-keep",
            "1",
        ],
    );
    supervisor.finish(true);
    let directory = project.path().join(".devd/devd.yml/logs");
    assert!(directory.join("archive-1.jsonl").exists());
    let mut count = 0;
    for name in ["archive-1.jsonl", "current.jsonl"] {
        let content = fs::read(directory.join(name)).unwrap();
        assert!(content.len() <= 1024 * 1024);
        for line in content
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            let entry: devd::logging::LogEntry = serde_json::from_slice(line).unwrap();
            assert_ne!(entry.level, devd::logging::LogLevel::Warn);
            count += 1;
        }
    }
    assert_eq!(count, 91);
    let logs = success(project.invoke(&["logs", "--stored", "--tail", "1"]));
    assert!(logs.contains("final-without-newline"));
    assert_eq!(logs.lines().count(), 1);
}

#[test]
fn test_cli_persistence_setup_failure_starts_no_services_and_preserves_files() {
    let project = project();
    let directory = project.path().join(".devd/devd.yml/logs");
    fs::create_dir_all(&directory).unwrap();
    let sentinel = project.path().join("sentinel");
    fs::write(&sentinel, "keep me").unwrap();
    symlink(&sentinel, directory.join("current.jsonl")).unwrap();
    failure(
        project.invoke(&["start", "--persist-logs"]),
        "cannot open persistent logs",
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "keep me");
    assert!(!project.path().join("events").exists());
    assert!(!project.path().join(".devd/devd.yml/control.sock").exists());
}

#[test]
fn test_cli_disk_failure_stops_services_and_reports_failed_persistence() {
    let project = project();
    // Emit enough paced data to rotate without relying on filesystem timings.
    let script = include_str!("fixtures/workload.sh").replace(
        "    if [ -f \"crash-$name\" ]; then",
        "    if [ -f flood ]; then\n        printf '%16000s\\n' data\n    fi\n    if [ -f \"crash-$name\" ]; then",
    );
    fs::write(project.path().join("workload.sh"), script).unwrap();
    let mut supervisor = start(
        &project,
        &[
            "start",
            "--persist-logs",
            "--log-max-size",
            "1",
            "--log-keep",
            "1",
        ],
    );
    let snapshot = project.running();
    wait(|| {
        success(project.invoke(&["logs", "worker"]))
            .contains("stdout-ready")
            .then_some(())
    });
    let directory = project.path().join(".devd/devd.yml/logs");
    fs::create_dir(directory.join("archive-1.jsonl")).unwrap();
    fs::write(project.path().join("flood"), "").unwrap();
    supervisor.finish(false);
    let stderr = fs::read_to_string(project.path().join("persistent-stderr")).unwrap();
    assert!(stderr.contains("cannot write persistent logs"), "{stderr}");
    assert!(kill(
        Pid::from_raw(snapshot.services["worker"].pid.unwrap() as i32),
        None
    )
    .is_err());
    let persisted: RuntimeSnapshot = serde_json::from_slice(
        &fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(persisted.services["worker"].status, ServiceState::Stopped);
    assert!(!project.path().join(".devd/devd.yml/control.sock").exists());
}
