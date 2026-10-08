#![cfg(unix)]
mod support;

use devd::core::{
    events::query::{EventBatch, EventKind},
    reload::{ChangeKind, ReloadOutcome, ReloadPlan, ReloadReport},
    service_manager::{RuntimeSnapshot, ServiceState},
};
use std::{fs, process::Stdio};
use support::{failure, success, Project, Supervisor};

fn plan(project: &Project, extra: &[&str]) -> ReloadPlan {
    let mut args = vec!["reload", "--dry-run", "--json"];
    args.extend_from_slice(extra);
    serde_json::from_str(&success(project.invoke(&args))).unwrap()
}

#[test]
fn test_cli_reload_preview_uses_running_baseline_and_changes_nothing() {
    let yaml = "services:\n  worker:\n    command: sleep 60\n    env: {TOKEN: startup-secret}\n    restart: {policy: never}\n  child:\n    command: sleep 60\n    depends-on: [worker]\n    restart: {policy: never}\n  isolated: {command: sleep 60, restart: {policy: never}}\n";
    let project = Project::new(yaml);
    let mut supervisor = project.start();
    let before = support::wait(|| {
        project.snapshot().filter(|snapshot| {
            snapshot
                .services
                .values()
                .all(|s| s.pid.is_some() && s.status == ServiceState::Running)
        })
    });
    let state = project.path().join(".devd/devd.yml/services.json");
    // Wait until startup state has reached disk before verifying read-only behavior.
    support::wait(|| {
        fs::read(&state).ok().and_then(|bytes| {
            let snapshot: devd::core::service_manager::RuntimeSnapshot =
                serde_json::from_slice(&bytes).ok()?;
            snapshot
                .services
                .values()
                .all(|s| s.pid.is_some() && s.status == ServiceState::Running)
                .then_some(())
        })
    });
    let no_change = plan(&project, &[]);
    assert_eq!(no_change.run_id, before.event_run_id.as_deref().unwrap());
    assert_eq!(no_change.base_config_id, no_change.candidate_config_id);
    assert!(no_change
        .services
        .values()
        .all(|impact| impact.change == ChangeKind::Unchanged));
    let changed = yaml.replace("startup-secret", "candidate-secret");
    fs::write(
        project.path().join("devd.yml"),
        format!("version: '1'\n{changed}"),
    )
    .unwrap();
    let mut file_before: RuntimeSnapshot =
        serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    let candidate_before = fs::read(project.path().join("devd.yml")).unwrap();
    let events_before: EventBatch =
        serde_json::from_str(&success(project.invoke(&["events", "--json"]))).unwrap();
    let changed_plan = plan(&project, &[]);
    assert_eq!(changed_plan.base_config_id, no_change.base_config_id);
    assert_ne!(
        changed_plan.candidate_config_id,
        no_change.candidate_config_id
    );
    assert_eq!(changed_plan.services["worker"].changed_fields, ["env"]);
    assert_eq!(
        changed_plan.services["child"].change,
        ChangeKind::DependencyAffected
    );
    assert_eq!(
        changed_plan.services["isolated"].change,
        ChangeKind::Unchanged
    );
    assert_eq!(changed_plan.stop_layers, [vec!["child"], vec!["worker"]]);
    assert_eq!(changed_plan.start_layers, [vec!["worker"], vec!["child"]]);
    let text = success(project.invoke(&["reload", "--dry-run"]));
    for value in ["startup-secret", "candidate-secret", "TOKEN", "sleep 60"] {
        assert!(!text.contains(value), "{text}");
    }
    assert!(text.contains("no changes applied"));
    assert!(text.contains("isolated: unchanged"));
    // Resource sampling legitimately updates the state file during a preview.
    // Compare all lifecycle fields while excluding those independent samples.
    let mut file_after: RuntimeSnapshot =
        serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    for snapshot in [&mut file_before, &mut file_after] {
        for state in snapshot.services.values_mut() {
            state.resources = None;
        }
    }
    assert_eq!(
        serde_json::to_value(file_after).unwrap(),
        serde_json::to_value(file_before).unwrap()
    );
    assert_eq!(
        fs::read(project.path().join("devd.yml")).unwrap(),
        candidate_before
    );
    let events_after: EventBatch =
        serde_json::from_str(&success(project.invoke(&["events", "--json"]))).unwrap();
    assert_eq!(events_after.entries, events_before.entries);
    for name in before.services.keys() {
        assert_eq!(
            project.snapshot().unwrap().services[name].pid,
            before.services[name].pid
        );
    }
    assert_eq!(plan(&project, &[]).plan_id, changed_plan.plan_id);
    success(project.invoke(&["restart", "worker"]));
    let restarted = plan(&project, &[]);
    assert_eq!(restarted.base_config_id, changed_plan.base_config_id);
    assert_ne!(restarted.plan_id, changed_plan.plan_id);
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    failure(
        project.invoke(&["reload", "--dry-run"]),
        "no reachable devd supervisor",
    );
}

#[test]
fn test_cli_reload_preview_invalid_candidate_and_boundaries_preserve_services() {
    let project =
        Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = project.start();
    let before = project.running();
    failure(project.invoke(&["reload"]), "--dry-run");
    // The endpoint rejects relative paths and client-selected profiles even
    // when a caller bypasses clap. The instance owns its selected profile.
    for (request, expected) in [
        (
            serde_json::json!({"command": "preview-reload", "candidate": "devd.yml"}),
            "must be absolute",
        ),
        (
            serde_json::json!({"command": "preview-reload", "candidate": "/devd.yml", "profile": "other"}),
            "invalid control message",
        ),
    ] {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixStream,
            time::Duration,
        };
        let mut stream =
            UnixStream::connect(project.path().join(".devd/devd.yml/control.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let bytes = serde_json::to_vec(&request).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let length = u32::from_be_bytes(header) as usize;
        assert!(length < 4096);
        let mut response = vec![0; length];
        stream.read_exact(&mut response).unwrap();
        let error: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(error["result"], "error");
        assert!(
            error["data"].as_str().unwrap().contains(expected),
            "{error}"
        );
    }
    let candidate = project.path().join("candidate.yml");
    for (yaml, error) in [
        ("not: [valid", "parse"),
        ("version: '1'\nservices:\n  worker: {command: sleep 60, depends-on: [missing]}\n", "unknown dependency"),
        ("version: '1'\nservices:\n  worker: {command: sleep 60, depends-on: [child]}\n  child: {command: sleep 60, depends-on: [worker]}\n", "circular dependency"),
        ("version: '1'\nservices:\n  worker: {command: \"sleep 'unterminated\"}\n", "missing closing quote"),
    ] {
        fs::write(&candidate, yaml).unwrap();
        failure(project.invoke(&["reload", "--dry-run", "--candidate", "candidate.yml"]), error);
        assert_eq!(project.running().services["worker"].pid, before.services["worker"].pid);
    }
    fs::write(&candidate, " ".repeat(1024 * 1024 + 1)).unwrap();
    failure(
        project.invoke(&["reload", "--dry-run", "--candidate", "candidate.yml"]),
        "exceeds 1 MiB",
    );
    fs::remove_file(&candidate).unwrap();
    nix::unistd::mkfifo(&candidate, nix::sys::stat::Mode::S_IRUSR).unwrap();
    failure(
        project.invoke(&["reload", "--dry-run", "--candidate", "candidate.yml"]),
        "regular file",
    );
    fs::remove_file(&candidate).unwrap();
    failure(
        project.invoke(&["reload", "--dry-run", "--candidate", "candidate.yml"]),
        "cannot locate candidate",
    );
    assert_eq!(
        project.running().services["worker"].pid,
        before.services["worker"].pid
    );
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_reload_preview_selected_profile_candidate_directory_and_deleted_source() {
    let yaml = "version: '1'\nservices:\n  worker: {command: sleep 60, env: {MODE: base}, restart: {policy: never}}\nprofiles:\n  dev:\n    services:\n      worker: {env: {MODE: dev}}\n";
    let project = Project::from_document(yaml);
    let mut supervisor = Supervisor(
        project
            .command(&["start", "--profile", "dev"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    support::wait(|| {
        project
            .invoke(&["status", "--profile", "dev"])
            .status
            .success()
            .then_some(())
    });
    let baseline = plan(&project, &["--profile", "dev"]);
    assert_eq!(baseline.profile.as_deref(), Some("dev"));
    assert_eq!(baseline.base_config_id, baseline.candidate_config_id);
    fs::write(
        project.path().join("candidate.yml"),
        yaml.replace("MODE: base", "MODE: changed-base"),
    )
    .unwrap();
    let selected = plan(
        &project,
        &["--profile", "dev", "--candidate", "candidate.yml"],
    );
    assert_eq!(selected.candidate_config_id, baseline.base_config_id);
    fs::write(
        project.path().join("candidate.yml"),
        "version: '1'\nservices:\n  worker: {command: sleep 60}\n",
    )
    .unwrap();
    failure(
        project.invoke(&[
            "reload",
            "--dry-run",
            "--profile",
            "dev",
            "--candidate",
            "candidate.yml",
        ]),
        "unknown profile",
    );
    fs::create_dir(project.path().join("other")).unwrap();
    fs::write(project.path().join("other/candidate.yml"), yaml).unwrap();
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    let moved = plan(
        &project,
        &["--profile", "dev", "--candidate", "other/candidate.yml"],
    );
    assert_eq!(moved.base_config_id, baseline.base_config_id);
    assert!(moved.services["worker"]
        .changed_fields
        .contains(&"cwd".into()));
    success(project.invoke(&["stop", "--profile", "dev"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_reload_preview_does_not_execute_candidate_or_inspect_runtime_inputs() {
    let project =
        Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = project.start();
    let before = project.running();
    let candidate = project.path().join("candidate.yml");
    fs::write(&candidate, "version: '1'\nservices:\n  worker:\n    command: sh -c 'touch service-executed'\n    healthcheck: {type: script, command: \"sh -c 'touch probe-executed'\"}\n").unwrap();
    assert_eq!(
        plan(&project, &["--candidate", "candidate.yml"]).services["worker"].change,
        ChangeKind::Modified
    );
    assert!(!project.path().join("service-executed").exists());
    assert!(!project.path().join("probe-executed").exists());
    // Filesystem readiness belongs to doctor/start. Even a FIFO dotenv file
    // must not be opened by the static preview.
    nix::unistd::mkfifo(
        &project.path().join("blocked.env"),
        nix::sys::stat::Mode::S_IRUSR,
    )
    .unwrap();
    fs::write(&candidate, "version: '1'\nservices:\n  worker:\n    command: program-that-does-not-exist\n    cwd: missing-directory\n    env-file: ../blocked.env\n    requires: [{type: file, path: missing-input}]\n    healthcheck: {type: tcp, port: 1}\n").unwrap();
    let report = plan(&project, &["--candidate", "candidate.yml"]);
    assert!(report.apply_available);
    assert_eq!(
        project.running().services["worker"].pid,
        before.services["worker"].pid
    );
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

fn apply(project: &Project, preview: &ReloadPlan, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["reload", "--apply", "--plan", &preview.plan_id, "--json"];
    args.extend_from_slice(extra);
    project.invoke(&args)
}

fn running_stack(project: &Project) -> RuntimeSnapshot {
    support::wait(|| {
        project.snapshot().filter(|snapshot| {
            snapshot.services.values().all(|state| {
                state.pid.is_some()
                    && matches!(state.status, ServiceState::Running | ServiceState::Healthy)
            })
        })
    })
}

#[test]
fn test_cli_reload_apply_selective_graph_changes_updates_baseline_and_order() {
    let project = Project::new("services:\n  worker: {command: sleep 60, env: {TOKEN: old-secret}, restart: {policy: never}}\n  child: {command: sleep 60, depends-on: [worker], restart: {policy: never}}\n  removed: {command: sleep 60, restart: {policy: never}}\n  isolated: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = project.start();
    let before = running_stack(&project);
    let candidate = "version: '1'\nservices:\n  worker: {command: sleep 60, env: {TOKEN: new-secret}, depends-on: [child], restart: {policy: never}}\n  child: {command: sleep 60, restart: {policy: never}}\n  added: {command: sleep 60, depends-on: [worker], restart: {policy: never}}\n  isolated: {command: sleep 60, restart: {policy: never}}\n";
    fs::write(project.path().join("candidate.yml"), candidate).unwrap();
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    let text = success(apply(&project, &preview, &["--candidate", "candidate.yml"]));
    assert!(!text.contains("new-secret"));
    let report: ReloadReport = serde_json::from_str(&text).unwrap();
    assert_eq!(report.outcome, ReloadOutcome::Applied);
    assert!(report.config_committed);
    assert_eq!(report.ready, ["child", "worker", "added"]);
    assert_eq!(report.stopped, ["child", "removed", "worker"]);
    let after = running_stack(&project);
    assert_eq!(
        after.services["isolated"].pid,
        before.services["isolated"].pid
    );
    assert_eq!(
        after.services["isolated"].event_generation,
        before.services["isolated"].event_generation
    );
    assert_eq!(
        after.services["worker"].restart_count,
        before.services["worker"].restart_count + 1
    );
    assert_eq!(after.services["added"].restart_count, 0);
    assert!(!after.services.contains_key("removed"));
    success(project.invoke(&["events", "removed", "--json"]));
    success(project.invoke(&["explain", "removed", "--json"]));
    success(project.invoke(&["logs", "removed"]));
    assert_ne!(
        after.services["worker"].event_generation,
        before.services["worker"].event_generation
    );
    let new_baseline = plan(&project, &["--candidate", "candidate.yml"]);
    assert_eq!(new_baseline.base_config_id, preview.candidate_config_id);
    assert!(new_baseline.stop_layers.is_empty());
    let events: EventBatch =
        serde_json::from_str(&success(project.invoke(&["events", "--json"]))).unwrap();
    let sequence = |name: &str, kind: EventKind, generation: Option<u64>| -> u64 {
        events
            .entries
            .iter()
            .find(|event| {
                event.service.as_deref() == Some(name)
                    && event.generation == generation
                    && event.data.kind() == kind
            })
            .unwrap()
            .sequence
    };
    assert!(
        sequence(
            "child",
            EventKind::Exited,
            before.services["child"].event_generation
        ) < sequence(
            "worker",
            EventKind::Exited,
            before.services["worker"].event_generation
        )
    );
    assert!(
        sequence(
            "child",
            EventKind::Started,
            after.services["child"].event_generation
        ) < sequence(
            "worker",
            EventKind::Started,
            after.services["worker"].event_generation
        )
    );
    assert!(
        sequence(
            "worker",
            EventKind::Started,
            after.services["worker"].event_generation
        ) < sequence(
            "added",
            EventKind::Started,
            after.services["added"].event_generation
        )
    );
    // Restart uses the new in-memory configuration, even though devd.yml is old.
    success(project.invoke(&["restart", "worker"]));
    assert_eq!(
        plan(&project, &["--candidate", "candidate.yml"]).base_config_id,
        preview.candidate_config_id
    );
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_reload_apply_noop_and_stale_candidate_runtime_preserve_services() {
    let project =
        Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = project.start();
    let before = project.running();
    let unchanged = plan(&project, &[]);
    let report: ReloadReport =
        serde_json::from_str(&success(apply(&project, &unchanged, &[]))).unwrap();
    assert!(report.stopped.is_empty() && report.started.is_empty());
    assert_eq!(
        project.running().services["worker"].event_generation,
        before.services["worker"].event_generation
    );
    let candidate = project.path().join("candidate.yml");
    fs::write(&candidate, "version: '1'\nservices:\n  worker: {command: sleep 60, env: {MODE: next}, restart: {policy: never}}\n").unwrap();
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    fs::write(
        &candidate,
        fs::read_to_string(&candidate)
            .unwrap()
            .replace("MODE: next", "MODE: changed"),
    )
    .unwrap();
    failure(
        apply(&project, &preview, &["--candidate", "candidate.yml"]),
        "plan is stale",
    );
    assert_eq!(
        project.running().services["worker"].pid,
        before.services["worker"].pid
    );
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    success(project.invoke(&["restart", "worker"]));
    let restarted = project.running();
    failure(
        apply(&project, &preview, &["--candidate", "candidate.yml"]),
        "plan is stale",
    );
    assert_eq!(
        project.running().services["worker"].pid,
        restarted.services["worker"].pid
    );
    fs::write(
        &candidate,
        "version: '1'\nservices:\n  worker: {command: sleep 60, depends-on: [missing]}\n",
    )
    .unwrap();
    failure(
        apply(&project, &preview, &["--candidate", "candidate.yml"]),
        "unknown dependency",
    );
    assert_eq!(
        project.running().services["worker"].pid,
        restarted.services["worker"].pid
    );
    failure(project.invoke(&["reload", "--apply"]), "--plan");
    failure(
        project.invoke(&["reload", "--apply", "--plan", "invalid"]),
        "plan ID",
    );
    failure(
        project.invoke(&["reload", "--dry-run", "--apply", "--plan", &preview.plan_id]),
        "cannot be used",
    );
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_reload_apply_waits_readiness_rejects_controls_and_stop_preempts() {
    let project = Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\n  isolated: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = project.start();
    let before = running_stack(&project);
    fs::write(project.path().join("candidate.yml"), "version: '1'\nservices:\n  worker:\n    command: sleep 60\n    healthcheck: {type: script, command: \"sh -c 'test -f ready'\", interval: 100ms, retries: 1000, timeout: 1s}\n    restart: {policy: never}\n  child: {command: \"sh -c 'touch child-started; sleep 60'\", depends-on: [{service: worker, condition: script-ready}], restart: {policy: never}}\n  isolated: {command: sleep 60, restart: {policy: never}}\n").unwrap();
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    let command = project.command(&[
        "reload",
        "--apply",
        "--plan",
        &preview.plan_id,
        "--candidate",
        "candidate.yml",
        "--json",
    ]);
    let request = std::thread::spawn(move || support::command_output(command));
    support::wait(|| {
        project.snapshot().filter(|snapshot| {
            snapshot.services.contains_key("child") && snapshot.services["worker"].pid.is_some()
        })
    });
    assert!(!project.path().join("child-started").exists());
    failure(
        project.invoke(&["restart", "isolated"]),
        "reload is in progress",
    );
    failure(
        apply(&project, &preview, &["--candidate", "candidate.yml"]),
        "already in progress",
    );
    assert_eq!(
        project.snapshot().unwrap().services["isolated"].pid,
        before.services["isolated"].pid
    );
    success(project.invoke(&["stop"]));
    let output = request.join().unwrap();
    assert!(!output.status.success());
    let report: ReloadReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.outcome, ReloadOutcome::Interrupted);
    assert!(report.config_committed);
    assert_eq!(report.started, ["worker"]);
    assert!(report.ready.is_empty());
    supervisor.finish(true);
    assert!(!project.path().join("child-started").exists());
    let final_state: RuntimeSnapshot = serde_json::from_slice(
        &fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap(),
    )
    .unwrap();
    assert!(final_state
        .services
        .values()
        .all(|state| state.status == ServiceState::Stopped && state.pid.is_none()));
}

#[test]
fn test_cli_reload_apply_failure_stops_whole_stack_without_automatic_retry() {
    let project = Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\n  isolated: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = Supervisor(
        project
            .command(&["start", "--persist-events"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    running_stack(&project);
    fs::write(project.path().join("candidate.yml"), "version: '1'\nservices:\n  worker: {command: devd-program-that-does-not-exist, restart: {policy: always, initial-delay: 1ms, max-attempts: 10000}}\n  isolated: {command: sleep 60, restart: {policy: never}}\n").unwrap();
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    let output = apply(&project, &preview, &["--candidate", "candidate.yml"]);
    assert!(!output.status.success());
    let report: ReloadReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.outcome, ReloadOutcome::Failed);
    assert!(report.config_committed);
    assert_eq!(report.stopped, ["worker"]);
    assert!(report.started.is_empty() && report.ready.is_empty());
    supervisor.finish(false);
    let final_state: RuntimeSnapshot = serde_json::from_slice(
        &fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap(),
    )
    .unwrap();
    assert!(final_state
        .services
        .values()
        .all(|state| state.pid.is_none()));
    assert_eq!(final_state.services["worker"].restart_count, 1);
    assert_eq!(final_state.services["worker"].status, ServiceState::Failed);
    assert_eq!(
        final_state.services["isolated"].status,
        ServiceState::Stopped
    );
    let stored: EventBatch =
        serde_json::from_str(&success(project.invoke(&["events", "--stored", "--json"]))).unwrap();
    assert!(stored.entries.iter().any(|event| matches!(&event.data,
        devd::core::events::EventData::ReloadFinished { outcome: ReloadOutcome::Failed, config_committed: true, stopped, failure: Some(_), .. } if stopped == &["worker"])));
    assert_eq!(
        stored
            .entries
            .iter()
            .filter(|event| matches!(
                event.data,
                devd::core::events::EventData::SpawnFailed { .. }
            ))
            .count(),
        1
    );
}

#[test]
fn test_cli_reload_apply_releases_loading_actors_and_health_gates_next_layer() {
    let project =
        Project::new("services:\n  worker: {command: sleep 60, restart: {policy: never}}\n");
    let mut supervisor = project.start();
    project.running();
    fs::write(project.path().join("candidate.yml"), "version: '1'\nservices:\n  worker:\n    command: sleep 60\n    healthcheck: {type: script, command: \"sh -c 'test -f ready'\", interval: 100ms, retries: 1000, timeout: 1s}\n    restart: {policy: never}\n  child: {command: \"sh -c 'touch child-started; sleep 60'\", depends-on: [worker], restart: {policy: never}}\n").unwrap();
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    let command = project.command(&[
        "reload",
        "--apply",
        "--plan",
        &preview.plan_id,
        "--candidate",
        "candidate.yml",
        "--json",
    ]);
    let request = std::thread::spawn(move || support::command_output(command));
    let loading = support::wait(|| {
        project.snapshot().filter(|snapshot| {
            snapshot.services.contains_key("child") && snapshot.services["worker"].pid.is_some()
        })
    });
    assert!(!project.path().join("child-started").exists());
    fs::write(project.path().join("ready"), "ready").unwrap();
    let report: ReloadReport = serde_json::from_str(&success(request.join().unwrap())).unwrap();
    assert_eq!(report.ready, ["worker", "child"]);
    let after = running_stack(&project);
    assert_eq!(
        after.services["worker"].event_generation,
        loading.services["worker"].event_generation
    );
    assert!(project.path().join("child-started").exists());
    // Loading -> Running must preserve the process, and normal restart works.
    success(project.invoke(&["restart", "worker"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_reload_stop_before_commit_keeps_old_baseline_and_never_spawns_candidate() {
    let project = Project::new("services:\n  worker:\n    command: \"sh -c 'trap \\\"\\\" TERM; touch ready; while :; do sleep 1; done'\"\n    restart: {policy: never}\n");
    let mut supervisor = project.start();
    project.running();
    support::wait(|| project.path().join("ready").exists().then_some(()));
    fs::write(project.path().join("candidate.yml"), "version: '1'\nservices:\n  worker: {command: \"sh -c 'touch candidate-started; sleep 60'\", restart: {policy: never}}\n").unwrap();
    let preview = plan(&project, &["--candidate", "candidate.yml"]);
    let command = project.command(&[
        "reload",
        "--apply",
        "--plan",
        &preview.plan_id,
        "--candidate",
        "candidate.yml",
        "--json",
    ]);
    let request = std::thread::spawn(move || support::command_output(command));
    support::wait(|| {
        project
            .snapshot()
            .filter(|snapshot| snapshot.services["worker"].status == ServiceState::Stopping)
    });
    assert_eq!(
        plan(&project, &["--candidate", "candidate.yml"]).base_config_id,
        preview.base_config_id
    );
    success(project.invoke(&["stop"]));
    let output = request.join().unwrap();
    assert!(!output.status.success());
    let report: ReloadReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.outcome, ReloadOutcome::Interrupted);
    assert!(!report.config_committed);
    assert_eq!(report.stopped, ["worker"]);
    assert!(report.started.is_empty());
    assert!(!project.path().join("candidate-started").exists());
    supervisor.finish(true);
}

#[test]
fn test_cli_reload_apply_updates_resource_limits_and_selected_profile() {
    let project = Project::from_document("version: '1'\nservices:\n  worker: {command: sleep 60, limits: {memory: 1024GiB}, restart: {policy: never}}\nprofiles:\n  dev:\n    services:\n      worker: {env: {MODE: before}}\n");
    let mut supervisor = Supervisor(
        project
            .command(&["start", "--profile", "dev"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    support::wait(|| {
        let output = project.invoke(&["status", "--profile", "dev", "--json"]);
        if !output.status.success() {
            return None;
        }
        let snapshot: RuntimeSnapshot = serde_json::from_slice(&output.stdout).unwrap();
        snapshot.services["worker"].pid.is_some().then_some(())
    });
    fs::create_dir(project.path().join("other")).unwrap();
    fs::write(project.path().join("other/candidate.yml"), "version: '1'\nservices:\n  worker: {command: sleep 60, limits: {memory: 1024GiB}, restart: {policy: never}}\nprofiles:\n  dev:\n    services:\n      worker: {env: {MODE: after}, limits: {memory: 1B}}\n").unwrap();
    let extra = ["--profile", "dev", "--candidate", "other/candidate.yml"];
    let preview = plan(&project, &extra);
    let report: ReloadReport =
        serde_json::from_str(&success(apply(&project, &preview, &extra))).unwrap();
    assert_eq!(report.ready, ["worker"]);
    support::wait(|| {
        let events: EventBatch = serde_json::from_str(&success(project.invoke(&[
            "events",
            "--profile",
            "dev",
            "--json",
        ])))
        .unwrap();
        events
            .entries
            .iter()
            .any(|event| {
                matches!(
                    event.data,
                    devd::core::events::EventData::ResourceChanged { exceeded: true, .. }
                )
            })
            .then_some(())
    });
    assert_eq!(
        plan(&project, &extra).base_config_id,
        preview.candidate_config_id
    );
    let output = project.invoke(&["status", "--profile", "dev", "--json"]);
    let snapshot: RuntimeSnapshot = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot.services["worker"].restart_count, 1);
    assert!(snapshot.services["worker"]
        .resource_restart_reason
        .is_none());
    success(project.invoke(&["stop", "--profile", "dev"]));
    supervisor.finish(true);
}

#[tokio::test]
async fn test_reload_readiness_deadline_cleans_stack_and_pending_services() {
    use devd::{
        config::DevdConfig,
        core::service_manager::{ManagerOptions, ServiceManager},
    };
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let base: DevdConfig = serde_yaml::from_str(
        "version: '1'\nservices:\n  worker: {command: sleep 60, restart: {policy: never}}\n",
    )
    .unwrap();
    let mut options = ManagerOptions::new(root.path().join("state.json"));
    options.dependency_timeout = Duration::from_millis(150);
    options.grace_period = Duration::from_millis(50);
    let manager = ServiceManager::new(base.clone(), options).unwrap();
    let controller = manager.controller();
    let mut snapshots = manager.subscribe();
    let run = tokio::spawn(manager.run_until(std::future::pending()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while snapshots.borrow_and_update().services["worker"]
            .pid
            .is_none()
        {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let candidate: DevdConfig = serde_yaml::from_str("version: '1'\nservices:\n  worker:\n    command: sleep 60\n    healthcheck: {type: script, command: \"sh -c 'sleep 60'\", timeout: 10s}\n    restart: {policy: never}\n  child: {command: sleep 60, depends-on: [worker], restart: {policy: never}}\n").unwrap();
    let preview =
        devd::core::reload::preview(&base, &candidate, &snapshots.borrow(), None).unwrap();
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        controller.reload(candidate, preview.plan_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.outcome, ReloadOutcome::Failed);
    assert!(report.failure.unwrap().contains("readiness timed out"));
    assert_eq!(report.started, ["worker"]);
    assert!(report.ready.is_empty());
    assert!(run.await.unwrap().is_err());
    assert!(snapshots
        .borrow()
        .services
        .values()
        .all(|state| state.status == ServiceState::Stopped && state.pid.is_none()));
}
