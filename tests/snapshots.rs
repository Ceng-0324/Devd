#![cfg(unix)]

mod support;

use std::{fs, os::unix::fs::symlink};

use support::{failure, success, Project};

#[test]
fn test_snapshot_round_trip_preserves_whole_document_and_original_config() {
    let project = Project::new(
        "services:\n  worker:\n    command: sleep 60\n    cwd: ./backend\nprofiles:\n  dev:\n    services:\n      worker:\n        env: {MODE: dev}\n",
    );
    let config = project.path().join("devd.yml");
    let original = fs::read(&config).unwrap();
    success(project.invoke(&["snapshot", "save", "working"]));
    assert_eq!(
        fs::read(project.path().join(".devd/devd.yml/snapshots/working.yml")).unwrap(),
        original
    );

    fs::write(&config, b"version: [\n").unwrap();
    success(project.invoke(&[
        "snapshot",
        "restore",
        "working",
        "--output",
        "devd-recovered.yml",
    ]));
    assert_eq!(
        fs::read(project.path().join("devd-recovered.yml")).unwrap(),
        original
    );
    assert_eq!(fs::read(&config).unwrap(), b"version: [\n");
    success(project.invoke(&["check", "--config", "devd-recovered.yml"]));
    success(project.invoke(&[
        "check",
        "--config",
        "devd-recovered.yml",
        "--profile",
        "dev",
    ]));
}

#[test]
fn test_snapshot_restores_deleted_config_and_keeps_invalid_input_exactly() {
    let project = Project::new("services: {}\n");
    let config = project.path().join("devd.yml");
    let bytes = b"version: [\n# Work in progress is still worth saving.\n";
    fs::write(&config, bytes).unwrap();
    success(project.invoke(&["snapshot", "save", "draft", "--state-dir", "state"]));
    assert_eq!(
        fs::read(project.path().join("state/snapshots/draft.yml")).unwrap(),
        bytes
    );
    fs::remove_file(&config).unwrap();
    success(project.invoke(&[
        "snapshot",
        "restore",
        "draft",
        "--output",
        "rescued.yml",
        "--state-dir",
        "state",
    ]));
    assert_eq!(fs::read(project.path().join("rescued.yml")).unwrap(), bytes);
    assert!(!config.exists());
    failure(
        project.invoke(&["check", "--config", "rescued.yml"]),
        "failed to parse",
    );
}

#[test]
fn test_snapshot_rejects_unsafe_names_and_never_overwrites() {
    let project = Project::new("services:\n  worker: {command: sleep 60}\n");
    let config = project.path().join("devd.yml");
    let original = fs::read(&config).unwrap();
    for name in ["../outside", "nested/name", "A", "", &"a".repeat(65)] {
        failure(
            project.invoke(&["snapshot", "save", name]),
            "invalid snapshot name",
        );
    }
    failure(
        project.invoke(&["snapshot", "save", "--color", "never", "--", "-bad"]),
        "invalid snapshot name",
    );
    failure(
        project.invoke(&[
            "snapshot",
            "restore",
            "../outside",
            "--output",
            "rescue.yml",
        ]),
        "invalid snapshot name",
    );
    success(project.invoke(&["snapshot", "save", "working"]));
    failure(
        project.invoke(&["snapshot", "save", "working"]),
        "already exists",
    );
    for filename in [
        "../outside.yml",
        "nested/rescue.yml",
        "/tmp/rescue.yml",
        ".",
    ] {
        failure(
            project.invoke(&["snapshot", "restore", "working", "--output", filename]),
            "--output must be a single filename",
        );
    }
    failure(
        project.invoke(&["snapshot", "restore", "working", "--output", "devd.yml"]),
        "already exists",
    );
    fs::write(project.path().join("rescue.yml"), b"do not replace").unwrap();
    failure(
        project.invoke(&["snapshot", "restore", "working", "--output", "rescue.yml"]),
        "already exists",
    );
    assert_eq!(fs::read(&config).unwrap(), original);
    assert_eq!(
        fs::read(project.path().join("rescue.yml")).unwrap(),
        b"do not replace"
    );
    failure(
        project.invoke(&["snapshot", "save", "other", "--profile", "dev"]),
        "--profile is not supported by snapshot",
    );
}

#[test]
fn test_snapshot_rejects_symlinked_snapshot_and_snapshot_directory() {
    let project = Project::new("services:\n  worker: {command: sleep 60}\n");
    let state = project.path().join("state");
    let snapshots = state.join("snapshots");
    let outside = project.path().join("outside");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir(&outside).unwrap();
    symlink(&outside, &snapshots).unwrap();
    failure(
        project.invoke(&["snapshot", "save", "working", "--state-dir", "state"]),
        "not a regular directory",
    );
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    fs::remove_file(&snapshots).unwrap();
    success(project.invoke(&["snapshot", "save", "working", "--state-dir", "state"]));
    symlink(snapshots.join("working.yml"), snapshots.join("linked.yml")).unwrap();
    failure(
        project.invoke(&[
            "snapshot",
            "restore",
            "linked",
            "--output",
            "rescue.yml",
            "--state-dir",
            "state",
        ]),
        "not a regular file",
    );
    assert!(!project.path().join("rescue.yml").exists());
}

#[test]
fn test_snapshot_does_not_restart_or_reconfigure_live_supervisor() {
    let project = Project::new("services:\n  worker: {command: sleep 60}\n");
    let mut supervisor = project.start();
    let before = project.running();
    success(project.invoke(&["snapshot", "save", "live"]));
    fs::write(project.path().join("devd.yml"), b"version: [\n").unwrap();
    success(project.invoke(&["snapshot", "restore", "live", "--output", "recovered.yml"]));
    let after = project.snapshot().unwrap();
    assert_eq!(after.supervisor_pid, before.supervisor_pid);
    assert_eq!(after.services["worker"].pid, before.services["worker"].pid);
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}
