//! Cross-platform task lifecycle tests using the readiness suite's CLI ownership.
use super::*;
use std::io::Write;

fn fixture(name: &str) -> String {
    format!(
        "{} --ignored --exact {name} --nocapture",
        shell_words::quote(std::env::current_exe().unwrap().to_str().unwrap())
    )
}

fn task(name: &str, mode: &str) -> Value {
    json!({"kind":"task", "command":fixture("tasks::test_task_fixture"),
        "env":{"TASK_NAME":name,"TASK_MODE":mode}, "task-timeout":"10s"})
}

fn project(mode: &str) -> (Project, Value) {
    let project = Project::new(false, false);
    let document = json!({"version":"1","services": {
        "build":task("build",mode),
        "api":{"command":fixture("test_wait_worker"),"restart":{"policy":"never"},
            "depends-on":[{"service":"build","condition":"completed-successfully"}]},
        "worker":{"command":fixture("test_wait_worker"),"restart":{"policy":"never"}}
    }});
    write(&project, &document);
    (project, document)
}

fn write(project: &Project, document: &Value) {
    fs::write(
        project.root.path().join("devd.yml"),
        serde_json::to_vec_pretty(document).unwrap(),
    )
    .unwrap();
}

fn await_state(project: &Project, check: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let output = project.invoke(&["status", "--json"]);
        if output.status.success() {
            let state = serde_json::from_slice(&output.stdout).unwrap();
            if check(&state) {
                return state;
            }
        }
        assert!(
            Instant::now() < deadline,
            "task state timed out: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn count(project: &Project, name: &str) -> usize {
    fs::read_to_string(project.root.path().join(format!("runs-{name}")))
        .unwrap_or_default()
        .lines()
        .count()
}

fn release(project: &Project) {
    fs::write(project.root.path().join("release-build"), "yes").unwrap();
}

#[test]
fn test_task_completion_wait_restart_and_stale_success() {
    let (mut project, _) = project("gated");
    project.supervisor = Some(project.spawn(&["start"]));
    await_state(&project, |s| s["services"]["build"]["pid"].is_number());
    let report = outcome(
        &mut project.spawn(&["wait", "build", "--timeout", "100ms", "--json"]),
        "timed-out",
    );
    assert_eq!(report["services"]["build"]["requires_completed"], true);
    assert!(project.json(&["status", "--json"])["services"]["api"]["pid"].is_null());
    release(&project);
    let first = outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    assert_eq!(first["services"]["build"]["status"], "succeeded");
    assert!(first["services"]["build"]["pid"].is_null());
    let text = project.invoke(&["wait", "build", "worker"]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.lines().any(|line| line.starts_with("\"build\": ready")
            && line.contains("requires=completed-successfully,")),
        "{text}"
    );
    assert!(text.lines().any(|line| line.starts_with("\"worker\": ready") && line.contains("requires=running,")), "{text}");
    assert_eq!(
        project.json(&["explain", "build", "--json"])["conclusion"],
        "task-succeeded"
    );
    assert!(project.invoke(&["restart", "api"]).status.success());
    assert_eq!(
        count(&project, "build"),
        1,
        "consumer restart must reuse task completion"
    );
    fs::remove_file(project.root.path().join("release-build")).unwrap();
    assert!(project.invoke(&["restart", "build"]).status.success());
    let second = await_state(&project, |s| s["services"]["build"]["pid"].is_number());
    assert_ne!(
        second["services"]["build"]["event_generation"],
        first["services"]["build"]["generation"]
    );
    outcome(
        &mut project.spawn(&["wait", "build", "--timeout", "100ms", "--json"]),
        "timed-out",
    );
    // A new consumer attempt cannot reuse the previous successful generation.
    let mut restart = project.spawn(&["restart", "api"]);
    await_state(&project, |s| s["services"]["api"]["pid"].is_null());
    assert!(restart.child.try_wait().unwrap().is_none());
    release(&project);
    assert!(restart.finish().status.success());
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    assert_eq!(count(&project, "build"), 2);
    project.stop();
}

#[test]
fn test_task_only_stack_and_failure_do_not_use_completion_markers() {
    for mode in ["success", "fail", "fail-once"] {
        let (project, _) = project(mode);
        let mut consumer = task("consumer", "success");
        consumer["depends-on"] = json!([{"service":"build","condition":"completed-successfully"}]);
        write(
            &project,
            &json!({"version":"1", "services":{"build":task("build",mode),"consumer":consumer}}),
        );
        // A stale file is deliberately present; it is not a dependency signal.
        fs::write(project.root.path().join("producer-complete"), "old").unwrap();
        let output = project.invoke(&["start"]);
        assert_eq!(
            output.status.success(),
            mode == "success",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(count(&project, "build"), 1, "task retry defaults to never");
        assert_eq!(count(&project, "consumer"), usize::from(mode == "success"));
    }
}

#[test]
fn test_task_explicit_retry_then_success_and_budget_exhaustion() {
    for (mode, succeeds) in [("fail-once", true), ("fail", false)] {
        let (mut project, mut document) = project(mode);
        document["services"]["build"]["restart"] =
            json!({"policy":"on-failure","max-attempts":1,"initial-delay":"10ms"});
        write(&project, &document);
        if succeeds {
            project.start();
            let state = outcome(&mut project.spawn(&["wait", "--json"]), "ready");
            assert_eq!(state["services"]["build"]["status"], "succeeded");
            project.stop();
        } else {
            assert!(!project.invoke(&["start"]).status.success());
        }
        assert_eq!(count(&project, "build"), 2);
    }
}

#[test]
fn test_task_timeout_spawn_failure_and_stop_never_release_consumers() {
    for failure in ["timeout", "spawn", "stop"] {
        let (mut project, mut document) = project("gated");
        document["services"]["api"] = task("consumer", "success");
        document["services"]["api"]["depends-on"] =
            json!([{"service":"build","condition":"completed-successfully"}]);
        if failure == "timeout" {
            document["services"]["build"]["task-timeout"] = json!("200ms");
        }
        if failure == "spawn" {
            document["services"]["build"]["command"] = json!("./no-such-task-program");
        }
        write(&project, &document);
        if failure == "stop" {
            project.supervisor = Some(project.spawn(&["start"]));
            await_state(&project, |s| s["services"]["build"]["pid"].is_number());
            project.stop();
        } else {
            assert!(!project
                .invoke(&["start", "--persist-events"])
                .status
                .success());
            if failure == "timeout" {
                let events = project.json(&["events", "--stored", "--json"]);
                assert!(
                    events["entries"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|e| e["data"]["type"] == "task-timed-out"),
                    "{events}"
                );
                assert_eq!(
                    project.json(&["explain", "build", "--stored", "--json"])["conclusion"],
                    "task-timed-out"
                );
            }
        }
        assert_eq!(count(&project, "consumer"), 0);
    }
}

#[test]
fn test_task_reload_reruns_only_affected_tasks_and_consumers() {
    let (mut project, mut document) = project("success");
    document["services"]["unrelated"] = task("unrelated", "success");
    write(&project, &document);
    project.start();
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    let before = project.json(&["status", "--json"]);
    document["services"]["build"]["env"]["CHANGED"] = json!("yes");
    write(&project, &document);
    let plan = project.json(&["reload", "--dry-run", "--json"]);
    let result = project.json(&[
        "reload",
        "--apply",
        "--plan",
        plan["plan_id"].as_str().unwrap(),
        "--json",
    ]);
    assert_eq!(result["outcome"], "applied");
    let after = project.json(&["status", "--json"]);
    assert_eq!(after["services"]["build"]["status"], "succeeded");
    assert_ne!(
        before["services"]["api"]["event_generation"],
        after["services"]["api"]["event_generation"]
    );
    assert_eq!(
        before["services"]["unrelated"]["event_generation"],
        after["services"]["unrelated"]["event_generation"]
    );
    assert_eq!(count(&project, "build"), 2);
    assert_eq!(count(&project, "unrelated"), 1);
    project.stop();
}

#[test]
fn test_task_evidence_export_and_offline_diagnosis() {
    let (mut project, _) = project("success");
    project.start();
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    let coverage = project.json(&["coverage", "--online", "--json"]);
    let build = coverage["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "build")
        .unwrap();
    assert_eq!(build["kind"], "task");
    assert_eq!(build["runtime_status"], "succeeded");
    assert!(project
        .invoke(&["export", "--output", "report.json"])
        .status
        .success());
    let report: Value =
        serde_json::from_slice(&fs::read(project.root.path().join("report.json")).unwrap())
            .unwrap();
    assert_eq!(report["state"]["build"]["kind"], "task");
    project.stop();
    fs::remove_file(project.root.path().join("devd.yml")).unwrap();
    let report = project.json(&["diagnose", "report.json", "--json"]);
    assert!(report.to_string().contains("task-succeeded"), "{report}");
}

#[test]
fn test_task_dependency_recovery_is_explicit_and_waits_for_new_completion() {
    let (mut project, mut document) = project("gated");
    document["services"]["linked"] = document["services"]["api"].clone();
    document["services"]["linked"]["restart"] =
        json!({"policy":"on-failure","initial-delay":"10ms"});
    document["services"]["linked"]["restart-on-dep-recovery"] = json!(true);
    write(&project, &document);
    release(&project);
    project.start();
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    let before = project.json(&["status", "--json"]);
    fs::remove_file(project.root.path().join("release-build")).unwrap();
    assert!(project.invoke(&["restart", "build"]).status.success());
    let running = await_state(&project, |s| s["services"]["build"]["pid"].is_number());
    for name in ["api", "linked"] {
        assert_eq!(
            before["services"][name]["event_generation"],
            running["services"][name]["event_generation"],
            "task spawn must not trigger dependency recovery"
        );
    }
    release(&project);
    let after = await_state(&project, |s| {
        s["services"]["linked"]["event_generation"]
            != before["services"]["linked"]["event_generation"]
            && s["services"]["linked"]["status"] == "running"
    });
    assert_eq!(after["services"]["build"]["status"], "succeeded");
    assert_eq!(
        before["services"]["api"]["event_generation"], after["services"]["api"]["event_generation"],
        "ungranted consumer must remain in its existing generation"
    );
    project.stop();
}

#[test]
fn test_task_reload_kind_changes_update_readiness_and_dependency_condition() {
    let (mut project, mut document) = project("success");
    project.start();
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    for kind in ["service", "task"] {
        document["services"]["build"] = if kind == "service" {
            json!({"command":fixture("test_wait_worker"),"restart":{"policy":"never"}})
        } else {
            task("build", "success")
        };
        document["services"]["api"]["depends-on"][0]["condition"] = json!(if kind == "service" {
            "started"
        } else {
            "completed-successfully"
        });
        write(&project, &document);
        let plan = project.json(&["reload", "--dry-run", "--json"]);
        let result = project.json(&[
            "reload",
            "--apply",
            "--plan",
            plan["plan_id"].as_str().unwrap(),
            "--json",
        ]);
        assert_eq!(result["outcome"], "applied");
        let report = outcome(&mut project.spawn(&["wait", "--json"]), "ready");
        assert_eq!(
            report["services"]["build"]["requires_completed"],
            kind == "task"
        );
        let state = project.json(&["status", "--json"]);
        assert_eq!(state["services"]["build"]["kind"], kind);
        assert_eq!(
            state["services"]["build"]["pid"].is_number(),
            kind == "service"
        );
    }
    project.stop();
}

#[test]
fn test_task_reload_failure_stops_stack_without_automatic_retry() {
    let (mut project, mut document) = project("success");
    project.start();
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    document["services"]["build"]["env"]["TASK_MODE"] = json!("fail");
    document["services"]["build"]["restart"] =
        json!({"policy":"on-failure","max-attempts":3,"initial-delay":"1ms"});
    write(&project, &document);
    let plan = project.json(&["reload", "--dry-run", "--json"]);
    let output = project.invoke(&[
        "reload",
        "--apply",
        "--plan",
        plan["plan_id"].as_str().unwrap(),
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(!project.supervisor.take().unwrap().finish().status.success());
    assert_eq!(
        count(&project, "build"),
        2,
        "reload must preserve its no-automatic-recovery boundary"
    );
}

#[test]
fn test_task_completion_cleans_background_descendants() {
    let (mut project, _) = project("descendant");
    project.start();
    outcome(&mut project.spawn(&["wait", "--json"]), "ready");
    let child: u32 = fs::read_to_string(project.root.path().join("task-child"))
        .unwrap()
        .parse()
        .unwrap();
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(child)]),
        true,
    );
    assert!(
        system.process(sysinfo::Pid::from_u32(child)).is_none(),
        "task success preceded descendant cleanup"
    );
    project.stop();
}

#[test]
#[ignore = "subprocess fixture, invoked by task lifecycle tests"]
fn test_task_fixture() {
    let name = std::env::var("TASK_NAME").unwrap();
    let mode = std::env::var("TASK_MODE").unwrap();
    let path = format!("runs-{name}");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", std::process::id()).unwrap();
    drop(file);
    if mode == "descendant" {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "test_wait_worker", "--nocapture"])
            .spawn()
            .unwrap();
        fs::write("task-child", child.id().to_string()).unwrap();
        // The supervisor owns this descendant's process group/Job, including
        // cleanup after this task leader exits successfully.
        std::process::exit(0);
    }
    if mode == "gated" {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !Path::new(&format!("release-{name}")).exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }
    if mode == "fail"
        || (mode == "fail-once" && fs::read_to_string(path).unwrap().lines().count() == 1)
    {
        std::process::exit(7);
    }
}
