#![cfg(unix)]

use devd::core::service_manager::RuntimeSnapshot;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Project {
    directory: TempDir,
}
impl Project {
    fn new(yaml: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("devd-cli-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::write(
            directory.path().join("devd.yml"),
            format!("version: '1'\n{yaml}"),
        )
        .unwrap();
        Self { directory }
    }
    fn path(&self) -> &Path {
        self.directory.path()
    }
    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devd"));
        command.current_dir(self.path()).args(arguments);
        if !arguments.contains(&"--color") {
            command.args(["--color", "never"]);
        }
        command
    }
    fn invoke(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }
    fn start(&self) -> Supervisor {
        let child = self
            .command(&["start"])
            .stdout(fs::File::create(self.path().join("stdout")).unwrap())
            .stderr(fs::File::create(self.path().join("stderr")).unwrap())
            .spawn()
            .unwrap();
        Supervisor(child)
    }
    fn snapshot(&self) -> Option<RuntimeSnapshot> {
        let output = self.invoke(&["status", "--json"]);
        if !output.status.success() {
            return None;
        }
        Some(serde_json::from_slice(&output.stdout).unwrap())
    }
    fn running(&self) -> RuntimeSnapshot {
        wait(|| {
            self.snapshot()
                .filter(|s| s.services["worker"].pid.is_some())
        })
    }
}

struct Supervisor(Child);
impl Supervisor {
    fn finish(&mut self, success: bool) {
        let status = wait(|| self.0.try_wait().unwrap());
        assert_eq!(status.success(), success);
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.0.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + Duration::from_secs(8);
            while self.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wait<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "CLI condition timed out");
        thread::sleep(Duration::from_millis(15));
    }
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(expected), "{error}");
}
const RUNNING: &str = "services:\n  worker:\n    command: sh -c 'echo hello; echo problem >&2; exec sleep 60'\n    restart:\n      policy: never\n";

#[test]
fn test_cli_help_validation_graph_and_failures() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\n  api:\n    command: sleep 60\n    depends-on: [worker]\n");
    assert!(success(project.invoke(&["--help"])).contains("restart"));
    assert!(success(project.invoke(&["check"])).contains("Configuration valid"));
    let graph = success(project.invoke(&["graph"]));
    assert!(graph.contains("api -> worker (Started)"), "{graph}");
    assert!(graph.contains("1: worker\n  2: api"));
    assert!(!project.path().join(".devd").exists());
    for command in ["init", "top", "wat"] {
        failure(project.invoke(&[command]), "unrecognized subcommand");
    }
    for arguments in [
        &["status"][..],
        &["stop"],
        &["logs"],
        &["restart", "worker"],
    ] {
        failure(project.invoke(arguments), "no reachable devd supervisor");
    }
    fs::write(project.path().join("devd.yml"), "services: [").unwrap();
    for command in ["check", "graph", "start"] {
        failure(project.invoke(&[command]), "error:");
    }
}

#[test]
fn test_cli_lifecycle_logs_restart_duplicate_and_config_changes() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    let first = project.running().services["worker"].pid.unwrap();
    assert!(success(project.invoke(&["status"])).contains(&first.to_string()));
    wait(|| {
        success(project.invoke(&["logs", "worker"]))
            .contains("problem")
            .then_some(())
    });
    assert!(success(project.invoke(&["logs"])).contains("hello"));
    assert_eq!(
        success(project.invoke(&["logs", "--tail", "1"]))
            .lines()
            .count(),
        1
    );
    failure(project.invoke(&["logs", "absent"]), "unknown service");
    failure(project.invoke(&["logs", "--tail", "0"]), "invalid value");
    let colored = project
        .command(&["logs", "--color", "always"])
        .output()
        .unwrap();
    assert!(success(colored).contains('\u{1b}'));
    failure(project.invoke(&["restart", "absent"]), "unknown service");
    failure(project.invoke(&["start"]), "another supervisor");
    assert!(success(project.invoke(&["restart", "worker"])).contains("Restarted worker"));
    let snapshot = project.running();
    assert_ne!(snapshot.services["worker"].pid.unwrap(), first);
    assert_eq!(snapshot.services["worker"].restart_count, 1);
    fs::write(project.path().join("devd.yml"), "invalid: [").unwrap();
    success(project.invoke(&["status"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    assert!(!project.path().join(".devd/devd.yml/control.sock").exists());
    failure(project.invoke(&["status"]), "no reachable devd supervisor");
    failure(project.invoke(&["stop"]), "no reachable devd supervisor");
    failure(project.invoke(&["logs"]), "no reachable devd supervisor");
}

#[test]
fn test_cli_relative_cwd_env_file_and_deleted_config() {
    let project = Project::new("services:\n  worker:\n    command: sh -c 'echo $MARKER; pwd; exec sleep 60'\n    cwd: service\n    env-file: local.env\n    restart: {policy: never}\n");
    fs::create_dir(project.path().join("service")).unwrap();
    fs::write(
        project.path().join("service/local.env"),
        "MARKER=relative-path-ok\n",
    )
    .unwrap();
    let mut command = project.command(&["start"]);
    command
        .current_dir("/tmp")
        .arg("--config")
        .arg(project.path().join("devd.yml"))
        .stdout(fs::File::create(project.path().join("stdout")).unwrap())
        .stderr(Stdio::piped());
    let mut supervisor = Supervisor(command.spawn().unwrap());
    project.running();
    wait(|| {
        success(project.invoke(&["logs"]))
            .contains("relative-path-ok")
            .then_some(())
    });
    assert!(success(project.invoke(&["logs"])).contains("/service"));
    fs::remove_file(project.path().join("devd.yml")).unwrap();
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_spawn_failure_and_unsupported_config() {
    let project = Project::new("services:\n  worker:\n    command: /no/such/devd-test-program\n    restart: {policy: never}\n");
    failure(project.invoke(&["start"]), "services failed");
    fs::write(
        project.path().join("devd.yml"),
        "version: '1'\nservices:\n  worker:\n    command: sleep 60\n    restart: {backoff: exponential}\n",
    )
    .unwrap();
    failure(project.invoke(&["check"]), "requires v0.2");
    fs::write(
        project.path().join("devd.yml"),
        "version: '1'\nservices:\n  worker:\n    command: \"sh '\"\n",
    )
    .unwrap();
    failure(project.invoke(&["check"]), "failed to parse command");
}

#[test]
fn test_cli_restart_stopped_service_and_failure() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\n    restart: {policy: never}\n  once:\n    command: sh -c 'echo once; exit 0'\n    restart: {policy: never}\n");
    let mut supervisor = project.start();
    project.running();
    wait(|| {
        project.snapshot().filter(|s| {
            s.services["once"].status == devd::core::service_manager::ServiceState::Stopped
        })
    });
    // A short-lived command may be observed running or already exited.
    let output = project.invoke(&["restart", "once"]);
    if !output.status.success() {
        failure(output, "exited before restart completed");
    }
    wait(|| {
        project
            .snapshot()
            .filter(|s| s.services["once"].restart_count == 1)
    });
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_restart_failure_reports_cause_and_cleans_stack() {
    let project = Project::new("services:\n  worker:\n    command: sleep 60\n    env-file: local.env\n    restart: {policy: never}\n");
    fs::write(project.path().join("local.env"), "VALUE=ok\n").unwrap();
    let mut supervisor = project.start();
    let pid = project.running().services["worker"].pid.unwrap();
    fs::remove_file(project.path().join("local.env")).unwrap();
    failure(project.invoke(&["restart", "worker"]), "local.env");
    supervisor.finish(false);
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

#[test]
fn test_cli_malformed_oversized_and_idle_clients_do_not_block_control() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
    };
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    project.running();
    let path = project.path().join(".devd/devd.yml/control.sock");
    let _idle = UnixStream::connect(&path).unwrap();
    for bytes in [vec![0, 0, 0, 1, b'!'], vec![255, 255, 255, 255]] {
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(&bytes).unwrap();
        let mut length = [0; 4];
        client.read_exact(&mut length).unwrap();
        let mut response = vec![0; u32::from_be_bytes(length) as usize];
        client.read_exact(&mut response).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response).unwrap()["result"],
            "error"
        );
    }
    success(project.invoke(&["status"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_cli_signals_null_output_and_stale_endpoint() {
    for signal in [
        nix::sys::signal::Signal::SIGINT,
        nix::sys::signal::Signal::SIGTERM,
    ] {
        let project = Project::new(RUNNING);
        let state_dir = project.path().join(".devd/devd.yml");
        fs::create_dir_all(&state_dir).unwrap();
        let stale = std::os::unix::net::UnixListener::bind(state_dir.join("control.sock")).unwrap();
        drop(stale);
        let mut command = project.command(&["start"]);
        let mut supervisor = Supervisor(
            command
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let pid = project.running().services["worker"].pid.unwrap();
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(supervisor.0.id() as i32), signal)
            .unwrap();
        supervisor.finish(true);
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        );
        assert!(!state_dir.join("control.sock").exists());
    }
}

#[test]
fn test_cli_broken_and_stalled_output_shut_down_without_hanging() {
    for broken in [false, true] {
        let project = Project::new("services:\n  worker:\n    command: sh -c 'while :; do echo noisy-output; done'\n    restart: {policy: never}\n");
        let mut command = project.command(&["start"]);
        let mut supervisor = Supervisor(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        project.running();
        if broken {
            drop(supervisor.0.stdout.take());
        } else {
            // Leave the pipe unread while the producer fills it.
            wait(|| {
                success(project.invoke(&["logs", "--tail", "1000"]))
                    .lines()
                    .count()
                    .eq(&1000)
                    .then_some(())
            });
            success(project.invoke(&["stop"]));
        }
        supervisor.finish(!broken);
    }
}

#[test]
fn test_cli_explicit_state_directory_and_non_socket_preservation() {
    let project = Project::new(RUNNING);
    let state = project.path().join("runtime");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("control.sock"), "keep this file").unwrap();
    failure(
        project.invoke(&["start", "--state-dir", "runtime"]),
        "refusing to replace",
    );
    assert_eq!(
        fs::read_to_string(state.join("control.sock")).unwrap(),
        "keep this file"
    );
    fs::remove_file(state.join("control.sock")).unwrap();
    let mut command = project.command(&["start", "--state-dir", "runtime"]);
    let mut supervisor = Supervisor(
        command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait(|| {
        project
            .invoke(&["status", "--state-dir", "runtime"])
            .status
            .success()
            .then_some(())
    });
    failure(project.invoke(&["status"]), "no reachable devd supervisor");
    success(project.invoke(&["stop", "--state-dir", "runtime"]));
    supervisor.finish(true);
}
