#![cfg(unix)]

use std::{future::Future, os::unix::process::ExitStatusExt, path::Path, time::Duration};

use devd::{
    config::{ConfigLoader, ServiceConfig},
    core::process_manager::{ManagedProcess, ProcessError, ProcessState},
};
use nix::{
    errno::Errno,
    sys::signal::kill,
    unistd::{getpgid, Pid},
};
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::ChildStdout,
};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("process operation timed out")
}

fn config(command: impl Into<String>) -> ServiceConfig {
    let mut config = ConfigLoader::from_str(
        "version: '1'\nservices:\n  test: {command: test}\n",
        "test.yml",
    )
    .unwrap()
    .services
    .remove("test")
    .unwrap();
    config.command = command.into();
    config
}

fn fixture_config(mode: &str) -> ServiceConfig {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process-service.sh");
    config(shell_words::join([
        "/bin/sh",
        fixture.to_str().unwrap(),
        mode,
    ]))
}

fn shell_config(script: &str) -> ServiceConfig {
    config(shell_words::join(["/bin/sh", "-c", script]))
}

async fn line(reader: &mut BufReader<ChildStdout>) -> String {
    let mut line = String::new();
    assert!(
        bounded(reader.read_line(&mut line)).await.unwrap() > 0,
        "unexpected EOF"
    );
    line
}

async fn ready(process: &mut ManagedProcess) -> BufReader<ChildStdout> {
    let mut reader = BufReader::new(process.take_stdout().unwrap());
    assert_eq!(line(&mut reader).await, "ready\n");
    reader
}

async fn assert_reaped(pid: u32) {
    bounded(async {
        loop {
            match kill(Pid::from_raw(pid as i32), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                error => panic!("unexpected PID probe: {error:?}"),
            }
        }
    })
    .await;
}

#[tokio::test]
async fn test_process_spawn_honors_cwd_environment_file_and_explicit_overrides() {
    let directory = tempdir().unwrap();
    let cwd = tokio::fs::canonicalize(directory.path()).await.unwrap();
    tokio::fs::write(
        cwd.join(".env"),
        "DEVD_TEST_FILE_ONLY='from file'\nDEVD_TEST_OVERRIDE=from-file\n",
    )
    .await
    .unwrap();
    let original = std::env::var_os("DEVD_TEST_OVERRIDE");
    let mut config = shell_config("pwd; printf '%s\n%s\n' \"$DEVD_TEST_FILE_ONLY\" \"$DEVD_TEST_OVERRIDE\"; printf 'stderr-value\n' >&2");
    config.cwd = Some(cwd.clone());
    config.env_file = Some(".env".into());
    config
        .env
        .insert("DEVD_TEST_OVERRIDE".into(), "explicit value".into());
    let mut process = ManagedProcess::spawn("environment", &config).await.unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let mut stderr = process.take_stderr().unwrap();
    assert!(process.take_stdout().is_none());
    assert!(process.take_stderr().is_none());
    let (mut output, mut errors) = (String::new(), String::new());
    let (out, err, status) = bounded(async {
        tokio::join!(
            stdout.read_to_string(&mut output),
            stderr.read_to_string(&mut errors),
            process.wait()
        )
    })
    .await;
    out.unwrap();
    err.unwrap();
    assert!(status.unwrap().success());
    assert_eq!(
        output,
        format!("{}\nfrom file\nexplicit value\n", cwd.display())
    );
    assert_eq!(errors, "stderr-value\n");
    assert_eq!(std::env::var_os("DEVD_TEST_OVERRIDE"), original);
}

#[tokio::test]
async fn test_process_arguments_preserve_spaces_empty_values_and_shell_metacharacters() {
    let mut config = fixture_config("args");
    config.command.push(' ');
    config.command.push_str(&shell_words::join([
        "two words",
        "",
        "$HOME; echo unexpected",
        "a'b\"c",
    ]));
    let mut process = ManagedProcess::spawn("arguments", &config).await.unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let mut output = String::new();
    bounded(stdout.read_to_string(&mut output)).await.unwrap();
    assert!(bounded(process.wait()).await.unwrap().success());
    assert_eq!(
        output,
        "<two words>\n<>\n<$HOME; echo unexpected>\n<a'b\"c>\n"
    );
}

#[tokio::test]
async fn test_process_spawn_reports_missing_program_and_invalid_cwd() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing-program");
    let error = ManagedProcess::spawn(
        "missing",
        &config(shell_words::join([missing.to_str().unwrap()])),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("missing-program"));
    assert!(
        matches!(error, ProcessError::Spawn { service, source, .. } if service == "missing" && source.kind() == std::io::ErrorKind::NotFound)
    );
    let mut config = shell_config("exit 0");
    config.cwd = Some(directory.path().join("missing-directory"));
    assert!(
        matches!(ManagedProcess::spawn("cwd", &config).await, Err(ProcessError::Spawn { source, .. }) if source.kind() == std::io::ErrorKind::NotFound)
    );
}

#[tokio::test]
async fn test_process_environment_file_errors_are_actionable() {
    let directory = tempdir().unwrap();
    let path = directory.path().join(".env");
    let mut config = shell_config("exit 0");
    config.env_file = Some(path.clone());
    assert!(
        matches!(ManagedProcess::spawn("environment", &config).await, Err(ProcessError::EnvironmentRead { path: actual, .. }) if actual == path)
    );
    tokio::fs::write(&path, "this is not a dotenv assignment\n")
        .await
        .unwrap();
    assert!(
        matches!(ManagedProcess::spawn("environment", &config).await, Err(ProcessError::EnvironmentParse { path: actual, .. }) if actual == path)
    );
}

#[tokio::test]
async fn test_process_exit_status_is_cached_and_stop_is_idempotent() {
    let mut process = ManagedProcess::spawn("exit", &shell_config("exit 7"))
        .await
        .unwrap();
    let status = bounded(process.wait()).await.unwrap();
    assert_eq!(status.code(), Some(7));
    assert_eq!(process.pid(), None);
    assert_eq!(process.try_wait().unwrap(), Some(status));
    assert_eq!(process.state().unwrap(), ProcessState::Exited { status });
    assert_eq!(bounded(process.stop(Duration::ZERO)).await.unwrap(), status);
    assert_eq!(bounded(process.wait()).await.unwrap(), status);
}

#[tokio::test]
async fn test_process_graceful_stop_uses_an_isolated_process_group() {
    let mut process = ManagedProcess::spawn("graceful", &fixture_config("graceful"))
        .await
        .unwrap();
    let mut stdout = ready(&mut process).await;
    let pid = process.pid().unwrap();
    assert_eq!(process.state().unwrap(), ProcessState::Running { pid });
    assert_eq!(
        getpgid(Some(Pid::from_raw(pid as i32))).unwrap(),
        Pid::from_raw(pid as i32)
    );
    let status = bounded(process.stop(Duration::from_secs(1))).await.unwrap();
    assert!(status.success(), "unexpected stop status: {status:?}");
    let mut tail = String::new();
    bounded(stdout.read_to_string(&mut tail)).await.unwrap();
    assert_eq!(tail, "stopped\n");
    assert_reaped(pid).await;
}

#[tokio::test]
async fn test_process_stop_handles_rapidly_exiting_descendant_groups() {
    for _ in 0..32 {
        let mut process = ManagedProcess::spawn("rapid-group", &fixture_config("graceful"))
            .await
            .unwrap();
        let _reader = ready(&mut process).await;
        let pid = process.pid().unwrap();
        let status = bounded(process.stop(Duration::from_secs(1))).await.unwrap();
        assert!(status.success(), "unexpected stop status: {status:?}");
        assert_eq!(bounded(process.stop(Duration::ZERO)).await.unwrap(), status);
        assert_reaped(pid).await;
    }
}

#[tokio::test]
async fn test_process_forces_non_cooperative_child_after_grace_period() {
    let mut process = ManagedProcess::spawn("stubborn", &fixture_config("stubborn"))
        .await
        .unwrap();
    let _stdout = ready(&mut process).await;
    let grace = Duration::from_millis(50);
    let started = tokio::time::Instant::now();
    let status = bounded(process.stop(grace)).await.unwrap();
    assert!(started.elapsed() >= grace);
    assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
    assert_eq!(process.state().unwrap(), ProcessState::Exited { status });
    assert_eq!(bounded(process.stop(Duration::ZERO)).await.unwrap(), status);
    assert_eq!(bounded(process.wait()).await.unwrap(), status);
}

#[tokio::test]
async fn test_process_stop_cleans_non_cooperative_descendants_after_leader_exits() {
    let mut process = ManagedProcess::spawn("tree", &fixture_config("tree"))
        .await
        .unwrap();
    let mut stdout = BufReader::new(process.take_stdout().unwrap());
    let mut lines = vec![line(&mut stdout).await, line(&mut stdout).await];
    lines.sort();
    assert_eq!(lines, ["parent-ready\n", "ready\n"]);
    assert!(bounded(process.stop(Duration::from_secs(1)))
        .await
        .unwrap()
        .success());
    let mut tail = String::new();
    // The descendant holds stdout open; EOF proves the whole group was stopped.
    bounded(stdout.read_to_string(&mut tail)).await.unwrap();
    assert!(tail.is_empty());
}

#[tokio::test]
async fn test_process_wait_cleans_background_descendants_after_natural_exit() {
    let mut process = ManagedProcess::spawn("background", &fixture_config("background"))
        .await
        .unwrap();
    let mut stdout = process.take_stdout().unwrap();
    assert_eq!(bounded(process.wait()).await.unwrap().code(), Some(7));
    let mut output = String::new();
    bounded(stdout.read_to_string(&mut output)).await.unwrap();
    assert!(output.contains("leader-exiting\n"));
}

#[tokio::test]
async fn test_process_try_wait_cleans_background_descendants_after_natural_exit() {
    let mut process = ManagedProcess::spawn("poll-background", &fixture_config("background"))
        .await
        .unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = process.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("polling the background leader timed out");
    assert_eq!(status.code(), Some(7));
    let mut output = String::new();
    let drained =
        tokio::time::timeout(Duration::from_secs(10), stdout.read_to_string(&mut output)).await;
    assert!(
        drained.is_ok(),
        "descendant stdout remained open after group cleanup; output: {output:?}"
    );
    drained.unwrap().unwrap();
    assert!(output.contains("leader-exiting\n"));
}

#[tokio::test]
async fn test_process_drop_kills_and_reaps_leader_and_descendants() {
    let mut process = ManagedProcess::spawn("drop-tree", &fixture_config("tree"))
        .await
        .unwrap();
    let pid = process.pid().unwrap();
    let mut stdout = BufReader::new(process.take_stdout().unwrap());
    let _first = line(&mut stdout).await;
    let _second = line(&mut stdout).await;
    drop(process);
    let mut output = String::new();
    bounded(stdout.read_to_string(&mut output)).await.unwrap();
    assert_reaped(pid).await;
}

#[tokio::test]
async fn test_process_wait_and_stop_cancellation_preserve_ownership() {
    let mut process = ManagedProcess::spawn("cancel", &fixture_config("stubborn"))
        .await
        .unwrap();
    let _stdout = ready(&mut process).await;
    let pid = process.pid().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), process.wait())
            .await
            .is_err()
    );
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        process.stop(Duration::from_secs(5))
    )
    .await
    .is_err());
    assert_eq!(process.state().unwrap(), ProcessState::Running { pid });
    assert_eq!(
        bounded(process.stop(Duration::ZERO))
            .await
            .unwrap()
            .signal(),
        Some(nix::libc::SIGKILL)
    );
    assert_reaped(pid).await;
}

#[tokio::test]
async fn test_process_rejects_overflowing_grace_period_without_signaling_child() {
    let mut process = ManagedProcess::spawn("overflow", &fixture_config("stubborn"))
        .await
        .unwrap();
    let _stdout = ready(&mut process).await;
    let pid = process.pid().unwrap();
    assert!(matches!(
        process.stop(Duration::MAX).await,
        Err(ProcessError::InvalidGracePeriod { .. })
    ));
    assert_eq!(process.state().unwrap(), ProcessState::Running { pid });
    assert_eq!(
        bounded(process.stop(Duration::ZERO))
            .await
            .unwrap()
            .signal(),
        Some(nix::libc::SIGKILL)
    );
}

#[tokio::test]
async fn test_process_restart_has_new_pipes_and_uses_original_config_snapshot() {
    let mut config = fixture_config("graceful");
    let mut process = ManagedProcess::spawn("restart", &config).await.unwrap();
    let mut old_stdout = ready(&mut process).await;
    let old_pid = process.pid().unwrap();
    config.command = "missing-command-after-config-mutation".into();
    let new_pid = bounded(process.restart(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_ne!(old_pid, new_pid);
    let _new_stdout = ready(&mut process).await;
    let mut old_output = String::new();
    bounded(old_stdout.read_to_string(&mut old_output))
        .await
        .unwrap();
    assert_eq!(old_output, "stopped\n");
    assert!(bounded(process.stop(Duration::from_secs(1)))
        .await
        .unwrap()
        .success());
}

#[tokio::test]
async fn test_process_failed_restart_leaves_old_generation_stopped() {
    let directory = tempdir().unwrap();
    let path = directory.path().join(".env");
    tokio::fs::write(&path, "DEVD_RESTART_TEST=1\n")
        .await
        .unwrap();
    let mut config = fixture_config("graceful");
    config.env_file = Some(path.clone());
    let mut process = ManagedProcess::spawn("failed-restart", &config)
        .await
        .unwrap();
    let _stdout = ready(&mut process).await;
    tokio::fs::remove_file(&path).await.unwrap();
    assert!(matches!(
        bounded(process.restart(Duration::from_secs(1))).await,
        Err(ProcessError::EnvironmentRead { .. })
    ));
    assert_eq!(process.pid(), None);
    assert!(
        matches!(process.state().unwrap(), ProcessState::Exited { status } if status.success())
    );
    tokio::fs::write(&path, "DEVD_RESTART_TEST=2\n")
        .await
        .unwrap();
    bounded(process.restart(Duration::from_secs(1)))
        .await
        .unwrap();
    let _new_stdout = ready(&mut process).await;
    assert!(bounded(process.stop(Duration::from_secs(1)))
        .await
        .unwrap()
        .success());
}

#[tokio::test]
async fn test_process_large_stdout_and_stderr_can_be_drained_concurrently() {
    let mut process = ManagedProcess::spawn("output", &fixture_config("output"))
        .await
        .unwrap();
    let mut stdout = process.take_stdout().unwrap();
    let mut stderr = process.take_stderr().unwrap();
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let (out, err, status) = bounded(async {
        tokio::join!(
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut errors),
            process.wait()
        )
    })
    .await;
    out.unwrap();
    err.unwrap();
    assert!(status.unwrap().success());
    assert_eq!(output, errors);
    assert_eq!(output.len(), 33 * 4096);
}
