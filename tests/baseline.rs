//! Explicit drift checks on native filesystems. No service is executed by this suite.
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output},
    time::{Duration, Instant},
};

struct Project(tempfile::TempDir);
impl Project {
    fn new(fields: &str) -> Self {
        let p = Self(tempfile::tempdir().unwrap());
        p.config(fields);
        p
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn config(&self, fields: &str) {
        fs::write(self.path().join("devd.yml"), format!("version: '1'\nservices:\n  api:\n    command: './never-execute --secret sentinel'\n{fields}  web:\n    command: nonexistent\n    depends-on: [api]\n  worker:\n    command: nonexistent\n    depends-on: [web]\n")).unwrap();
    }
    fn invoke(&self, args: &[&str]) -> Output {
        let stdout = tempfile::NamedTempFile::new().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let mut child = Guard(
            Command::new(env!("CARGO_BIN_EXE_devd"))
                .current_dir(self.path().parent().unwrap())
                .arg("--config")
                .arg(self.path().join("devd.yml"))
                .args(args)
                .stdout(stdout.reopen().unwrap())
                .stderr(stderr.reopen().unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(s) = child.0.try_wait().unwrap() {
                break s;
            }
            assert!(Instant::now() < deadline, "baseline CLI timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        Output {
            status,
            stdout: fs::read(stdout.path()).unwrap(),
            stderr: fs::read(stderr.path()).unwrap(),
        }
    }
    fn capture(&self, resources: &[&str], output: &str, extra: &[&str]) -> Output {
        let path = self.path().join(output);
        let mut args = vec![
            "baseline",
            "capture",
            "--service",
            "api",
            "--output",
            path.to_str().unwrap(),
        ];
        for r in resources {
            args.extend(["--resource", r]);
        }
        args.extend(extra);
        self.invoke(&args)
    }
    fn check(&self, input: &str, extra: &[&str]) -> (bool, Value) {
        let path = self.path().join(input);
        let mut args = vec!["baseline", "check", path.to_str().unwrap(), "--json"];
        args.extend(extra);
        let output = self.invoke(&args);
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.success(), value)
    }
}
struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn symlink(target: &str, path: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, path).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target, path)
        .expect("Windows baseline acceptance requires Developer Mode or symlink privileges");
}
fn assert_success(o: Output) {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn test_baseline_valid_retarget_missing_recovery_privacy_and_no_clobber() {
    let p = Project::new("    requires: [{type: file, path: alias}]\n");
    fs::write(p.path().join("a"), "SECRET_CONTENT_A").unwrap();
    fs::write(p.path().join("b"), "SECRET_CONTENT_B").unwrap();
    symlink("a", &p.path().join("alias"));
    assert_success(p.capture(&["requires:0"], "base.json", &[]));
    let original = fs::read(p.path().join("base.json")).unwrap();
    assert!(!String::from_utf8_lossy(&original).contains("SECRET_CONTENT"));
    assert!(!String::from_utf8_lossy(&original).contains("sentinel"));
    let (ok, report) = p.check("base.json", &[]);
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "no-observed-change");
    assert_eq!(
        report["entries"][0]["current_declared_impact"]["services"],
        serde_json::json!(["api", "web", "worker"])
    );
    fs::remove_file(p.path().join("alias")).unwrap();
    symlink("b", &p.path().join("alias"));
    let (ok, report) = p.check("base.json", &[]);
    assert!(!ok);
    assert_eq!(report["outcome"], "changed");
    assert!(report["entries"][0]["comparison"]["reasons"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("resolution-endpoint-changed")));
    assert!(p.invoke(&["doctor", "--json"]).stdout.len() > 10);
    fs::remove_file(p.path().join("b")).unwrap();
    let (_, report) = p.check("base.json", &[]);
    assert_eq!(
        report["entries"][0]["current"]["evidence"]["inspection"]["failure"],
        "dangling-symlink"
    );
    assert_success(p.capture(&["requires:0"], "missing.json", &[]));
    fs::write(p.path().join("b"), "recovered").unwrap();
    let (ok, report) = p.check("missing.json", &[]);
    assert!(!ok);
    assert_eq!(report["outcome"], "changed");
    assert!(!p
        .capture(&["requires:0"], "base.json", &[])
        .status
        .success());
    assert_eq!(fs::read(p.path().join("base.json")).unwrap(), original);
    assert!(!p.path().join(".devd/devd.yml/services.json").exists());
}

#[test]
fn test_baseline_selectors_metadata_replacement_and_directory_activity() {
    let p = Project::new(
        "    env-file: .env\n    paths:\n      CACHE: {path: cache, scope: instance}\n",
    );
    fs::write(p.path().join(".env"), "TOP_SECRET=never-read").unwrap();
    fs::write(p.path().join("never-execute"), "not executable").unwrap();
    assert_success(p.capture(
        &["env-file", "program", "cwd", "paths:CACHE"],
        "base.json",
        &[],
    ));
    fs::write(
        p.path().join("unrelated"),
        "directory activity is not drift",
    )
    .unwrap();
    let (ok, report) = p.check("base.json", &[]);
    assert!(ok, "{report}");
    assert!(!report.to_string().contains("TOP_SECRET"));
    fs::rename(p.path().join(".env"), p.path().join("old.env")).unwrap();
    fs::write(p.path().join(".env"), "replacement with different metadata").unwrap();
    let (ok, report) = p.check("base.json", &[]);
    assert!(!ok);
    assert_eq!(report["entries"][0]["comparison"]["outcome"], "changed");
    assert_eq!(
        report["entries"][2]["comparison"]["outcome"],
        "no-observed-change"
    );
    assert_eq!(
        report["entries"][3]["current"]["declaration"]["scope"],
        "instance"
    );
    assert!(!p.path().join(".devd/devd.yml/runtime/cache").exists());
}

#[test]
fn test_baseline_declaration_change_removed_selection_and_foreign_context() {
    let p = Project::new("    requires: [{type: file, path: a}]\n");
    fs::write(p.path().join("a"), "a").unwrap();
    fs::write(p.path().join("b"), "b").unwrap();
    assert_success(p.capture(&["requires:0"], "base.json", &[]));
    p.config("    requires: [{type: file, path: b}]\n");
    let (ok, report) = p.check("base.json", &[]);
    assert!(!ok);
    assert_eq!(report["outcome"], "unknown");
    assert_eq!(
        report["entries"][0]["comparison"]["reasons"][0],
        "resource-declaration-changed"
    );
    p.config("");
    let (ok, report) = p.check("base.json", &[]);
    assert!(!ok);
    assert_eq!(
        report["entries"][0]["comparison"]["reasons"][0],
        "resource-declaration-unavailable"
    );
    p.config("    requires: [{type: file, path: a}]\n");
    fs::remove_file(p.path().join(".devd/devd.yml/baseline-origin.id")).unwrap();
    let (ok, report) = p.check("base.json", &[]);
    assert!(!ok);
    assert_eq!(report["outcome"], "unknown");
    assert_eq!(report["local_context_comparable"], false);
    assert!(!p.path().join(".devd/devd.yml/baseline-origin.id").exists());
    let recipient = Project::new("    requires: [{type: file, path: a}]\n");
    fs::copy(
        p.path().join("base.json"),
        recipient.path().join("base.json"),
    )
    .unwrap();
    let (_, report) = recipient.check("base.json", &[]);
    assert_eq!(report["outcome"], "unknown");
    assert!(!recipient.path().join(".devd").exists());
}

#[test]
fn test_baseline_rejects_unsafe_malformed_oversized_and_duplicate_inputs() {
    let p = Project::new("");
    assert!(!p.capture(&["cwd", "cwd"], "bad.json", &[]).status.success());
    assert!(!p.path().join(".devd").exists());
    assert_success(p.capture(&["cwd"], "base.json", &[]));
    let path = p.path().join("base.json");
    let invoke = |name: &str| {
        let path = p.path().join(name);
        p.invoke(&["baseline", "check", path.to_str().unwrap(), "--json"])
    };
    symlink("base.json", &p.path().join("link.json"));
    assert!(!invoke("link.json").status.success());
    fs::hard_link(&path, p.path().join("hard.json")).unwrap();
    assert!(!invoke("hard.json").status.success());
    fs::remove_file(p.path().join("hard.json")).unwrap();
    let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (pointer, value) in [
        ("/entries/0/evidence/inspection/resolved", Value::Null),
        (
            "/entries/0/evidence/inspection/failure",
            serde_json::json!("missing"),
        ),
        (
            "/entries/0/evidence/inspection/change_detected",
            serde_json::json!(true),
        ),
        (
            "/entries/0/evidence/inspection/nodes/0/target",
            serde_json::json!("forged-link"),
        ),
        ("/entries/0/evidence/metadata/0", Value::Null),
        ("/entries/0/evidence/metadata/0/size", serde_json::json!(42)),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        fs::write(p.path().join("bad.json"), serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(!invoke("bad.json").status.success(), "{pointer}");
        assert!(invoke("bad.json").stdout.is_empty(), "{pointer}");
    }
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["schema_version"] = serde_json::json!(99);
    fs::write(
        p.path().join("bad.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    assert!(
        String::from_utf8_lossy(&invoke("bad.json").stderr).contains("unsupported baseline schema")
    );
    document["schema_version"] = serde_json::json!(1);
    document["entries"][0]["evidence"]["metadata"] = serde_json::json!([]);
    fs::write(
        p.path().join("bad.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    assert!(!invoke("bad.json").status.success());
    fs::write(p.path().join("large.json"), vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    assert!(!invoke("large.json").status.success());
    #[cfg(unix)]
    {
        nix::unistd::mkfifo(&p.path().join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(!invoke("fifo").status.success());
    }
}

#[test]
fn test_baseline_profile_custom_state_and_alias_consumers() {
    let p = Project::new("");
    fs::write(p.path().join("a"), "a").unwrap();
    fs::write(p.path().join("b"), "b").unwrap();
    let yaml = "version: '1'\nservices:\n  api:\n    command: never-execute\n    requires: [{type: file, path: a}]\n  other:\n    command: never-execute\n    requires: [{type: file, path: a}]\n  web:\n    command: never-execute\n    depends-on: [other]\nprofiles:\n  second:\n    services:\n      api:\n        requires: [{type: file, path: b}]\n";
    fs::write(p.path().join("devd.yml"), yaml).unwrap();
    let state = p.path().join("custom-state");
    let args = ["--state-dir", state.to_str().unwrap()];
    assert_success(p.capture(&["requires:0"], "base.json", &args));
    let (ok, r) = p.check("base.json", &args);
    assert!(ok, "{r}");
    assert_eq!(
        r["entries"][0]["current_declared_impact"]["services"],
        serde_json::json!(["api", "other", "web"])
    );
    let (ok, r) = p.check(
        "base.json",
        &[
            "--profile",
            "second",
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );
    assert!(!ok);
    assert_eq!(r["outcome"], "unknown");
    assert!(!state.join("profiles").exists());
    let args = [
        "--profile",
        "second",
        "--state-dir",
        state.to_str().unwrap(),
    ];
    assert_success(p.capture(&["requires:0"], "second.json", &args));
    let (ok, r) = p.check("second.json", &args);
    assert!(ok, "{r}");
    assert_eq!(r["context"]["profile"], "second");
    assert_eq!(
        r["entries"][0]["current_declared_impact"]["services"],
        serde_json::json!(["api"])
    );
    assert!(!p.path().join(".devd").exists());
}

#[test]
fn test_baseline_selection_limit_and_untrusted_paths_do_not_drive_io() {
    let mut fields = String::from("    requires:\n");
    for n in 0..33 {
        fields.push_str(&format!("      - {{type: file, path: missing{n}}}\n"));
    }
    let p = Project::new(&fields);
    let selected: Vec<_> = (0..33).map(|n| format!("requires:{n}")).collect();
    let selected: Vec<_> = selected.iter().map(String::as_str).collect();
    assert!(!p.capture(&selected, "bad.json", &[]).status.success());
    assert!(!p.path().join(".devd").exists());
    assert_success(p.capture(&selected[..32], "base.json", &[]));
    let (ok, r) = p.check("base.json", &[]);
    assert!(ok, "{r}");
    assert_eq!(r["entries"].as_array().unwrap().len(), 32);
    let mut b: Value =
        serde_json::from_slice(&fs::read(p.path().join("base.json")).unwrap()).unwrap();
    // A forged ambient path is a recorded assertion, not an IO directive.
    let malicious = p.path().join("should-not-be-opened");
    #[cfg(unix)]
    nix::unistd::mkfifo(&malicious, nix::sys::stat::Mode::S_IRUSR).unwrap();
    b["entries"][0]["declaration"]["requested_path"] = serde_json::json!(malicious);
    fs::write(
        p.path().join("imported.json"),
        serde_json::to_vec(&b).unwrap(),
    )
    .unwrap();
    let (ok, r) = p.check("imported.json", &[]);
    assert!(!ok);
    assert_eq!(
        r["entries"][0]["comparison"]["reasons"][0],
        "resource-declaration-changed"
    );
    assert_ne!(
        r["entries"][0]["current"]["declaration"]["requested_path"],
        b["entries"][0]["declaration"]["requested_path"]
    );
}

#[test]
fn test_baseline_output_backpressure_has_deadline_and_cancellation() {
    let mut fields = String::from("    requires:\n");
    let nested = "component/".repeat(24);
    for n in 0..32 {
        fields.push_str(&format!(
            "      - {{type: file, path: '{nested}missing{n}'}}\n"
        ));
    }
    let p = Project::new(&fields);
    fs::create_dir_all(p.path().join(&nested)).unwrap();
    let selections: Vec<_> = (0..32).map(|n| format!("requires:{n}")).collect();
    let selections: Vec<_> = selections.iter().map(String::as_str).collect();
    assert_success(p.capture(&selections, "base.json", &[]));
    for cancel in [false, true] {
        if cancel && !cfg!(unix) {
            continue;
        }
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let mut child = Guard(
            Command::new(env!("CARGO_BIN_EXE_devd"))
                .current_dir(p.path())
                .args(["baseline", "check", "base.json", "--json"])
                .stdout(std::process::Stdio::piped())
                .stderr(stderr.reopen().unwrap())
                .spawn()
                .unwrap(),
        );
        let mut pipe = child.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut first = [0];
            let result = pipe.read_exact(&mut first);
            let _ = sender.send((pipe, result));
        });
        let (_pipe, first) = receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        first.unwrap();
        reader.join().unwrap();
        #[cfg(unix)]
        if cancel {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.0.id() as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(if cancel { 3 } else { 10 });
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert_eq!(status.code(), Some(1));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "blocked baseline output did not terminate"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let error = fs::read_to_string(stderr.path()).unwrap();
        assert!(
            error.contains(if cancel {
                "baseline output cancelled"
            } else {
                "baseline output timed out"
            }),
            "{error}"
        );
    }
}
