use super::*;
use std::future::pending;

fn package(root: &Path, value: Value) {
    std::fs::write(
        root.join("package.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

#[test]
fn test_scan_manager_precedence_and_literal_script_arguments() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("pnpm-lock.yaml"), "must not be read").unwrap();
    package(
        root.path(),
        serde_json::json!({"packageManager":"npm@10", "scripts":{"build assets":"touch must-not-exist", "dev":"node app.js"}}),
    );
    let report = scan::node(root.path(), &Control::new()).unwrap();
    assert_eq!(report.package_manager, "npm");
    assert!(!report.complete);
    assert_eq!(
        shell_words::split(&report.scripts[0].command).unwrap(),
        ["npm", "run", "build assets"]
    );
    assert_eq!(report.scripts[0].script_body, "touch must-not-exist");
    assert!(!root.path().join("must-not-exist").exists());
}

#[test]
fn test_scan_bounds_omissions_and_unconfirmed_workspaces() {
    let root = tempfile::tempdir().unwrap();
    let scripts: serde_json::Map<String, Value> = (0..140)
        .map(|i| (format!("script-{i:03}"), Value::from("do-not-run")))
        .collect();
    package(
        root.path(),
        serde_json::json!({"packageManager":"bun@1", "scripts":scripts, "workspaces": (0..140).map(|i| format!("packages/{i}")).collect::<Vec<_>>()}),
    );
    let report = scan::node(root.path(), &Control::new()).unwrap();
    assert_eq!(report.scripts.len(), 128);
    assert_eq!(report.omitted_scripts, 12);
    assert_eq!(report.workspaces.len(), 128);
    assert_eq!(report.omitted_workspaces, 12);
    assert!(!report.complete);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("not scanned")));
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::to_value(scan::node(root.path(), &Control::new()).unwrap()).unwrap()
    );
}

#[test]
fn test_scan_rejects_bad_metadata_oversize_and_cancelled_reads() {
    let root = tempfile::tempdir().unwrap();
    for value in [
        serde_json::json!([]),
        serde_json::json!({"scripts":[]}),
        serde_json::json!({"packageManager":"other@1"}),
    ] {
        package(root.path(), value);
        assert!(scan::node(root.path(), &Control::new()).is_err());
    }
    std::fs::write(
        root.path().join("package.json"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .unwrap();
    assert!(scan::node(root.path(), &Control::new()).is_err());
    package(
        root.path(),
        serde_json::json!({"scripts":{"build":"never-run"}}),
    );
    let cancelled = Control::new();
    cancelled.cancel();
    assert!(scan::node(root.path(), &cancelled).is_err());
    assert!(scan::node(root.path(), &Control::expired()).is_err());
}

#[test]
fn test_scan_invalid_scripts_and_workspace_entries_are_not_silent() {
    let root = tempfile::tempdir().unwrap();
    package(
        root.path(),
        serde_json::json!({"scripts":{"--help":"bad", "bad\nname":"bad", "empty":"", "numeric":2, "build":"build"}, "workspaces":[false, "packages/*"]}),
    );
    let report = scan::node(root.path(), &Control::new()).unwrap();
    assert_eq!(report.scripts.len(), 1);
    assert_eq!(report.omitted_scripts, 4);
    assert_eq!(report.omitted_workspaces, 1);
    assert!(report.scripts[0].role_hint == Some("task"));
    assert!(!report.complete);
}

#[test]
fn test_init_cancelled_publication_leaves_no_partial_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("devd.yml");
    let control = Control::new();
    control.cancel();
    assert!(publish_blocking(&path, "version: '1'", &control).is_err());
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let control = Control::new();
    publish_blocking(&path, "original", &control).unwrap();
    assert!(publish_blocking(&path, "replacement", &Control::new()).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test(start_paused = true)]
async fn test_init_output_backpressure_and_cancellation() {
    let (mut writer, _reader) = tokio::io::duplex(1);
    let error = write_to(&mut writer, "blocked", pending())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let error = write_to(&mut writer, "blocked", async {})
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}

#[test]
fn test_scan_refuses_linked_manifest_and_follows_no_env_file() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("actual.json");
    std::fs::write(
        &target,
        r#"{"packageManager":"npm@10","scripts":{"build":"never-run"}}"#,
    )
    .unwrap();
    let manifest = root.path().join("package.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &manifest).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &manifest)
        .expect("Windows validation requires symlink privileges");
    assert!(scan::node(root.path(), &Control::new()).is_err());
    std::fs::remove_file(&manifest).unwrap();
    std::fs::hard_link(&target, &manifest).unwrap();
    assert!(scan::node(root.path(), &Control::new()).is_err());
    std::fs::remove_file(&manifest).unwrap();
    std::fs::rename(target, &manifest).unwrap();
    #[cfg(unix)]
    nix::unistd::mkfifo(&root.path().join(".env"), nix::sys::stat::Mode::S_IRUSR).unwrap();
    #[cfg(windows)]
    std::fs::create_dir(root.path().join(".env")).unwrap();
    assert!(scan::node(root.path(), &Control::new()).is_ok());
}

#[test]
fn test_scan_manager_detection_and_unresolved_conflicts() {
    for (manager, filename) in [
        ("npm", "package-lock.json"),
        ("pnpm", "pnpm-lock.yaml"),
        ("yarn", "yarn.lock"),
        ("bun", "bun.lock"),
    ] {
        let root = tempfile::tempdir().unwrap();
        package(
            root.path(),
            serde_json::json!({"scripts":{"build":"build"}, "ignored-future-metadata":true}),
        );
        std::fs::write(root.path().join(filename), "do-not-parse").unwrap();
        assert_eq!(
            scan::node(root.path(), &Control::new())
                .unwrap()
                .package_manager,
            manager
        );
    }
    let root = tempfile::tempdir().unwrap();
    package(
        root.path(),
        serde_json::json!({"scripts":{"build":"build"}}),
    );
    std::fs::write(root.path().join("yarn.lock"), "do-not-parse").unwrap();
    std::fs::write(root.path().join("pnpm-lock.yaml"), "do-not-parse").unwrap();
    assert!(scan::node(root.path(), &Control::new()).is_err());
    std::fs::write(root.path().join("package.json"), b"{\"name\":\"\xff\"}").unwrap();
    assert!(scan::node(root.path(), &Control::new()).is_err());
}
