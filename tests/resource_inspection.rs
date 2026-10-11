//! Native, portable doctor acceptance. Symlink privileges are prerequisites.
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
        let directory = tempfile::tempdir().unwrap();
        let command = serde_json::to_string(&format!(
            "{} --version",
            shell_words::quote(env!("CARGO_BIN_EXE_devd"))
        ))
        .unwrap();
        fs::write(
            directory.path().join("devd.yml"),
            format!("version: '1'\nservices:\n  api:\n    command: {command}\n{fields}"),
        )
        .unwrap();
        Self(directory)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn invoke(&self, arguments: &[&str]) -> Output {
        let stdout = tempfile::NamedTempFile::new().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let mut child = Guard(
            Command::new(env!("CARGO_BIN_EXE_devd"))
                // Start elsewhere; config-relative cwd must not change.
                .current_dir(self.path().parent().unwrap())
                .arg("--config")
                .arg(self.path().join("devd.yml"))
                .args(arguments)
                .stdout(stdout.reopen().unwrap())
                .stderr(stderr.reopen().unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "doctor CLI timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        Output {
            status,
            stdout: fs::read(stdout.path()).unwrap(),
            stderr: fs::read(stderr.path()).unwrap(),
        }
    }

    fn json(&self, extra: &[&str]) -> (bool, Value) {
        let mut args = vec!["doctor", "--json"];
        args.extend_from_slice(extra);
        let output = self.invoke(&args);
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.success(), report)
    }
}

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>, directory: bool) {
    #[cfg(unix)]
    {
        let _ = directory;
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    #[cfg(windows)]
    {
        let result = if directory {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        result.expect("Windows acceptance requires Developer Mode or symlink privileges");
    }
}

#[test]
fn test_doctor_explains_intermediate_chain_and_dangling_target() {
    let project =
        Project::new("    cwd: service\n    requires: [{type: file, path: alias/input}]\n");
    fs::create_dir_all(project.path().join("service/real")).unwrap();
    fs::write(
        project.path().join("service/real/input"),
        "private-contents",
    )
    .unwrap();
    symlink("real", project.path().join("service/middle"), true);
    symlink("middle", project.path().join("service/alias"), true);
    let (success, report) = project.json(&[]);
    assert!(success, "{report}");
    assert_eq!(report["resources"]["schema_version"], 1);
    let entry = &report["resources"]["entries"][0];
    assert_eq!(entry["declaration"]["field"], "requires");
    assert_eq!(entry["declaration"]["index"], 0);
    assert_eq!(entry["declaration"]["condition"]["satisfied"], true);
    assert_eq!(entry["declared_path"], "alias/input");
    let nodes = entry["inspection"]["nodes"].as_array().unwrap();
    assert!(nodes.iter().any(|n| n["target"] == "middle"));
    assert!(nodes.iter().any(|n| n["target"] == "real"));
    assert_eq!(entry["inspection"]["atomic"], false);
    assert!(!report.to_string().contains("private-contents"));
    fs::remove_file(project.path().join("service/real/input")).unwrap();
    fs::remove_dir(project.path().join("service/real")).unwrap();
    let (success, report) = project.json(&[]);
    assert!(!success);
    let inspection = &report["resources"]["entries"][0]["inspection"];
    assert_eq!(inspection["failure"], "dangling-symlink");
    let node = inspection["failed_node"].as_u64().unwrap() as usize;
    assert!(inspection["nodes"][node]["path"]
        .as_str()
        .unwrap()
        .ends_with("real"));
    let text = project.invoke(&["doctor"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("DanglingSymlink"));
    assert!(!project.path().join(".devd").exists());
}

#[test]
fn test_profile_and_custom_state_bindings_are_observations_not_prerequisites() {
    let project = Project::new("    cwd: service\n    requires: [{type: file, path: should-be-replaced}]\nprofiles:\n  dev:\n    services:\n      api:\n        requires: []\n        paths:\n          SHARED: {scope: shared, path: shared}\n          CACHE: {scope: instance, path: cache, cleanup: true}\n");
    fs::create_dir(project.path().join("service")).unwrap();
    let state = project.path().join("chosen-state");
    let (success, report) =
        project.json(&["--profile", "dev", "--state-dir", state.to_str().unwrap()]);
    assert!(success, "{report}");
    assert_eq!(report["ready"], true);
    assert_eq!(report["profile"], "dev");
    let entries = report["resources"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let shared = entries
        .iter()
        .find(|e| e["declaration"]["name"] == "SHARED")
        .unwrap();
    assert_eq!(shared["declaration"]["scope"], "shared");
    assert!(Path::new(shared["cwd"].as_str().unwrap()).ends_with("service"));
    let cache = entries
        .iter()
        .find(|e| e["declaration"]["name"] == "CACHE")
        .unwrap();
    assert_eq!(cache["declaration"]["scope"], "instance");
    let requested = Path::new(cache["requested_path"].as_str().unwrap());
    assert!(requested.starts_with(&state));
    assert!(requested.ends_with("runtime/cache"));
    assert!(shared["requested_path"]
        .as_str()
        .unwrap()
        .ends_with(if cfg!(windows) {
            "service\\shared"
        } else {
            "service/shared"
        }));
    assert!(report["resources"]["state_dir"]
        .as_str()
        .unwrap()
        .contains("chosen-state"));
    assert!(
        !state.exists(),
        "doctor must never provision cleanup directories"
    );
}

#[test]
fn test_missing_wrong_type_and_not_symlink_still_use_existing_evaluator() {
    let project = Project::new("    requires:\n      - {type: file, path: missing}\n      - {type: directory, path: file}\n      - {type: symlink, path: file}\n");
    fs::write(project.path().join("file"), "private-contents").unwrap();
    let (success, report) = project.json(&[]);
    assert!(!success);
    let checks = report["checks"].as_array().unwrap();
    for summary in [
        "required path does not exist",
        "path has the wrong type",
        "path is not a symbolic link",
    ] {
        assert!(checks.iter().any(|c| c["summary"] == summary), "{report}");
    }
    assert!(!report.to_string().contains("private-contents"));
    let entries = report["resources"]["entries"].as_array().unwrap();
    assert_eq!(
        entries[1]["declaration"]["condition"]["failure"],
        "wrong-type"
    );
    assert_eq!(
        entries[2]["declaration"]["condition"]["failure"],
        "not-symlink"
    );
}

#[test]
fn test_cli_cycle_is_distinct_from_a_missing_resource() {
    let project = Project::new("    requires: [{type: symlink, path: first}]\n");
    symlink("second", project.path().join("first"), false);
    symlink("first", project.path().join("second"), false);
    let (success, report) = project.json(&[]);
    assert!(!success);
    assert_eq!(
        report["resources"]["entries"][0]["inspection"]["failure"],
        "symlink-cycle"
    );
    assert_eq!(
        report["resources"]["entries"][0]["declaration"]["condition"]["failure"],
        "symlink-cycle"
    );
}

#[test]
fn test_resource_limit_reports_omissions_without_changing_ready() {
    let mut fields = "    requires:\n".to_owned();
    for _ in 0..140 {
        fields.push_str("      - {type: directory, path: .}\n");
    }
    let project = Project::new(&fields);
    let (success, report) = project.json(&[]);
    assert!(success);
    assert_eq!(report["ready"], true);
    assert_eq!(report["resources"]["complete"], false);
    assert_eq!(report["resources"]["omitted"], 12);
    assert_eq!(
        report["resources"]["entries"].as_array().unwrap().len(),
        128
    );
    assert_eq!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["check"] == "required-directory")
            .count(),
        140
    );
}

#[cfg(unix)]
#[test]
fn test_text_escapes_link_target_controls_and_json_preserves_them() {
    let project = Project::new("    requires: [{type: symlink, path: link}]\n");
    let name = "bad\u{1b}[2J\nname";
    symlink(name, project.path().join("link"), false);
    let (_, report) = project.json(&[]);
    assert!(report["resources"]["entries"][0]["inspection"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["target"] == name));
    let text = String::from_utf8(project.invoke(&["doctor"]).stdout).unwrap();
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("bad\\u{1b}[2J\\nname"));
}
