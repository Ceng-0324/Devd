#![cfg(unix)]
mod support;

use devd::core::events::{
    query::{EventBatch, EventGap, EventSource, PersistenceState},
    EventData,
};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use std::{fs, io::Write, process::Stdio};
use support::{failure, success, wait, Project, Supervisor};

fn project() -> Project {
    Project::new("services:\n  worker:\n    command: sleep 60\n    restart: {policy: never}\n")
}
fn start(project: &Project, args: &[&str]) -> Supervisor {
    Supervisor(
        project
            .command(args)
            .stdout(Stdio::null())
            .stderr(fs::File::create(project.path().join("event-stderr")).unwrap())
            .spawn()
            .unwrap(),
    )
}
fn events(project: &Project, args: &[&str]) -> EventBatch {
    let mut command = vec!["events", "--json"];
    command.extend(args);
    serde_json::from_str(&success(project.invoke(&command))).unwrap()
}

#[test]
fn test_cli_events_query_filters_cursors_restart_and_missing_config() {
    let project = project();
    let mut supervisor = project.start();
    project.running();
    let batch = events(&project, &[]);
    assert_eq!(batch.source, EventSource::Live);
    assert_eq!(batch.persistence, Some(PersistenceState::Disabled));
    assert!(batch
        .entries
        .iter()
        .any(|e| matches!(e.data, EventData::SupervisorStarted)));
    let cursor = batch.cursor.unwrap().to_string();
    success(project.invoke(&["restart", "worker"]));
    let resumed = events(
        &project,
        &[
            "--cursor",
            &cursor,
            "worker",
            "--type",
            "manual-restart-requested",
        ],
    );
    assert_eq!(resumed.entries.len(), 1);
    assert_eq!(resumed.entries[0].service.as_deref(), Some("worker"));
    let started = events(
        &project,
        &[
            "worker", "--type", "started", "--tail", "1", "--since", "5m",
        ],
    );
    assert_eq!(started.entries.len(), 1);
    assert!(started.gaps.contains(&EventGap::TailLimited {
        matching_records: 1
    }));
    failure(project.invoke(&["events", "missing"]), "unknown service");
    let future = format!("{}:999999", started.context.unwrap().run_id);
    failure(project.invoke(&["events", "--cursor", &future]), "ahead");
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    assert!(!events(&project, &[]).entries.is_empty());
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    assert!(!project.path().join(".devd/devd.yml/events").exists());
}

fn followed(path: &std::path::Path) -> Vec<EventBatch> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn test_cli_event_follow_handoff_ctrl_c_and_shutdown() {
    let project = project();
    let mut supervisor = project.start();
    project.running();
    for stop in [false, true] {
        let path = project.path().join("follow");
        let mut follower = Supervisor(
            project
                .command(&["events", "--follow", "--json"])
                .stdout(fs::File::create(&path).unwrap())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        wait(|| (!followed(&path).is_empty()).then_some(()));
        success(project.invoke(&["restart", "worker"]));
        wait(|| {
            followed(&path)
                .iter()
                .any(|b| {
                    b.entries
                        .iter()
                        .any(|e| matches!(e.data, EventData::ManualRestartRequested))
                })
                .then_some(())
        });
        if stop {
            success(project.invoke(&["stop"]));
            supervisor.finish(true);
        } else {
            kill(Pid::from_raw(follower.0.id() as i32), Signal::SIGINT).unwrap();
        }
        follower.finish(true);
        let batches = followed(&path);
        let all: Vec<_> = batches.iter().flat_map(|batch| &batch.entries).collect();
        assert!(all
            .windows(2)
            .all(|pair| pair[0].sequence + 1 == pair[1].sequence));
        assert!(batches.iter().all(|b| b.gaps.is_empty()));
        if stop {
            assert!(matches!(
                all.last().unwrap().data,
                EventData::SupervisorStopped { .. }
            ));
        } else {
            assert!(project.snapshot().is_some());
        }
    }
}

#[test]
fn test_cli_events_persistence_opt_in_multi_run_offline_and_profile_isolation() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\nprofiles:\n  dev:\n    services:\n      worker: {}\n");
    let mut old_cursor: Option<String> = None;
    for _ in 0..2 {
        let mut supervisor = start(&project, &["start", "--persist-events"]);
        project.running();
        let batch = events(&project, &[]);
        assert_eq!(batch.persistence, Some(PersistenceState::Recording));
        if let Some(cursor) = old_cursor {
            assert!(events(&project, &["--cursor", &cursor])
                .gaps
                .iter()
                .any(|g| matches!(g, EventGap::DifferentRun { .. })));
        }
        old_cursor = Some(batch.cursor.unwrap().to_string());
        failure(
            project.invoke(&["events", "--stored"]),
            "stored history is in use",
        );
        success(project.invoke(&["stop"]));
        supervisor.finish(true);
    }
    assert!(!project.path().join(".devd/devd.yml/logs").exists());
    let mut profile = start(
        &project,
        &[
            "start",
            "--persist-events",
            "--profile",
            "dev",
            "--state-dir",
            "custom",
        ],
    );
    wait(|| {
        project
            .invoke(&["events", "--profile", "dev", "--state-dir", "custom"])
            .status
            .success()
            .then_some(())
    });
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    success(project.invoke(&["stop", "--profile", "dev", "--state-dir", "custom"]));
    profile.finish(true);
    let history = events(&project, &["--stored"]);
    assert_eq!(history.source, EventSource::Stored);
    assert!(history.gaps.is_empty());
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|e| matches!(e.data, EventData::SupervisorStopped { .. }))
            .count(),
        2
    );
    let history = events(
        &project,
        &["--stored", "--profile", "dev", "--state-dir", "custom"],
    );
    assert_eq!(history.context.unwrap().profile.as_deref(), Some("dev"));
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|e| matches!(e.data, EventData::SupervisorStarted))
            .count(),
        1
    );
}

#[test]
fn test_cli_event_options_and_unsafe_storage_fail_before_services_start() {
    let project = project();
    for args in [
        vec!["start", "--event-max-size", "1"],
        vec!["start", "--event-keep", "1"],
        vec!["start", "--persist-events", "--event-max-size", "0"],
        vec!["start", "--persist-events", "--event-keep", "101"],
        vec!["events", "--stored", "--follow"],
        vec!["events", "--cursor", "bad:1"],
        vec!["events", "--type", "made-up"],
        vec!["events", "--tail", "0"],
    ] {
        assert!(!project.invoke(&args).status.success());
        assert!(!project.path().join(".devd").exists());
    }
    failure(
        project.invoke(&["events", "--stored"]),
        "cannot read stored events",
    );
    assert!(!project.path().join(".devd").exists());
    let directory = project.path().join(".devd/devd.yml/events");
    fs::create_dir_all(&directory).unwrap();
    let target = project.path().join("sentinel");
    fs::write(&target, "keep me").unwrap();
    std::os::unix::fs::symlink(&target, directory.join("current.jsonl")).unwrap();
    failure(
        project.invoke(&["start", "--persist-events"]),
        "cannot open persistent events",
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "keep me");
    assert!(!project.path().join(".devd/devd.yml/control.sock").exists());
}

#[test]
fn test_cli_event_disk_failure_disables_recording_but_services_keep_running() {
    let project = project();
    let directory = project.path().join(".devd/devd.yml/events");
    fs::create_dir_all(&directory).unwrap();
    // A valid retained run nearly fills the first file. New lifecycle events
    // reach rotation quickly without timing-dependent event floods.
    let context = serde_json::json!({"run_id": uuid::Uuid::new_v4().to_string(), "state_path": "state", "profile": null});
    let line = format!(
        "{}\n",
        serde_json::json!({"schema_version":1, "context":context, "record":{"record":"gap", "data":{"reason":"subscriber-lag", "run_id":context["run_id"], "skipped":1}}})
    );
    let mut file = fs::File::create(directory.join("current.jsonl")).unwrap();
    for _ in 0..((1024 * 1024 - 16000) / line.len()) {
        file.write_all(line.as_bytes()).unwrap();
    }
    drop(file);
    let mut supervisor = start(
        &project,
        &[
            "start",
            "--persist-events",
            "--event-max-size",
            "1",
            "--event-keep",
            "1",
        ],
    );
    project.running();
    fs::create_dir(directory.join("archive-1.jsonl")).unwrap();
    for _ in 0..30 {
        success(project.invoke(&["restart", "worker"]));
        if events(&project, &[]).persistence == Some(PersistenceState::Failed) {
            break;
        }
    }
    wait(|| (events(&project, &[]).persistence == Some(PersistenceState::Failed)).then_some(()));
    let pid = project.running().services["worker"].pid.unwrap();
    assert!(kill(Pid::from_raw(pid as i32), None).is_ok());
    let batch = events(&project, &[]);
    assert!(batch
        .entries
        .iter()
        .any(|e| matches!(e.data, EventData::ManualRestartRequested)));
    assert!(fs::read_to_string(project.path().join("event-stderr"))
        .unwrap()
        .contains("services continue"));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    fs::remove_dir(directory.join("archive-1.jsonl")).unwrap();
    assert!(events(&project, &["--stored"])
        .gaps
        .iter()
        .any(|g| matches!(g, EventGap::IncompleteRun { .. })));
}
