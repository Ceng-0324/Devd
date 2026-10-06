use devd::core::service_manager::RuntimeSnapshot;
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub struct Project {
    directory: TempDir,
}

impl Project {
    pub fn new(yaml: &str) -> Self {
        Self::from_document(&format!("version: '1'\n{yaml}"))
    }

    pub fn from_document(yaml: &str) -> Self {
        // Keep Unix socket paths short on macOS, whose default TMPDIR is long.
        let directory = tempfile::Builder::new()
            .prefix("devd-test-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::write(directory.path().join("devd.yml"), yaml).unwrap();
        Self { directory }
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devd"));
        command.current_dir(self.path()).args(arguments);
        if !arguments.contains(&"--color") {
            command.args(["--color", "never"]);
        }
        command
    }

    pub fn invoke(&self, arguments: &[&str]) -> Output {
        command_output(self.command(arguments))
    }

    pub fn start(&self) -> Supervisor {
        Supervisor(
            self.command(&["start"])
                .stdout(fs::File::create(self.path().join("stdout")).unwrap())
                .stderr(fs::File::create(self.path().join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        )
    }

    pub fn snapshot(&self) -> Option<RuntimeSnapshot> {
        let output = self.invoke(&["status", "--json"]);
        output
            .status
            .success()
            .then(|| serde_json::from_slice(&output.stdout).unwrap())
    }

    pub fn running(&self) -> RuntimeSnapshot {
        wait(|| {
            self.snapshot()
                .filter(|s| s.services["worker"].pid.is_some())
        })
    }
}

pub struct Supervisor(pub Child);

impl Supervisor {
    pub fn finish(&mut self, success: bool) {
        let status = wait(|| self.0.try_wait().unwrap());
        assert_eq!(status.success(), success, "supervisor exit: {status}");
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = kill(Pid::from_raw(self.0.id() as i32), Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(8);
            while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// File-backed output avoids pipe-capacity deadlocks; every CLI subprocess is
/// owned during unwinding and bounded even when the command itself hangs.
pub fn command_output(mut command: Command) -> Output {
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let description = format!("{command:?}");
    let mut process = Supervisor(
        command
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            drop(process);
            panic!(
                "command timed out: {description}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&read_output(&mut stdout)),
                String::from_utf8_lossy(&read_output(&mut stderr))
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    Output {
        status,
        stdout: read_output(&mut stdout),
        stderr: read_output(&mut stderr),
    }
}

fn read_output(file: &mut fs::File) -> Vec<u8> {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

pub fn wait<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "CLI condition timed out");
        thread::sleep(Duration::from_millis(15));
    }
}

pub fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

pub fn failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(expected), "{error}");
}
