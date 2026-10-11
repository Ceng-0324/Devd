use super::*;

fn link(target: impl AsRef<Path>, path: impl AsRef<Path>, directory: bool) {
    #[cfg(unix)]
    {
        let _ = directory;
        std::os::unix::fs::symlink(target, path).unwrap();
    }
    #[cfg(windows)]
    {
        let result = if directory {
            std::os::windows::fs::symlink_dir(target, path)
        } else {
            std::os::windows::fs::symlink_file(target, path)
        };
        result.expect("Windows symlink tests require Developer Mode or symlink privileges");
    }
}

#[test]
fn test_direct_metadata_and_special_file_without_opening_contents() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("data");
    fs::write(&path, "secret").unwrap();
    let report = inspect(&path);
    assert_eq!(report.failure, None);
    assert_eq!(report.nodes.last().unwrap().kind, NodeKind::File);
    assert_eq!(
        fs::canonicalize(report.resolved.unwrap()).unwrap(),
        fs::canonicalize(&path).unwrap()
    );
    assert!(report.complete);
    assert!(!report.atomic);
    #[cfg(unix)]
    {
        let fifo = root.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
        let report = inspect(&fifo);
        assert_eq!(report.failure, None);
        assert_eq!(report.nodes.last().unwrap().kind, NodeKind::Special);
    }
}

#[test]
fn test_relative_links_directory_links_and_parent_resolution() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("real/sub")).unwrap();
    fs::write(root.path().join("real/file"), "secret").unwrap();
    link("real/sub", root.path().join("directory"), true);
    link("directory/../file", root.path().join("first"), false);
    link("first", root.path().join("second"), false);
    let report = inspect(&root.path().join("second"));
    #[cfg(windows)]
    {
        assert_eq!(report.failure, Some(Failure::Unsupported));
        assert!(!report.complete);
    }
    #[cfg(unix)]
    {
        assert_eq!(report.failure, None, "{report:?}");
        assert_eq!(
            fs::canonicalize(report.resolved.unwrap()).unwrap(),
            fs::canonicalize(root.path().join("real/file")).unwrap()
        );
        assert!(report
            .nodes
            .iter()
            .any(|n| n.target.as_deref() == Some("directory/../file")));
        assert!(report.complete);
    }
}

#[test]
fn test_absolute_link_target_including_windows_verbatim_paths() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    fs::write(&file, "").unwrap();
    let target = fs::canonicalize(&file).unwrap();
    link(&target, root.path().join("absolute"), false);
    let report = inspect(&root.path().join("absolute"));
    assert_eq!(report.failure, None, "{report:?}");
    assert_eq!(fs::canonicalize(report.resolved.unwrap()).unwrap(), target);
}

#[test]
fn test_missing_target_missing_suffix_and_wrong_intermediate_type() {
    let root = tempfile::tempdir().unwrap();
    link("missing", root.path().join("dangling"), false);
    let report = inspect(&root.path().join("dangling"));
    assert_eq!(report.failure, Some(Failure::DanglingSymlink));
    assert!(report.nodes[report.failed_node.unwrap()]
        .path
        .ends_with("missing"));
    fs::create_dir(root.path().join("directory")).unwrap();
    link("directory", root.path().join("alias"), true);
    assert_eq!(
        inspect(&root.path().join("alias/missing")).failure,
        Some(Failure::Missing)
    );
    fs::write(root.path().join("file"), "").unwrap();
    for suffix in ["file/child", "file/"] {
        assert_eq!(
            inspect(&root.path().join(suffix)).failure,
            Some(Failure::WrongType),
            "{suffix}"
        );
    }
    #[cfg(unix)]
    for suffix in ["file/.", "file/../other"] {
        assert_eq!(
            inspect(&root.path().join(suffix)).failure,
            Some(Failure::WrongType)
        );
    }
    #[cfg(windows)]
    {
        let path = root.path().join("file/.");
        assert!(fs::metadata(&path).unwrap().is_file());
        assert_eq!(inspect(&path).failure, None);
    }
}

#[test]
fn test_cycles_and_repeated_noncyclic_directory_link() {
    let root = tempfile::tempdir().unwrap();
    link("two", root.path().join("one"), false);
    link("one", root.path().join("two"), false);
    assert_eq!(
        inspect(&root.path().join("one")).failure,
        Some(Failure::SymlinkCycle)
    );
    link(".", root.path().join("self"), true);
    fs::write(root.path().join("file"), "").unwrap();
    assert_eq!(inspect(&root.path().join("self/self/file")).failure, None);
}

#[test]
fn test_link_depth_path_and_node_limits_are_incomplete() {
    let root = tempfile::tempdir().unwrap();
    for n in 0..=MAX_LINKS {
        link(
            format!("link{}", n + 1),
            root.path().join(format!("link{n}")),
            false,
        );
    }
    assert_eq!(
        inspect(&root.path().join("link0")).failure,
        Some(Failure::Limit)
    );
    let oversized = inspect(Path::new(&"x".repeat(MAX_PATH_BYTES + 1)));
    assert_eq!(oversized.failure, Some(Failure::Limit));
    assert!(!oversized.complete);
    let too_many = inspect(&root.path().join("a/".repeat(MAX_NODES + 1)));
    assert_eq!(too_many.failure, Some(Failure::Limit));
}

#[test]
fn test_recheck_marks_replacement_and_missing_target_creation_uncertain() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("before"), "").unwrap();
    fs::write(root.path().join("after"), "").unwrap();
    let path = root.path().join("link");
    link("before", &path, false);
    let report = inspect_with(&path, || {
        fs::remove_file(&path).unwrap();
        link("after", &path, false);
    });
    assert!(report.change_detected);
    assert!(!report.complete);
    let missing = root.path().join("missing");
    let report = inspect_with(&missing, || fs::write(&missing, "").unwrap());
    assert_eq!(report.failure, Some(Failure::Missing));
    assert!(report.change_detected);
}

#[test]
fn test_json_node_budget_includes_escaped_paths() {
    let mut report = inspect(Path::new("/"));
    report.nodes.clear();
    let mut bytes = 2;
    for _ in 0..MAX_NODES {
        let node = Node {
            path: "\u{1b}".repeat(MAX_PATH_BYTES),
            kind: NodeKind::Unknown,
            target: None,
        };
        if push_node(&mut report, node, &mut bytes).is_err() {
            break;
        }
    }
    assert!(report.nodes.len() < MAX_NODES);
    assert!(serde_json::to_vec(&report.nodes).unwrap().len() <= MAX_CHAIN_BYTES);
}

#[cfg(windows)]
#[test]
fn test_windows_device_namespace_is_not_traversed() {
    let report = inspect(Path::new(r"\\.\pipe\devd-inspection-must-not-open"));
    assert_eq!(report.failure, Some(Failure::Unsupported));
    assert!(!report.complete);
    assert!(report.nodes.is_empty());
}

#[cfg(unix)]
#[test]
fn test_non_utf8_target_is_unknown_without_lossy_identity() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    link(
        OsString::from_vec(vec![0xff]),
        root.path().join("link"),
        false,
    );
    let report = inspect(&root.path().join("link"));
    assert_eq!(report.failure, Some(Failure::NonUtf8));
    assert!(!report.complete);
    assert!(report.nodes.last().unwrap().target.is_none());
    serde_json::to_string(&report).unwrap();
}

#[cfg(unix)]
#[test]
fn test_directory_permission_denial_is_not_missing() {
    use std::os::unix::fs::PermissionsExt;
    assert!(
        unsafe { nix::libc::geteuid() } != 0,
        "permission tests must run as a non-root user"
    );
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("private");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
    let report = inspect(&directory.join("child"));
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(report.failure, Some(Failure::AccessDenied));
}
