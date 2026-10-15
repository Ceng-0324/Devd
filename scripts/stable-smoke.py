"""Exercise v1 task gates, evidence gaps and authorized controls with a native binary."""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time


def wait_for(check):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError("stable workflow timed out")


class Agent:
    def __init__(self, binary, root, config):
        self.directory = tempfile.TemporaryDirectory(prefix="devd-agent-smoke-")
        self.output = (Path(self.directory.name) / "stdout").open("w+b")
        self.reader = (Path(self.directory.name) / "stdout").open("rb")
        self.errors = tempfile.TemporaryFile(mode="w+b")
        self.child = subprocess.Popen([binary, "agent", "--stdio", "--config", str(config),
                                      "--allow", "restart,stop", "--restart-budget", "generate=1",
                                      "--deduplicate"], cwd=root, stdin=subprocess.PIPE,
                                     stdout=self.output, stderr=self.errors)
        self.offset = 0

    def request(self, operation):
        self.child.stdin.write((json.dumps({"schema_version": 1, "id": "request",
                                          "operation": operation}) + "\n").encode())
        self.child.stdin.flush()

        def receive():
            self.reader.seek(self.offset)
            line = self.reader.readline()
            if line.endswith(b"\n"):
                self.offset += len(line)
                return json.loads(line)
            if self.child.poll() is not None:
                self.errors.seek(0)
                raise AssertionError(self.errors.read().decode(errors="replace"))
            return None

        return wait_for(receive)

    def close(self):
        try:
            self.child.stdin.close()
            self.child.wait(timeout=10)
        finally:
            if self.child.poll() is None:
                self.child.kill()
                self.child.wait(timeout=5)
            self.reader.close()
            self.output.close()
            self.errors.close()
            self.directory.cleanup()


def exercise(binary):
    with tempfile.TemporaryDirectory(prefix="dv1-", dir=None if os.name == "nt" else "/tmp") as directory:
        root = Path(directory)
        config = root / "devd.yml"
        script = Path(__file__).resolve()
        command = " ".join(shlex.quote(s) for s in (Path(sys.executable).as_posix(), script.as_posix(), "--worker"))
        services = {
            "generate": {"kind": "task", "command": command + " generate", "task-timeout": "15s"},
            "api": {"command": command + " api", "restart": {"policy": "never"},
                    "requires": [{"type": "file", "path": "input"}],
                    "depends-on": [{"service": "generate", "condition": "completed-successfully"}]},
        }
        document = {"version": "1", "services": services,
                    "profiles": {"review": {"services": {"api": {"env": {"REVIEW": "1"}}}}}}
        (root / "input").write_text("synthetic input", encoding="utf-8")
        (root / "release-task").touch()

        def write_config():
            config.write_text(json.dumps(document), encoding="utf-8")

        def cli(*args, code=0):
            result = subprocess.run([binary, *args, "--color", "never"], cwd=root,
                                    capture_output=True, text=True, encoding="utf-8", timeout=25)
            assert result.returncode == code, f"{args}: exit {result.returncode}: {result.stderr}\n{result.stdout}"
            return result

        def read(*args, code=0):
            return json.loads(cli(*args, "--json", code=code).stdout)

        child, agent = None, None
        with (root / "supervisor.stderr").open("w", encoding="utf-8") as errors:
            def start(*extra):
                nonlocal child
                child = subprocess.Popen([binary, "start", *extra], cwd=root,
                                         stdout=subprocess.DEVNULL, stderr=errors)
                wait_for(lambda: subprocess.run([binary, "identity", *extra, "--json"], cwd=root,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5).returncode == 0)
                return read("wait", *extra, "--timeout", "10s")

            def stop(*extra):
                nonlocal child
                cli("stop", *extra)
                assert child.wait(timeout=15) == 0
                child = None

            try:
                write_config()
                static = read("coverage")
                assert all(static["evidence"][c]["state"] == "unknown" for c in ("events", "logs", "incidents"))
                assert not (root / ".devd").exists(), "static coverage created runtime state"
                ready = start()
                assert ready["services"]["generate"]["status"] == "succeeded"
                assert ready["services"]["generate"]["pid"] is None
                coverage = read("coverage", "--online")
                assert all(coverage["evidence"][c]["state"] == "disabled" for c in ("events", "logs", "incidents"))
                assert any(f["code"] == "requires-not-monitored" for f in coverage["findings"])
                cli("export", "--output", "before.json")
                agent = Agent(binary, root, config)
                identity = agent.request({"method": "identity"})["data"]
                target = {k: identity[k] for k in ("instance_id", "run_id")}
                old = read("status")["services"]["generate"]["event_generation"]
                (root / "release-task").unlink()
                request = {"method": "restart", "service": "generate", "target": target, "operation_key": "rerun"}
                accepted = agent.request(request)
                assert accepted["ok"], accepted
                waiting = read("wait", "generate", "--timeout", "100ms", code=1)
                assert waiting["outcome"] == "timed-out", "stale completion released a new task"
                assert read("status")["services"]["generate"]["event_generation"] != old
                assert agent.request(request)["data"] == accepted["data"]
                denied = agent.request({**request, "operation_key": "another"})
                assert denied["error"]["code"] == "budget-exhausted", denied
                conflict = agent.request({**request, "service": "api"})
                assert conflict["error"]["code"] == "operation-key-conflict", conflict
                (root / "release-task").touch()
                read("wait", "--timeout", "10s")
                assert len((root / "runs-generate").read_text().splitlines()) == 2
                services["generate"]["env"] = {"REVISION": "2"}
                write_config()
                plan = read("reload", "--dry-run")
                result = read("reload", "--apply", "--plan", plan["plan_id"])
                assert result["outcome"] == "applied", result
                read("wait", "--timeout", "10s")
                assert len((root / "runs-generate").read_text().splitlines()) == 3
                cli("export", "--output", "after.json")
                stopped = agent.request({"method": "stop", "target": target, "operation_key": "stop"})
                assert stopped["data"]["result"]["outcome"] == "stopping", stopped
                assert child.wait(timeout=15) == 0
                child = None
                agent.close()
                agent = None
                start("--profile", "review")
                cli("export", "--profile", "review", "--output", "profile.json")
                stop("--profile", "review")
                config.unlink()
                (root / "input").unlink()
                compared = read("compare", "before.json", "after.json")
                assert compared["source"] == "offline-report-comparison"
                assert any(item["status"] == "unknown" for item in compared["comparability"])
                changed_profile = read("compare", "before.json", "profile.json")
                # The default profile is encoded as null in reports; the
                # comparator conservatively treats that absence as unknown.
                assert any(item["field"] == "profile" and item["status"] == "unknown"
                           for item in changed_profile["comparability"])
                assert read("diagnose", "before.json")["current_health"] == "not-inspected-historical-evidence-only"
                cli("compare", "before.json", "absent.json", code=1)
                cli("wait", "--timeout", "nonsense", code=2)
                # Old output is deliberately present, but cannot satisfy a task gate.
                (root / "fail-task").touch()
                services["consume"] = {"kind": "task", "command": command + " consume",
                    "depends-on": [{"service": "generate", "condition": "completed-successfully"}]}
                del services["api"]
                document.pop("profiles")
                write_config()
                cli("start", code=1)
                assert not (root / "consumer-ran").exists()
                print("Native v1 workflow passed: task gates/rerun/reload, default coverage gaps, bounded Agent retry/stop, offline comparison and failure blocking")
            finally:
                (root / "finish-workers").touch()
                if agent:
                    agent.close()
                if child and child.poll() is None:
                    subprocess.run([binary, "stop", "--profile", "review"], cwd=root,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
                    subprocess.run([binary, "stop"], cwd=root, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=10)
                    try:
                        child.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)


def worker(mode):
    root = Path.cwd()
    if mode == "generate":
        with (root / "runs-generate").open("a") as output:
            output.write("run\n")
        if (root / "fail-task").exists():
            raise SystemExit(7)
        wait_for(lambda: (root / "release-task").exists())
        (root / "generated").write_text("generated", encoding="utf-8")
    elif mode == "consume":
        (root / "consumer-ran").touch()
    else:
        assert (root / "generated").exists()
        deadline = time.monotonic() + 180
        while not (root / "finish-workers").exists() and time.monotonic() < deadline:
            time.sleep(0.05)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/devd.exe" if os.name == "nt" else "target/debug/devd"))
    parser.add_argument("--worker", choices=["generate", "api", "consume"])
    args = parser.parse_args()
    if args.worker:
        worker(args.worker)
    else:
        exercise(str(args.binary.resolve(strict=True)))
