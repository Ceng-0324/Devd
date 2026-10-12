use super::*;
use crate::config::ConfigLoader;
use std::fs;

fn config(yaml: &str, root: &Path) -> DevdConfig {
    let mut config = ConfigLoader::from_str(
        &format!("version: '1'\nservices:\n{yaml}"),
        root.join("devd.yml"),
    )
    .unwrap();
    for service in config.services.values_mut() {
        service.cwd = Some(root.into());
    }
    config
}

#[test]
fn test_diamond_impact_is_deduplicated_with_explainable_shortest_path() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("data"), "not read").unwrap();
    let config = config("  db: {command: db, requires: [{type: file, path: data}]}\n  api: {command: api, depends-on: [db]}\n  worker: {command: worker, depends-on: [db]}\n  web: {command: web, depends-on: [api, worker]}\n", root.path());
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "db".into(),
            field: Field::Requires { index: 0 },
        },
    )
    .unwrap();
    assert!(target.declared_scope.is_none());
    let view = analyze(&config, root.path(), &target).unwrap();
    assert_eq!(view.matches.len(), 1);
    assert_eq!(
        view.direct_consumers
            .iter()
            .map(|c| c.service.as_str())
            .collect::<Vec<_>>(),
        ["db"]
    );
    assert_eq!(
        view.downstream
            .iter()
            .map(|d| d.service.as_str())
            .collect::<Vec<_>>(),
        ["api", "web", "worker"]
    );
    assert_eq!(
        view.downstream
            .iter()
            .find(|d| d.service == "web")
            .unwrap()
            .via_services,
        ["db", "api", "web"]
    );
    assert!(view.complete);
}

#[test]
fn test_binding_direct_recipients_and_required_alias_keep_distinct_declaration_scope() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("real"), "").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("real", root.path().join("alias")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file("real", root.path().join("alias"))
        .expect("Windows symlink privileges are required");
    let config = config("  db: {command: db, paths: {DATA: {scope: shared, path: alias}}}\n  api: {command: api, depends-on: [db], requires: [{type: file, path: real}]}\n  web: {command: web, depends-on: [api]}\n", root.path());
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "db".into(),
            field: Field::Paths {
                name: "DATA".into(),
            },
        },
    )
    .unwrap();
    assert_eq!(target.declared_scope, Some(PathScope::Shared));
    let view = analyze(&config, root.path(), &target).unwrap();
    assert_eq!(view.matches.len(), 2);
    assert!(matches!(
        view.matches[0].basis,
        MatchBasis::SameObservedTarget
    ));
    assert!(view.matches[0].declared_scope.is_none());
    assert!(matches!(
        view.matches[1].basis,
        MatchBasis::SameRequestedPath
    ));
    assert_eq!(
        view.direct_consumers
            .iter()
            .map(|c| c.service.as_str())
            .collect::<Vec<_>>(),
        ["api", "db"]
    );
    let api = view
        .direct_consumers
        .iter()
        .find(|c| c.service == "api")
        .unwrap();
    assert!(api
        .reasons
        .iter()
        .any(|r| r.relation == ConsumerRelation::Requires));
    assert!(api
        .reasons
        .iter()
        .any(|r| r.relation == ConsumerRelation::BindingRecipient));
    assert_eq!(view.downstream[0].via_services, ["api", "web"]);
}

#[test]
fn test_missing_and_repeated_aliases_are_bounded_without_inventing_ownership() {
    let root = tempfile::tempdir().unwrap();
    let config = config("  a: {command: a, requires: [{type: file, path: missing}]}\n  b: {command: b, requires: [{type: file, path: missing}]}\n  c: {command: c, requires: [{type: file, path: other}]}\n", root.path());
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "a".into(),
            field: Field::Requires { index: 0 },
        },
    )
    .unwrap();
    let view = analyze(&config, root.path(), &target).unwrap();
    assert_eq!(view.matches.len(), 2);
    assert_eq!(view.uncertain_aliases, 1);
    assert!(!view.complete);
    assert!(view.matches.iter().all(|m| m.declared_scope.is_none()));
}

#[test]
fn test_cycle_does_not_repeat_downstream_or_hang() {
    let root = tempfile::tempdir().unwrap();
    let config = config("  a: {command: a, requires: [{type: file, path: data}], depends-on: [b]}\n  b: {command: b, depends-on: [c]}\n  c: {command: c, depends-on: [a]}\n", root.path());
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "a".into(),
            field: Field::Requires { index: 0 },
        },
    )
    .unwrap();
    let view = analyze(&config, root.path(), &target).unwrap();
    assert_eq!(view.downstream.len(), 2);
    assert_eq!(view.downstream[0].service, "b");
    assert_eq!(view.downstream[1].service, "c");
}

#[test]
fn test_graph_and_declaration_caps_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(
        "  a: {command: a, requires: [{type: file, path: data}]}\n",
        root.path(),
    );
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "a".into(),
            field: Field::Requires { index: 0 },
        },
    )
    .unwrap();
    let example = config.services["a"].clone();
    for n in 0..MAX_SERVICES {
        config
            .services
            .insert(format!("other-{n}"), example.clone());
    }
    assert!(analyze(&config, root.path(), &target)
        .unwrap_err()
        .to_string()
        .contains("impact graph exceeds"));
    config.services.retain(|name, _| name == "a");
    let requirement = config.services["a"].requires[0].clone();
    config.services.get_mut("a").unwrap().requires = vec![requirement; MAX_DECLARATIONS + 2];
    let view = analyze(&config, root.path(), &target).unwrap();
    assert_eq!(view.omitted_declarations, 2);
    assert_eq!(view.omitted_matches, MAX_DECLARATIONS - MAX_MATCHES);
    assert!(!view.complete);
}

#[test]
fn test_temporary_hint_never_changes_scope() {
    let root = tempfile::tempdir().unwrap();
    let config = config(
        "  a: {command: a, requires: [{type: file, path: data}]}\n",
        root.path(),
    );
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "a".into(),
            field: Field::Requires { index: 0 },
        },
    )
    .unwrap();
    // Temp directories on supported platforms are common test roots.
    if root.path().starts_with(std::env::temp_dir())
        || root.path().starts_with(Path::new("/tmp"))
        || root.path().starts_with(Path::new("/private/tmp"))
    {
        assert_eq!(
            target.risk_hints,
            ["temporary-location-is-not-deletion-authorization"]
        );
    }
    assert!(target.declared_scope.is_none());
}

#[test]
fn test_conflicting_observations_are_not_reported_as_complete() {
    let root = tempfile::tempdir().unwrap();
    let config = config(
        "  a: {command: a, requires: [{type: file, path: data}]}\n",
        root.path(),
    );
    let target = select(
        &config,
        root.path(),
        Selector {
            service: "a".into(),
            field: Field::Requires { index: 0 },
        },
    )
    .unwrap();
    fs::write(root.path().join("data"), "").unwrap();
    let view = analyze(&config, root.path(), &target).unwrap();
    assert_eq!(view.matches.len(), 1);
    assert!(view.matches[0].conflicts_with_selection);
    assert!(!view.complete);
}
