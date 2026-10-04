#![cfg(unix)]

#[path = "support/http.rs"]
mod http;
mod support;

use devd::core::service_manager::{RuntimeSnapshot, ServiceState};
use http::HttpMock;
use nix::{
    errno::Errno,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use support::{failure, success, wait, Project};

fn fixture(name: &str, configure: impl FnOnce(&mut serde_yaml::Value)) -> Project {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut config: serde_yaml::Value =
        serde_yaml::from_slice(&fs::read(root.join(name)).unwrap()).unwrap();
    configure(&mut config);
    let project = Project::new("services: {}\n");
    fs::write(
        project.path().join("devd.yml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    fs::copy(root.join("workload.sh"), project.path().join("workload.sh")).unwrap();
    project
}

fn chain(mock: &HttpMock) -> Project {
    fixture("dependency-chain.yml", |config| {
        for name in ["database", "api"] {
            config["services"][name]["healthcheck"]["url"] = format!("{}/{name}", mock.url).into();
        }
    })
}

fn until(
    project: &Project,
    description: &str,
    predicate: impl Fn(&RuntimeSnapshot) -> bool,
) -> RuntimeSnapshot {
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut last = None;
    loop {
        if let Some(snapshot) = project.snapshot() {
            if predicate(&snapshot) {
                return snapshot;
            }
            last = Some(snapshot);
        }
        assert!(
            Instant::now() < deadline,
            "waiting for {description}; last status: {last:#?}\nstdout: {}\nstderr: {}",
            fs::read_to_string(project.path().join("stdout")).unwrap_or_default(),
            fs::read_to_string(project.path().join("stderr")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(15));
    }
}

fn events(project: &Project) -> Vec<String> {
    fs::read_to_string(project.path().join("events"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn pids(project: &Project) -> Vec<u32> {
    let mut result = Vec::new();
    for entry in fs::read_dir(project.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "pid") {
            result.push(
                path.file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .rsplit('.')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
            result.push(fs::read_to_string(path).unwrap().trim().parse().unwrap());
        }
    }
    result
}

fn assert_gone(pids: &[u32]) {
    wait(|| {
        pids.iter()
            .all(|pid| match kill(Pid::from_raw(*pid as i32), None) {
                Err(Errno::ESRCH) => true,
                Ok(()) => false,
                error => panic!("cannot inspect fixture PID {pid}: {error:?}"),
            })
            .then_some(())
    });
}

fn final_snapshot(project: &Project) -> RuntimeSnapshot {
    serde_json::from_slice(&fs::read(project.path().join(".devd/devd.yml/services.json")).unwrap())
        .unwrap()
}

#[test]
fn test_mvp_single_service_crash_recovery_logs_and_cleanup() {
    let project = fixture("simple.yml", |_| {});
    success(project.invoke(&["check"]));
    assert!(success(project.invoke(&["graph"])).contains("1: worker"));
    let mut supervisor = project.start();
    let first = project.running().services["worker"].pid.unwrap();
    wait(|| (pids(&project).len() == 2).then_some(()));
    let first_generation = pids(&project);
    wait(|| {
        success(project.invoke(&["logs", "worker"]))
            .contains("stderr-ready")
            .then_some(())
    });
    fs::write(project.path().join("crash-worker"), "").unwrap();
    let recovered = until(&project, "automatic restart", |s| {
        s.services["worker"].pid.is_some() && s.services["worker"].restart_count == 1
    });
    assert_ne!(recovered.services["worker"].pid, Some(first));
    assert_eq!(recovered.services["worker"].last_exit_code, Some(23));
    assert_gone(&first_generation);
    let logs = success(project.invoke(&["logs", "worker"]));
    assert!(
        logs.contains("[ERROR] worker injected crash, exit 23"),
        "{logs}"
    );
    assert!(logs.contains("[INFO] worker stdout-ready"), "{logs}");
    wait(|| (pids(&project).len() == 4).then_some(()));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    assert_gone(&pids(&project));
    let saved = final_snapshot(&project);
    assert_eq!(saved.services["worker"].status, ServiceState::Stopped);
    assert_eq!(saved.services["worker"].pid, None);
    assert!(fs::read_to_string(project.path().join("stdout"))
        .unwrap()
        .contains("worker stopped"));
    failure(project.invoke(&["status"]), "no reachable devd supervisor");
}

#[test]
fn test_mvp_dependency_readiness_health_recovery_and_reverse_shutdown() {
    let mock = HttpMock::start();
    let project = chain(&mock);
    success(project.invoke(&["check"]));
    let graph = success(project.invoke(&["graph"]));
    assert!(
        graph.contains("1: database\n  2: api\n  3: frontend"),
        "{graph}"
    );
    let mut supervisor = project.start();
    until(&project, "database unhealthy", |s| {
        s.services["database"].status == ServiceState::Unhealthy
    });
    let requests = mock.database.requests();
    wait(|| (mock.database.requests() >= requests + 3).then_some(()));
    let waiting = project.snapshot().unwrap();
    for name in ["api", "frontend"] {
        assert_eq!(waiting.services[name].status, ServiceState::Pending);
        assert_eq!(waiting.services[name].pid, None);
        assert!(!events(&project)
            .iter()
            .any(|line| line.starts_with(&format!("start {name} "))));
    }
    wait(|| (events(&project).len() == 1).then_some(()));
    mock.database.set(200);
    let api = until(&project, "api unhealthy", |s| {
        s.services["api"].status == ServiceState::Unhealthy
    });
    assert_eq!(api.services["frontend"].status, ServiceState::Pending);
    assert!(api.services["api"]
        .last_error
        .as_ref()
        .unwrap()
        .contains("503"));
    let requests = mock.api.requests();
    wait(|| (mock.api.requests() >= requests + 3).then_some(()));
    assert!(!events(&project)
        .iter()
        .any(|line| line.starts_with("start frontend ")));
    wait(|| (events(&project).len() == 2).then_some(()));
    mock.api.set(200);
    let healthy = until(&project, "whole chain running", |s| {
        s.services["database"].status == ServiceState::Healthy
            && s.services["api"].status == ServiceState::Healthy
            && s.services["frontend"].pid.is_some()
    });
    wait(|| (pids(&project).len() == 6).then_some(()));
    wait(|| (events(&project).len() == 3).then_some(()));
    let started: Vec<_> = events(&project)
        .iter()
        .map(|line| line.split_whitespace().nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(started, ["database", "api", "frontend"]);
    mock.api.set(503);
    let failed = until(&project, "observable health failure", |s| {
        s.services["api"].status == ServiceState::Unhealthy
    });
    assert_eq!(failed.services["api"].pid, healthy.services["api"].pid);
    assert!(success(project.invoke(&["status"])).contains("503"));
    assert!(success(project.invoke(&["logs", "api"])).contains("api stderr-ready"));
    mock.api.set(200);
    let recovered = until(&project, "health recovery", |s| {
        s.services["api"].status == ServiceState::Healthy
    });
    assert_eq!(recovered.services["api"].restart_count, 0);
    assert!(recovered.services["api"].last_error.is_none());
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    assert_gone(&pids(&project));
    let stopped: Vec<_> = events(&project)
        .iter()
        .filter(|line| line.starts_with("stop "))
        .map(|line| line.split_whitespace().nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(stopped, ["frontend", "api", "database"]);
    assert!(final_snapshot(&project)
        .services
        .values()
        .all(|s| s.pid.is_none() && s.status == ServiceState::Stopped));
}

#[test]
fn test_mvp_startup_failure_rolls_back_running_dependencies() {
    let mock = HttpMock::start();
    let project = chain(&mock);
    let mut config: serde_yaml::Value =
        serde_yaml::from_slice(&fs::read(project.path().join("devd.yml")).unwrap()).unwrap();
    config["services"]["api"]["command"] = "/missing/devd-fixture-program".into();
    fs::write(
        project.path().join("devd.yml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    let mut supervisor = project.start();
    until(&project, "database waiting", |s| {
        s.services["database"].status == ServiceState::Unhealthy
    });
    wait(|| (pids(&project).len() == 2).then_some(()));
    mock.database.set(200);
    supervisor.finish(false);
    let saved = final_snapshot(&project);
    assert_eq!(saved.services["api"].status, ServiceState::Failed);
    assert!(saved.services["api"]
        .last_error
        .as_ref()
        .unwrap()
        .contains("/missing/devd-fixture-program"));
    assert!(saved.services.values().all(|s| s.pid.is_none()));
    assert_gone(&pids(&project));
    assert!(!events(&project)
        .iter()
        .any(|line| line.starts_with("start frontend ")));
    assert!(fs::read_to_string(project.path().join("stderr"))
        .unwrap()
        .contains("services failed"));
}

#[test]
fn test_mvp_signal_shutdown_while_readiness_is_pending() {
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let mock = HttpMock::start();
        let project = chain(&mock);
        let mut supervisor = project.start();
        until(&project, "database waiting", |s| {
            s.services["database"].status == ServiceState::Unhealthy
        });
        wait(|| (pids(&project).len() == 2).then_some(()));
        kill(Pid::from_raw(supervisor.0.id() as i32), signal).unwrap();
        supervisor.finish(true);
        assert_gone(&pids(&project));
        assert!(final_snapshot(&project)
            .services
            .values()
            .all(|s| s.pid.is_none() && s.status == ServiceState::Stopped));
        assert_eq!(events(&project).len(), 2);
    }
}

#[test]
fn test_mvp_restart_budget_exhaustion_preserves_diagnostics_and_cleans_every_generation() {
    let project = fixture("simple.yml", |_| {});
    let mut supervisor = project.start();
    project.running();
    for generation in 0..=2 {
        until(&project, "next generation", |s| {
            s.services["worker"].pid.is_some() && s.services["worker"].restart_count == generation
        });
        wait(|| (pids(&project).len() == (generation as usize + 1) * 2).then_some(()));
        fs::write(project.path().join("crash-worker"), "").unwrap();
    }
    supervisor.finish(false);
    assert_gone(&pids(&project));
    let saved = final_snapshot(&project);
    assert_eq!(saved.services["worker"].status, ServiceState::Failed);
    assert_eq!(saved.services["worker"].restart_count, 2);
    assert_eq!(saved.services["worker"].last_exit_code, Some(23));
    assert_eq!(saved.services["worker"].pid, None);
    let output = fs::read_to_string(project.path().join("stdout")).unwrap();
    assert_eq!(output.matches("injected crash, exit 23").count(), 3);
    failure(project.invoke(&["logs"]), "no reachable devd supervisor");
}
