"""Fixed-version synthetic orchestration comparison, not a user or performance study."""
import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

FIXTURE = Path(__file__).with_name("comparison-fixture.py").resolve()


def invoke(arguments, root=None, **options):
    return subprocess.run(arguments, cwd=root, capture_output=True, text=True, timeout=30, **options)


def command(mode, root):
    return " ".join(shlex.quote(value) for value in (sys.executable, str(FIXTURE), mode, str(root)))


def wait_for(check):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError("comparison fixture timed out")


def config(path, name, modes, root, gate=False):
    if name == "process-compose":
        processes = {service: {"command": command(mode, root)} for service, mode in modes.items()}
        if gate:
            processes["producer"]["availability"] = {"restart": "exit_on_failure"}
            processes["consumer"].update({
                "depends_on": {"producer": {"condition": "process_completed_successfully"}},
                "availability": {"exit_on_end": True, "exit_on_skipped": True},
            })
        value = {"version": "0.5", "processes": processes}
    else:
        value = {"version": "1", "services": {service: {"command": command(mode, root),
                 "restart": {"policy": "never"}} for service, mode in modes.items()}}
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    return len(path.read_text().splitlines())


def tasks(root, binaries):
    results = {}
    for fail in (False, True):
        producer = "failed-producer" if fail else "producer"
        expected = ["producer-failed"] if fail else ["consumer-complete", "producer-complete"]
        for name in ("process-compose", "devd", "scripts"):
            case = root / f"t-{name}-{fail}"
            case.mkdir()
            if name == "scripts":
                arguments = [sys.executable, str(FIXTURE), "sequence", str(case), producer]
                lines = 0
            else:
                path = case / "config.yml"
                lines = config(path, name, {"producer": producer,
                               "consumer": "consumer" if name == "process-compose" else "wrapped-consumer"}, case, gate=True)
                if name == "process-compose":
                    arguments = [binaries[name], "--no-server", "--tui=false", "--disable-dotenv",
                                 "--log-file", str(case / "pc.log"), "--config", str(path), "up"]
                else:
                    arguments = [binaries[name], "start", "--config", str(path), "--color", "never"]
            started = time.monotonic()
            output = invoke(arguments, case)
            produced = sorted(path.name for path in case.glob("*-complete")) + sorted(path.name for path in case.glob("*-failed"))
            result = {"exit_code": output.returncode, "files": produced, "configuration_lines": lines,
                      "elapsed_seconds": round(time.monotonic() - started, 3)}
            assert produced == expected, (name, fail, result, output.stderr)
            if not fail:
                assert output.returncode == 0, output.stderr
            results[f"{name}-{'failure' if fail else 'success'}"] = result
    return results


class Live:
    def __init__(self, name, binary, root, mode="worker"):
        self.name, self.binary, self.root = name, binary, root
        self.child = None
        self.path = root / "config.yml"
        self.config_lines = 0 if name == "scripts" else config(self.path, name, {"worker": mode}, root)
        self.mode = mode
        self.log = (root / "supervisor.log").open("w")

    def start(self):
        (self.root / "finish").unlink(missing_ok=True)
        (self.root / "worker.json").unlink(missing_ok=True)
        if self.name == "process-compose":
            args = [self.binary, "--tui=false", "--disable-dotenv", "-U", "-u", str(self.root / "pc.sock"),
                    "--log-file", str(self.root / "pc.log"), "--config", str(self.path), "up"]
        elif self.name == "devd":
            args = [self.binary, "start", "--config", str(self.path), "--color", "never"]
        else:
            args = [sys.executable, str(FIXTURE), self.mode, str(self.root)]
        self.child = subprocess.Popen(args, cwd=self.root, stdout=self.log, stderr=self.log)
        wait_for(lambda: (self.root / "worker.json").exists())
        if self.name == "devd":
            wait_for(lambda: self.cli("wait", "--timeout", "100ms").returncode == 0)
        elif self.name == "process-compose":
            wait_for(lambda: self.cli("process", "list", "-o", "json").returncode == 0)

    def cli(self, *args):
        if self.name == "devd":
            return invoke([self.binary, *args, "--config", str(self.path), "--color", "never"], self.root)
        return invoke([self.binary, "-U", "-u", str(self.root / "pc.sock"), *args], self.root)

    def worker(self):
        return json.loads((self.root / "worker.json").read_text())

    def stop(self):
        if self.child is None:
            return
        (self.root / "finish").touch()
        self.child.wait(timeout=15)
        self.child = None

    def close(self):
        try:
            self.stop()
        finally:
            if self.child is not None and self.child.poll() is None:
                self.child.terminate()
                self.child.wait(timeout=10)
            self.log.close()


def loss(root, binaries):
    results = {}
    # Glue can reach the smaller synthetic outcome. It does not implement devd's
    # registered ownership, bounded declaration graph or report validator.
    for name in ("process-compose", "scripts"):
        case = root / f"l-{name}"
        case.mkdir()
        (case / "target").write_text("synthetic")
        (case / "resource").symlink_to("target")
        live = Live(name, binaries.get(name), case, "observer")
        try:
            live.start()
            (case / "target").unlink()
            wait_for(lambda: (case / "report.json").exists())
            live.stop()
            original = (case / "report.json").read_bytes()
            (case / "target").write_text("repaired")
            assert (case / "report.json").read_bytes() == original
            recipient = case / "recipient"
            recipient.mkdir()
            shutil.copyfile(case / "report.json", recipient / "report.json")
            (case / "target").unlink()
            (case / "resource").unlink()
            imported = json.loads((recipient / "report.json").read_bytes())
            assert imported["failure"] == "dangling-symlink"
            results[name] = {"recorded_and_transferred": True, "configuration_lines": live.config_lines,
                             "relationship_source": "handwritten fixture declaration",
                             "repair": "explicit fixture write; no ownership/replay contract"}
        finally:
            live.close()
    result = invoke([sys.executable, str(Path(__file__).with_name("recovery-smoke.py")),
                     "--binary", binaries["devd"]])
    assert result.returncode == 0, result.stderr
    results["devd"] = {"recorded_and_transferred": True, "scenario": result.stdout.strip(),
                       "relationship_source": "retained devd declaration graph",
                       "repair": "stopped-instance plan, registered ownership, receipt and stale-run guards"}
    return results


def worktrees(root, binaries):
    results = {}
    for name in ("process-compose", "devd", "scripts"):
        repo = root / f"w-{name}"
        repo.mkdir()
        for args in (["init", "-q"], ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                     "commit", "--allow-empty", "-qm", "fixture"]):
            assert invoke(["git", *args], repo).returncode == 0
        peer = root / f"p-{name}"
        assert invoke(["git", "worktree", "add", "--detach", str(peer)], repo).returncode == 0
        with ExitStack() as stack:
            a, b = (Live(name, binaries.get(name), path) for path in (repo, peer))
            stack.callback(a.close)
            stack.callback(b.close)
            a.start()
            b.start()
            initial_peer = b.worker()
            previous = a.worker()
            if name == "devd":
                old = json.loads(a.cli("identity", "--json").stdout)
                discovered = json.loads(invoke([binaries[name], "instances", "--json"], a.root).stdout)
                assert len(discovered["entries"]) == 2, discovered
            a.stop()
            a.start()
            assert previous["run_id"] != a.worker()["run_id"]
            assert initial_peer == b.worker(), "other worktree was affected"
            result = {"isolated": True, "new_run": True, "peer_unchanged": True,
                      "configuration_lines_per_instance": a.config_lines}
            if name == "devd":
                request = {"schema_version": 1, "id": "old-run", "operation": {"method": "restart", "service": "worker",
                           "target": {key: old[key] for key in ("instance_id", "run_id")}}}
                reply = invoke([binaries[name], "agent", "--stdio", "--allow", "restart", "--config", str(a.path)],
                               a.root, input=json.dumps(request) + "\n")
                response = json.loads(reply.stdout)
                assert response["error"]["code"] == "stale-run", response
                result["old_run_control"] = "Agent target rejected; ordinary CLI is endpoint-addressed"
            elif name == "process-compose":
                before = a.worker()
                assert a.cli("process", "restart", "worker").returncode == 0
                wait_for(lambda: a.worker()["pid"] != before["pid"])
                result["old_run_control"] = "endpoint-addressed restart reaches new run; caller UUID guard can be added"
            else:
                assert previous["run_id"] != a.worker()["run_id"]
                result["old_run_control"] = "handwritten UUID comparison rejects previous token"
            results[name] = result
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--process-compose", type=Path, required=True)
    parser.add_argument("--devd", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if os.name == "nt":
        parser.error("comparison uses Unix sockets; devd Windows validation is separate")
    if args.output.exists():
        parser.error("output already exists")
    binaries = {"process-compose": str(args.process_compose.resolve(strict=True)), "devd": str(args.devd.resolve(strict=True))}
    versions = {name: invoke([binary, "version" if name == "process-compose" else "--version"]).stdout.strip()
                for name, binary in binaries.items()}
    if "Version:        v1.122.0\n" not in versions["process-compose"]:
        parser.error("comparison requires Process Compose v1.122.0")
    with tempfile.TemporaryDirectory(prefix="dc-", dir="/tmp") as directory:
        root = Path(directory)
        results = {"task_gate": tasks(root, binaries), "resource_loss": loss(root, binaries),
                   "worktrees": worktrees(root, binaries)}
    report = {"schema": 1, "platform": sys.platform, "tools": versions,
              "fixture_lines": len(FIXTURE.read_text().splitlines()), "results": results,
              "limitations": ["Synthetic fixtures, not independent user feedback.",
                              "Configuration line counts depend on formatting; common fixture/harness glue also have cost.",
                              "Elapsed times are observations, not comparable performance measurements.",
                              "No setup/diagnosis time, RSS/CPU, containers, Windows comparison or value claims."]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(f"Passed all three synthetic comparisons; wrote {args.output}")


if __name__ == "__main__":
    main()
