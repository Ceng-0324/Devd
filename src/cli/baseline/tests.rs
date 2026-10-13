use super::*;
#[test]
fn test_local_lineage_is_not_recreated_by_check_and_replacement_is_unknown() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let c = Control::new();
    assert!(model::local_scope(&state, false, &c).unwrap().is_none());
    assert!(!state.exists());
    let a = model::local_scope(&state, true, &c).unwrap().unwrap();
    assert!(model::local_scope(&state, false, &c).unwrap().as_ref() == Some(&a));
    let marker = state.join("baseline-origin.id");
    let text = std::fs::read(&marker).unwrap();
    std::fs::rename(&marker, state.join("old-origin.id")).unwrap();
    std::fs::write(&marker, text).unwrap();
    assert!(model::local_scope(&state, false, &c).unwrap().as_ref() != Some(&a));
}
#[test]
fn test_expired_capture_cannot_publish_or_create_origin() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("baseline.json");
    let args = Args {
        action: Action::Capture {
            service: "api".into(),
            resource: vec![Selection::Cwd],
            output: output.clone(),
        },
    };
    let state = root.path().join("state");
    assert!(prepare(
        args,
        &root.path().join("absent.yml"),
        &state,
        None,
        &Control::expired()
    )
    .is_err());
    assert!(!output.exists());
    assert!(!state.exists());
}
#[test]
fn test_selection_is_strict_and_program_does_not_infer_shell_or_path() {
    for s in ["requires:-1", "requires:", "paths:", "auto", "program:sh"] {
        assert!(s.parse::<Selection>().is_err());
    }
    let mut config = ConfigLoader::from_str(
        "version: '1'\nservices:\n  api: {command: 'sh -c whatever'}",
        "devd.yml",
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    super::super::resolve_working_directories(&mut config, &root.path().join("devd.yml"));
    assert!(resources::resolve(&config, root.path(), "api", &Selection::Program).is_err());
    config.services.get_mut("api").unwrap().command = "./sh -c whatever".into();
    assert!(resources::resolve(&config, root.path(), "api", &Selection::Program).is_ok());
}
