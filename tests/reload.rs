#![cfg(unix)]
mod support;

use devd::core::{
    events::query::EventBatch,
    reload::{ChangeKind, ReloadPlan},
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
    assert!(!report.apply_available);
    assert_eq!(
        project.running().services["worker"].pid,
        before.services["worker"].pid
    );
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}
