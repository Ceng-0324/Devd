#![cfg(unix)]

use std::{future::Future, path::Path, time::Duration};

use devd::{
    config::{HealthCheck, ServiceConfig},
    core::health_check::{
        HealthCheckError, HealthChecker, HealthMonitor, HealthState, ProbeFailure, ProbeResult,
    },
};
use nix::{
    errno::Errno,
    sys::signal::{kill, killpg},
    unistd::Pid,
};
use tempfile::tempdir;

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("script probe timed out")
}

fn checker(directory: &Path, script: &str, timeout: Duration) -> HealthChecker {
    let service: ServiceConfig = serde_yaml::from_str("command: sleep 60").unwrap();
    let service = ServiceConfig {
        cwd: Some(directory.into()),
        ..service
    };
    HealthChecker::for_service(
        &HealthCheck::Script {
            command: shell_words::join(["/bin/sh", "-c", script]),
            interval: Duration::from_millis(20),
            timeout,
            retries: 2,
        },
        &service,
    )
    .unwrap()
}

async fn pid_file(path: &Path) -> u32 {
    bounded(async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(path).await {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}

async fn reaped_group(pid: u32) {
    bounded(async {
        loop {
            let pid = Pid::from_raw(pid as i32);
            if kill(pid, None) == Err(Errno::ESRCH) && killpg(pid, None) == Err(Errno::ESRCH) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn test_script_exit_status_signal_and_spawn_errors_are_unhealthy() {
    let directory = tempdir().unwrap();
    assert!(checker(directory.path(), "exit 0", Duration::from_secs(2))
        .probe()
        .await
        .is_healthy());
    for script in ["exit 7", "kill -TERM $$"] {
        let result = checker(directory.path(), script, Duration::from_secs(2))
            .probe()
            .await;
        assert!(
            matches!(result, ProbeResult::Unhealthy(ProbeFailure::Script { .. })),
            "{result:?}"
        );
    }
    let config: HealthCheck =
        serde_yaml::from_str("type: script\ncommand: /no/such/devd-probe").unwrap();
    assert!(matches!(
        HealthChecker::new(&config).unwrap().probe().await,
        ProbeResult::Unhealthy(ProbeFailure::Script { .. })
    ));
}

#[tokio::test]
async fn test_script_inherits_service_context_and_discards_large_output() {
    let directory = tempdir().unwrap();
    let mut service: ServiceConfig =
        serde_yaml::from_str("command: sleep 60\nenv-file: .env\nenv: {OVERRIDE: explicit}")
            .unwrap();
    service.cwd = Some(directory.path().into());
    tokio::fs::write(
        directory.path().join(".env"),
        "OVERRIDE=from-file\nFILE_VALUE=first\n",
    )
    .await
    .unwrap();
    // Literal metacharacters and spaces reach the script unchanged; stdin is EOF.
    tokio::fs::write(directory.path().join("probe.sh"), "test \"$OVERRIDE\" = explicit || exit 2\ntest \"$FILE_VALUE\" = first || exit 3\ntest \"$1\" = 'literal $HOME; two words' || exit 4\nif read line; then exit 5; fi\ndd if=/dev/zero bs=1024 count=256\ndd if=/dev/zero bs=1024 count=256 >&2\n").await.unwrap();
    let config = HealthCheck::Script {
        command: shell_words::join(["/bin/sh", "probe.sh", "literal $HOME; two words"]),
        interval: Duration::from_millis(10),
        timeout: Duration::from_secs(2),
        retries: 2,
    };
    let checker = HealthChecker::for_service(&config, &service).unwrap();
    assert!(bounded(checker.probe()).await.is_healthy());
    tokio::fs::write(directory.path().join(".env"), "FILE_VALUE=changed\n")
        .await
        .unwrap();
    assert!(!bounded(checker.probe()).await.is_healthy());
    tokio::fs::remove_file(directory.path().join(".env"))
        .await
        .unwrap();
    assert!(matches!(
        bounded(checker.probe()).await,
        ProbeResult::Unhealthy(ProbeFailure::Script { .. })
    ));
}

#[tokio::test]
async fn test_script_timeout_and_cancellation_kill_probe_groups() {
    let directory = tempdir().unwrap();
    let script = "echo $$ > leader; sleep 60 & echo $! > child; wait";
    let timed = checker(directory.path(), script, Duration::from_secs(1));
    assert!(matches!(
        bounded(timed.probe()).await,
        ProbeResult::Unhealthy(ProbeFailure::Timeout { .. })
    ));
    reaped_group(pid_file(&directory.path().join("leader")).await).await;
    let cancelled = checker(directory.path(), script, Duration::from_secs(60));
    tokio::fs::remove_file(directory.path().join("leader"))
        .await
        .unwrap();
    tokio::fs::remove_file(directory.path().join("child"))
        .await
        .unwrap();
    let pid;
    {
        let leader_path = directory.path().join("leader");
        let child_path = directory.path().join("child");
        let probe = cancelled.probe();
        tokio::pin!(probe);
        tokio::select! {
            result = &mut probe => panic!("probe ended before cancellation: {result:?}"),
            leader = pid_file(&leader_path) => pid = leader,
        }
        tokio::select! {
            result = &mut probe => panic!("probe ended before child started: {result:?}"),
            _ = pid_file(&child_path) => {},
        }
    }
    reaped_group(pid).await;
}

#[tokio::test]
async fn test_script_success_cleans_background_children_and_readiness_timeout_cancels() {
    let directory = tempdir().unwrap();
    let success = checker(
        directory.path(),
        "echo $$ > leader; sleep 60 & exit 0",
        Duration::from_secs(2),
    );
    assert!(bounded(success.probe()).await.is_healthy());
    reaped_group(pid_file(&directory.path().join("leader")).await).await;
    tokio::fs::remove_file(directory.path().join("leader"))
        .await
        .unwrap();
    let pending = checker(
        directory.path(),
        "echo $$ > leader; sleep 60 & wait",
        Duration::from_secs(60),
    );
    assert!(matches!(
        bounded(pending.wait_ready(Duration::from_secs(1))).await,
        Err(HealthCheckError::ReadinessTimeout { .. })
    ));
    reaped_group(pid_file(&directory.path().join("leader")).await).await;
}

#[tokio::test]
async fn test_script_monitor_counts_failures_and_recovers_without_overlap() {
    let directory = tempdir().unwrap();
    let checker = checker(
        directory.path(),
        "mkdir probe-lock || exit 99; sleep 0.04; rmdir probe-lock; test -e ready",
        Duration::from_secs(2),
    );
    let mut monitor = HealthMonitor::new(checker);
    let first = bounded(monitor.next_check()).await;
    assert_eq!(first.state, HealthState::Retrying);
    assert_eq!(
        bounded(monitor.next_check()).await.state,
        HealthState::Unhealthy
    );
    tokio::fs::write(directory.path().join("ready"), "")
        .await
        .unwrap();
    let recovered = bounded(monitor.next_check()).await;
    assert_eq!(recovered.state, HealthState::Healthy);
    assert_eq!(recovered.consecutive_failures, 0);
    assert!(!directory.path().join("probe-lock").exists());
}

#[tokio::test]
async fn test_script_pending_probe_is_cancelled_when_supervisor_stops() {
    use devd::core::service_manager::{ManagerOptions, ServiceManager};
    let directory = tempdir().unwrap();
    let mut config = devd::config::ConfigLoader::from_str("version: '1'\nservices:\n  worker:\n    command: sleep 60\n    restart: {policy: never}\n    healthcheck:\n      type: script\n      command: sh probe.sh\n      timeout: 60s\n  web:\n    command: sleep 60\n    depends-on: [{service: worker, condition: script-ready}]\n", "test.yml").unwrap();
    config.services.get_mut("worker").unwrap().cwd = Some(directory.path().into());
    tokio::fs::write(
        directory.path().join("probe.sh"),
        "echo $$ > leader\nsleep 60 &\necho $! > child\nwait\n",
    )
    .await
    .unwrap();
    let manager = ServiceManager::new(
        config,
        ManagerOptions::new(directory.path().join("services.json")),
    )
    .unwrap();
    let snapshots = manager.subscribe();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let run = manager.run_until(async {
        let _ = stopped.await;
    });
    let control = async {
        let pid = pid_file(&directory.path().join("leader")).await;
        pid_file(&directory.path().join("child")).await;
        assert!(snapshots.borrow().services["web"].pid.is_none());
        stop.send(()).unwrap();
        pid
    };
    let (result, pid) = bounded(async { tokio::join!(run, control) }).await;
    assert!(result
        .unwrap()
        .services
        .values()
        .all(|service| service.pid.is_none()));
    reaped_group(pid).await;
}
