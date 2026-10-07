#![cfg(windows)]

use devd::{
    config::{HealthCheck, ServiceConfig},
    core::{health_check::HealthChecker, process_manager::ManagedProcess},
};
use std::{
    ffi::OsStr,
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT},
    System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
};

fn alive(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_PARAMETER as i32)
        );
        return false;
    }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    result == WAIT_TIMEOUT
}

fn fixture_command() -> String {
    format!(
        "{} --ignored --exact test_windows_fixture --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    )
}

fn config(root: &Path, role: &str) -> ServiceConfig {
    let mut config: ServiceConfig = serde_yaml::from_str("command: placeholder").unwrap();
    config.command = fixture_command();
    config.cwd = Some(root.into());
    config.env.insert("DEVD_TEST_ROLE".into(), role.into());
    config
        .env
        .insert("DEVD_TEST_ROOT".into(), root.to_string_lossy().into_owned());
    config
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Windows fixture timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn descendants(root: &Path) -> Vec<u32> {
    fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            if path.extension() == Some(OsStr::new("pid")) {
                fs::read_to_string(path).ok()?.parse().ok()
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn test_windows_job_cleans_descendants_on_stop_restart_and_drop() {
    let root = tempfile::tempdir().unwrap();
    let mut process = ManagedProcess::spawn("tree", &config(root.path(), "tree"))
        .await
        .unwrap();
    let first = process.pid().unwrap();
    wait_until(|| !descendants(root.path()).is_empty()).await;
    let second = process.restart(Duration::ZERO).await.unwrap();
    assert_ne!(first, second);
    assert!(!alive(first));
    wait_until(|| descendants(root.path()).len() == 2).await;
    process.stop(Duration::ZERO).await.unwrap();
    assert!(!alive(second));
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
    process.restart(Duration::ZERO).await.unwrap();
    wait_until(|| descendants(root.path()).len() == 3).await;
    drop(process);
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
}

#[tokio::test]
async fn test_windows_script_cleanup_on_completion_timeout_and_cancellation() {
    for role in ["probe", "tree"] {
        let root = tempfile::tempdir().unwrap();
        let service = config(root.path(), role);
        let health: HealthCheck = serde_yaml::from_value(
            serde_yaml::to_value(serde_json::json!({
                "type": "script", "command": fixture_command(), "timeout": "3s"
            }))
            .unwrap(),
        )
        .unwrap();
        let checker = HealthChecker::for_service(&health, &service).unwrap();
        let result = checker.probe().await;
        assert_eq!(result.is_healthy(), role == "probe");
        assert!(!descendants(root.path()).is_empty());
        for pid in descendants(root.path()) {
            wait_until(|| !alive(pid)).await;
        }
    }
    let root = tempfile::tempdir().unwrap();
    let service = config(root.path(), "tree");
    let health: HealthCheck = serde_yaml::from_value(
        serde_yaml::to_value(serde_json::json!({
            "type": "script", "command": fixture_command(), "timeout": "30s"
        }))
        .unwrap(),
    )
    .unwrap();
    let checker = HealthChecker::for_service(&health, &service).unwrap();
    let task = tokio::spawn(async move { checker.probe().await });
    wait_until(|| !descendants(root.path()).is_empty()).await;
    task.abort();
    let _ = task.await;
    for pid in descendants(root.path()) {
        wait_until(|| !alive(pid)).await;
    }
}

struct Supervisor(Child);
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    use std::io::{Read, Seek};
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args(args)
            .args(["--color", "never"])
            .current_dir(root)
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI timed out: {args:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    stdout.rewind().unwrap();
    stderr.rewind().unwrap();
    let mut output = std::process::Output {
        status,
        stdout: vec![],
        stderr: vec![],
    };
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    output
}

#[test]
fn test_windows_init_and_stalled_terminal_do_not_block_shutdown() {
    let root = tempfile::tempdir().unwrap();
    assert!(cli(root.path(), &["init"]).status.success());
    assert!(!cli(root.path(), &["init"]).status.success());
    assert!(fs::read_to_string(root.path().join("devd.yml"))
        .unwrap()
        .contains("powershell.exe"));
    assert!(cli(root.path(), &["check"]).status.success());
    let service = config(root.path(), "flood");
    fs::write(root.path().join("devd.yml"), serde_yaml::to_string(&serde_json::json!({
        "version": "1", "services": {"worker": { "command": service.command, "env": service.env }}
    })).unwrap()).unwrap();
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !root.path().join("flood-ready").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    // Never drain foreground stdout. The service has already produced more
    // than a Windows pipe can hold, while status/stop must remain responsive.
    assert!(cli(root.path(), &["status"]).status.success());
    assert!(cli(root.path(), &["stop"]).status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = supervisor.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shutdown blocked on foreground stdout"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn test_windows_cli_control_persistence_and_supervisor_death_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let service = config(root.path(), "tree");
    fs::write(
        root.path().join("devd.yml"),
        serde_yaml::to_string(&serde_json::json!({
            "version": "1", "services": {"worker": {
                "command": service.command, "cwd": service.cwd, "env": service.env
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let output = cli(root.path(), &["check"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .args([
                "start",
                "--persist-logs",
                "--persist-events",
                "--color",
                "never",
            ])
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = cli(root.path(), &["status", "--json"]);
        if output.status.success() && !descendants(root.path()).is_empty() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!cli(root.path(), &["start"]).status.success());
    let output = cli(root.path(), &["events", "worker", "--json"]);
    assert!(output.status.success());
    let events: devd::core::events::query::EventBatch =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        events.persistence,
        Some(devd::core::events::query::PersistenceState::Recording)
    );
    assert!(!events.entries.is_empty());
    assert!(cli(root.path(), &["restart", "worker"]).status.success());
    assert!(cli(root.path(), &["snapshot", "save", "before-stop"])
        .status
        .success());
    fs::remove_file(root.path().join("devd.yml")).unwrap();
    assert!(cli(root.path(), &["stop"]).status.success());
    let stop_deadline = Instant::now() + Duration::from_secs(15);
    while supervisor.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < stop_deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cli(root.path(), &["logs", "--stored"]).status.success());
    let output = cli(root.path(), &["events", "--stored", "--json"]);
    assert!(output.status.success());
    let events: devd::core::events::query::EventBatch =
        serde_json::from_slice(&output.stdout).unwrap();
    assert!(events.gaps.is_empty());
    assert!(events.entries.iter().any(|event| matches!(
        event.data,
        devd::core::events::EventData::SupervisorStopped { .. }
    )));
    assert!(cli(
        root.path(),
        &[
            "snapshot",
            "restore",
            "before-stop",
            "--output",
            "restored.yml"
        ]
    )
    .status
    .success());
    assert!(!cli(
        root.path(),
        &[
            "snapshot",
            "restore",
            "before-stop",
            "--output",
            "restored.yml"
        ]
    )
    .status
    .success());
    for pid in descendants(root.path()) {
        assert!(!alive(pid));
    }
    fs::rename(
        root.path().join("restored.yml"),
        root.path().join("devd.yml"),
    )
    .unwrap();
    let count = descendants(root.path()).len();
    supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_devd"))
            .arg("start")
            .current_dir(root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while descendants(root.path()).len() == count {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    supervisor.0.kill().unwrap();
    supervisor.0.wait().unwrap();
    let cleanup_deadline = Instant::now() + Duration::from_secs(15);
    for pid in descendants(root.path()) {
        while alive(pid) {
            assert!(Instant::now() < cleanup_deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

// Invoked as a real child process by the tests above. The Windows CI runs the
// parent tests; this ignored entry is only the deterministic workload helper.
#[test]
#[ignore]
#[allow(clippy::zombie_processes)] // Deliberately orphan a descendant to test Job cleanup.
fn test_windows_fixture() {
    let role = std::env::var("DEVD_TEST_ROLE").unwrap();
    if role == "flood" {
        // The collector uses a broadcast channel, independent of terminal
        // writes. Reaching this marker proves it drained beyond pipe capacity.
        // If it blocks, fail the test instead of issuing stop before the flood.
        for _ in 0..4096 {
            println!("{}", "x".repeat(1024));
        }
        fs::write(
            Path::new(&std::env::var("DEVD_TEST_ROOT").unwrap()).join("flood-ready"),
            "ready",
        )
        .unwrap();
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if role != "leaf" {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "test_windows_fixture",
                "--nocapture",
            ])
            .env("DEVD_TEST_ROLE", "leaf")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let root = std::env::var("DEVD_TEST_ROOT").unwrap();
        fs::write(
            Path::new(&root).join(format!("{}.pid", std::process::id())),
            child.id().to_string(),
        )
        .unwrap();
        println!("fixture spawned descendant {}", child.id());
        if role == "probe" {
            return;
        }
    }
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}
