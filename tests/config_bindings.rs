use devd::config::{ConfigLoader, PathScope};

fn validate(services: &str) -> Result<(), String> {
    let yaml = format!("version: '1'\nservices:\n{services}");
    let config = ConfigLoader::from_str(&yaml, "devd.yml").map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())
}

#[test]
fn test_port_bindings_reject_conflicts_with_other_ports_and_listen() {
    let conflict = "  api: {command: api, ports: {API_ADDR: '127.0.0.1:3000'}}\n  web: {command: web, listen: ['0.0.0.0:3000']}\n";
    assert!(validate(conflict)
        .unwrap_err()
        .contains("TCP address conflicts"));
    let same_service =
        "  api: {command: api, ports: {API_ADDR: '127.0.0.1:3000'}, listen: ['127.0.0.1:3000']}\n";
    validate(same_service).unwrap();
    let distinct = "  api: {command: api, ports: {API_ADDR: '127.0.0.1:3000'}}\n  web: {command: web, ports: {WEB_ADDR: '127.0.0.2:3000'}}\n";
    validate(distinct).unwrap();
    assert!(validate(&conflict.replace("'127.0.0.1:3000'", "'[::ffff:127.0.0.1]:3000'")).is_err());
    for address in [
        "0.0.0.0:3000",
        "[::]:3000",
        "224.0.0.1:3000",
        "255.255.255.255:3000",
        "[::ffff:0.0.0.0]:3000",
    ] {
        assert!(validate(&format!(
            "  api: {{command: api, ports: {{API_ADDR: '{address}'}}}}\n"
        ))
        .unwrap_err()
        .contains("concrete unicast"));
    }
    assert!(
        validate("  api: {command: api, ports: {API_ADDR: '127.0.0.1:0'}}\n")
            .unwrap_err()
            .contains("port must be between")
    );
}

#[test]
fn test_path_bindings_validate_ownership_and_environment_names() {
    for path in [
        "/tmp/data",
        "../data",
        "data/../other",
        ".",
        "con",
        "com1.txt",
        "data.",
        "Data",
        "data:stream",
    ] {
        let yaml = format!(
            "  api: {{command: api, paths: {{DATA: {{scope: instance, path: '{path}'}}}}}}\n"
        );
        assert!(validate(&yaml)
            .unwrap_err()
            .contains("instance path must be relative"));
    }
    let overlap = "  api: {command: api, paths: {DATA: {scope: instance, path: data}}}\n  worker: {command: worker, paths: {WORKER_DATA: {scope: instance, path: data/worker}}}\n";
    assert!(validate(overlap)
        .unwrap_err()
        .contains("overlaps another owned path"));
    let shared = "  api: {command: api, paths: {DATA: {scope: shared, path: data}}}\n  worker: {command: worker, paths: {WORKER_DATA: {scope: shared, path: data}}}\n";
    validate(shared).unwrap();
    let name = "  api: {command: api, ports: {bad-key: '127.0.0.1:3000'}}\n";
    assert!(validate(name).unwrap_err().contains("uppercase ASCII"));
}

#[test]
fn test_direct_dependency_bindings_reject_ambiguous_environment() {
    let duplicated = "  a: {command: a, ports: {ADDR: '127.0.0.1:3000'}}\n  b: {command: b, ports: {ADDR: '127.0.0.1:3001'}}\n  c: {command: c, depends-on: [a, b]}\n";
    assert!(validate(duplicated)
        .unwrap_err()
        .contains("binding environment name conflicts"));
    let explicit = "  a: {command: a, ports: {ADDR: '127.0.0.1:3000'}}\n  b: {command: b, env: {ADDR: wrong}, depends-on: [a]}\n";
    assert!(validate(explicit)
        .unwrap_err()
        .contains("binding environment name conflicts"));
    assert!(validate(&explicit.replace("env: {ADDR", "env: {addr")).is_err());
    let unrelated = "  a: {command: a, ports: {ADDR: '127.0.0.1:3000'}}\n  b: {command: b, ports: {ADDR: '127.0.0.1:3001'}}\n";
    validate(unrelated).unwrap();
}

#[test]
fn test_profile_replaces_binding_maps_and_keeps_path_scope() {
    let yaml = "version: '1'\nservices:\n  api:\n    command: api\n    ports: {API_ADDR: '127.0.0.1:3000', ADMIN_ADDR: '127.0.0.1:3002'}\n    paths: {API_DATA: {scope: shared, path: data}, CACHE: {scope: shared, path: cache}}\nprofiles:\n  dev:\n    services:\n      api:\n        ports: {API_ADDR: '127.0.0.1:4000'}\n        paths: {API_DATA: {scope: instance, path: api}}\n  empty:\n    services:\n      api: {ports: {}, paths: {}}\n";
    let base = ConfigLoader::from_str(yaml, "devd.yml").unwrap();
    let dev = ConfigLoader::from_str_profile(yaml, "devd.yml", Some("dev")).unwrap();
    base.validate().unwrap();
    dev.validate().unwrap();
    assert_eq!(base.services["api"].ports["API_ADDR"].port(), 3000);
    assert_eq!(dev.services["api"].ports["API_ADDR"].port(), 4000);
    assert_eq!(
        dev.services["api"].paths["API_DATA"].scope,
        PathScope::Instance
    );
    assert_eq!(dev.services["api"].paths.len(), 1);
    assert_eq!(dev.services["api"].ports.len(), 1);
    let empty = ConfigLoader::from_str_profile(yaml, "devd.yml", Some("empty")).unwrap();
    assert!(empty.services["api"].ports.is_empty());
    assert!(empty.services["api"].paths.is_empty());
}

#[tokio::test]
async fn test_application_bind_failure_keeps_logs_and_exit_evidence() {
    use devd::core::service_manager::{
        ManagerOptions, ServiceManager, ServiceManagerError, ServiceState,
    };
    let root = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let command = shell_words::join([
        std::env::current_exe().unwrap().to_str().unwrap(),
        "--ignored",
        "--exact",
        "test_binding_fixture",
        "--nocapture",
    ]);
    let yaml = serde_yaml::to_string(&serde_json::json!({
        "version": "1",
        "services": {"api": {"command": command,
            "ports": {"API_ADDR": address.to_string()}, "restart": {"policy": "never"}}}
    }))
    .unwrap();
    let config = ConfigLoader::from_str(&yaml, "devd.yml").unwrap();
    let manager = ServiceManager::new(
        config,
        ManagerOptions::new(root.path().join("state/services.json")),
    )
    .unwrap();
    let state = manager.subscribe();
    let logs = manager.log_history();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        manager.run_until(std::future::pending()),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(ServiceManagerError::FailedServices { .. })
    ));
    assert_eq!(state.borrow().services["api"].status, ServiceState::Failed);
    assert_eq!(state.borrow().services["api"].last_exit_code, Some(17));
    assert!(logs
        .recent(Some("api"), 50)
        .iter()
        .any(|entry| entry.message.contains("application-bind-failed")));
    assert!(listener.local_addr().is_ok());
}

// Real child workload: a held test listener makes the application's bind fail.
#[test]
#[ignore]
fn test_binding_fixture() {
    let address = std::env::var("API_ADDR").unwrap();
    match std::net::TcpListener::bind(&address) {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            eprintln!("application-bind-failed: {address}: {error}");
            std::process::exit(17);
        }
    }
}
