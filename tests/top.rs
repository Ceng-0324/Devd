#![cfg(unix)]

mod support;

use nix::{
    fcntl::{fcntl, FcntlArg, OFlag},
    pty::{openpty, Winsize},
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use std::{
    fs,
    io::{Read, Write},
    thread,
    time::{Duration, Instant},
};
use support::{failure, success, wait, Project, Supervisor};

const RUNNING: &str = "services:\n  worker:\n    command: sh -c 'echo ready; exec sleep 60'\n    restart: {policy: never}\n";

struct Top {
    child: Supervisor,
    master: fs::File,
    // macOS may discard unread output when the last slave closes. Keep one
    // open until the test has drained even an immediately exiting command.
    _slave: fs::File,
    screen: String,
    terminal: vt100::Parser,
}

impl Top {
    fn start(project: &Project) -> Self {
        let size = Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pair = openpty(Some(&size), None).unwrap();
        let slave = fs::File::from(pair.slave);
        let mut command = project.command(&["top"]);
        let child = command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap())
            .spawn()
            .unwrap();
        let master = fs::File::from(pair.master);
        let flags = OFlag::from_bits_truncate(fcntl(&master, FcntlArg::F_GETFL).unwrap());
        fcntl(&master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
        Self {
            child: Supervisor(child),
            master,
            _slave: slave,
            screen: String::new(),
            terminal: vt100::Parser::new(size.ws_row, size.ws_col, 0),
        }
    }

    fn write(&mut self, input: &[u8]) {
        self.master.write_all(input).unwrap();
    }

    fn seen(&mut self, expected: &str) -> bool {
        self.drain();
        self.terminal.screen().contents().contains(expected)
    }

    fn drain(&mut self) {
        let mut bytes = [0u8; 8192];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(size) => {
                    // Ratatui writes only changed cells, which may split words
                    // across frames and UTF-8 characters across PTY reads.
                    self.terminal.process(&bytes[..size]);
                    self.screen
                        .push_str(&String::from_utf8_lossy(&bytes[..size]));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("PTY read failed: {error}"),
            }
        }
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            self.drain();
            if let Some(status) = self.child.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "top exit: {status}, screen: {}",
                    self.screen
                );
                self.drain();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "top did not exit: {}",
                self.screen
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn test_top_requires_tty_and_running_supervisor() {
    let project = Project::new(RUNNING);
    failure(project.invoke(&["top"]), "interactive terminal");
    let mut top = Top::start(&project);
    let status = wait(|| {
        top.drain();
        top.child.0.try_wait().unwrap()
    });
    top.drain();
    assert!(!status.success(), "unexpected success: {}", top.screen);
    assert!(
        top.screen.contains("no reachable devd supervisor"),
        "missing connection error: {}",
        top.screen
    );
}

#[test]
fn test_top_restart_cancel_quit_and_confirm_stop() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    let initial = project.running().services["worker"].pid;
    let mut top = Top::start(&project);
    wait(|| top.seen("worker").then_some(()));
    top.write(b"r");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        top.drain();
        let snapshot = project.snapshot();
        if snapshot.as_ref().is_some_and(|state| {
            state.services["worker"].restart_count == 1 && state.services["worker"].pid != initial
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "top status: {:?}, screen: {}",
            snapshot,
            top.screen
        );
        thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while !top.seen("Restarted") {
        assert!(
            Instant::now() < deadline,
            "restart notice was not rendered: {}",
            top.screen
        );
        thread::sleep(Duration::from_millis(20));
    }
    top.write(b"s");
    let deadline = Instant::now() + Duration::from_secs(4);
    while !top.seen("Esc/n") {
        assert!(
            Instant::now() < deadline,
            "confirmation prompt was not rendered: {}",
            top.screen
        );
        thread::sleep(Duration::from_millis(20));
    }
    top.write(b"n");
    assert!(project.snapshot().is_some());
    top.write(b"q");
    top.finish();
    assert!(
        top.screen.contains("\x1b[?1049l"),
        "q must restore the terminal: {}",
        top.screen
    );
    assert!(
        success(project.invoke(&["status"])).contains("worker"),
        "q must not stop the supervisor"
    );

    let mut top = Top::start(&project);
    wait(|| top.seen("worker").then_some(()));
    top.write(b"s");
    wait(|| top.seen("Esc/n").then_some(()));
    top.write(b"\r");
    top.finish();
    supervisor.finish(true);
}

#[test]
fn test_top_sigterm_restores_terminal_without_stopping_stack() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    project.running();
    let mut top = Top::start(&project);
    wait(|| top.seen("worker").then_some(()));
    kill(Pid::from_raw(top.child.0.id() as i32), Signal::SIGTERM).unwrap();
    top.finish();
    assert!(
        top.screen.contains("\x1b[?1049l"),
        "SIGTERM must restore the terminal: {}",
        top.screen
    );
    success(project.invoke(&["status"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}

#[test]
fn test_top_disconnect_restores_terminal_and_reports_failure() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    project.running();
    let mut top = Top::start(&project);
    wait(|| top.seen("worker").then_some(()));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
    let status = wait(|| {
        top.drain();
        top.child.0.try_wait().unwrap()
    });
    top.drain();
    assert!(!status.success());
    assert!(top.screen.contains("\x1b[?1049l"));
    assert!(
        top.screen.contains("supervisor closed the log stream")
            || top.screen.contains("no reachable devd supervisor")
    );
}

#[test]
fn test_top_ctrl_c_only_closes_view() {
    let project = Project::new(RUNNING);
    let mut supervisor = project.start();
    project.running();
    let mut top = Top::start(&project);
    wait(|| top.seen("worker").then_some(()));
    top.write(b"\x03");
    top.finish();
    assert!(top.screen.contains("\x1b[?1049l"));
    success(project.invoke(&["status"]));
    success(project.invoke(&["stop"]));
    supervisor.finish(true);
}
