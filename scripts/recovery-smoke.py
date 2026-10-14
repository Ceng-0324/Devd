"""Exercise v0.9 drift, reviewed recovery and evidence handoff with a native binary."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


SECRET = "PRIVATE-RECOVERY-SMOKE"


def wait_for(check, description):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}")


def exercise(binary):
    script = Path(__file__).resolve()
    with tempfile.TemporaryDirectory(prefix="dr-smoke-", dir=None if os.name == "nt" else "/tmp") as directory:
        root = Path(directory)
        state = root / ".devd/devd.yml"
        cache = state / "runtime/cache"
        config = root / "devd.yml"
        command = " ".join(shlex.quote(p) for p in
                           [sys.executable.replace("\\", "/"), script.as_posix(), "--worker", root.as_posix()])
        services = {"worker": {"command": command, "restart": {"policy": "never"},
                               "paths": {"CACHE": {"scope": "instance", "path": "cache", "cleanup": True},
                                         "SHARED": {"scope": "shared", "path": "shared"}}},
                    "downstream": {"command": command, "restart": {"policy": "never"},
                                   "depends-on": ["worker"]}}
        (root / "shared").mkdir()
        (root / "shared/user").write_text(SECRET, encoding="utf-8")
        recipe = {"version": 1, "service": "worker", "resources": ["CACHE"],
                  "steps": [{"id": "generate", "resource": "CACHE", "timeout": "5s",
                             "command": [sys.executable.replace("\\", "/"), script.as_posix(), "--generate"],
                             "ensures": [{"type": "file", "path": "generated"}]}]}
        (root / "recipe.yml").write_text(json.dumps(recipe), encoding="utf-8")

        def write_config():
            config.write_text(json.dumps({"version": "1", "services": services}), encoding="utf-8")

        def cli(*args, required=True):
            out = subprocess.run([binary, *args, "--color", "never"], cwd=root,
                                 capture_output=True, text=True, encoding="utf-8", timeout=20)
            if required:
                assert out.returncode == 0, f"{args}: {out.stderr}"
            return out

        def read(*args):
            return json.loads(cli(*args, "--json").stdout)

        child = None
        with (root / "stderr").open("w", encoding="utf-8") as errors:
            def start():
                nonlocal child
                child = subprocess.Popen([binary, "start", "--record-incidents", "--color", "never"],
                                         cwd=root, stdout=subprocess.DEVNULL, stderr=errors)

                def connected():
                    assert child.poll() is None, "supervisor exited during startup"
                    return cli("identity", "--json", required=False).returncode == 0

                wait_for(connected, "supervisor identity")
                cli("wait", "--timeout", "15s")
                return read("identity")

            def stop():
                nonlocal child
                cli("stop")
                assert child.wait(timeout=15) == 0
                child = None

            def recover():
                plan = read("recover", "--recipe", "recipe.yml", "--dry-run")
                assert plan["resources"][0]["present"] is False
                assert not cache.exists(), "preview rebuilt a resource"
                args = ("recover", "--recipe", "recipe.yml", "--apply", "--plan", plan["plan_id"])
                result = read(*args)
                assert result["outcome"] == "completed" and result["rebuilt"]
                assert result["steps"][0]["state"] == "completed"
                content = (cache / "generated").read_bytes()
                assert read(*args)["already_attempted"] is True
                assert (cache / "generated").read_bytes() == content, "replayed command"
                assert cli("identity", "--json", required=False).returncode != 0, "recovery auto-started"
                assert SECRET not in json.dumps([plan, result]), "recovery leaked command/output"
                return plan

            try:
                write_config()
                start()
                stop()
                shutil.rmtree(cache)  # Only this smoke's disposable temporary fixture.
                recover()
                link = root / "resource"
                link.symlink_to(cache / "generated")  # Missing Windows privilege is a failure.
                services["worker"].update({"requires": [{"type": "file", "path": "resource"}],
                                           "monitor-requires": True})
                write_config()
                identity = start()
                before = read("status")
                cli("baseline", "capture", "--service", "worker", "--resource", "requires:0",
                    "--output", "baseline.json")
                baseline_bytes = (root / "baseline.json").read_bytes()
                unchanged = cli("baseline", "check", "baseline.json", "--json", required=False)
                unchanged_value = json.loads(unchanged.stdout)
                # Windows metadata comparisons explicitly lack file identity.
                assert unchanged_value["outcome"] == ("unknown" if os.name == "nt" else "no-observed-change")
                alternate = root / "alternate"
                alternate.write_text(SECRET, encoding="utf-8")
                link.unlink()
                link.symlink_to(alternate)
                changed = cli("baseline", "check", "baseline.json", "--json", required=False)
                assert changed.returncode != 0 and json.loads(changed.stdout)["outcome"] == "changed"
                assert (root / "baseline.json").read_bytes() == baseline_bytes
                after = read("status")
                assert all(before["services"][name]["pid"] == after["services"][name]["pid"] for name in services)
                link.unlink()
                link.symlink_to(cache / "generated")
                (cache / "generated").unlink()

                def captured():
                    for entry in read("incidents", "list")["entries"]:
                        if entry["trigger"] == "path-condition-changed":
                            value = read("incidents", "show", entry["id"])["incident"]
                            if value["end"] is not None:
                                return value
                    return None

                incident = wait_for(captured, "completed path-loss incident")
                assert incident["resources"]["observations"][0]["inspection"]["failure"] == "dangling-symlink"
                assert incident["resources"]["relationships"]["impacts"][0]["view"]["downstream"][0]["service"] == "downstream"
                stop()
                shutil.rmtree(cache)
                plan = recover()
                repaired = read("incidents", "show", incident["id"], "--stored")["incident"]
                assert repaired == incident, "recovery rewrote historical evidence"
                assert (root / "shared/user").read_text(encoding="utf-8") == SECRET
                assert (root / "baseline.json").read_bytes() == baseline_bytes, "recovery accepted a baseline"
                cli("export", "--stored", "--incident", incident["id"], "--output", "report.json")
                assert SECRET not in (root / "report.json").read_text(encoding="utf-8")
                next_identity = start()
                assert next_identity["run_id"] != identity["run_id"]
                stop()
                assert cli("recover", "--recipe", "recipe.yml", "--apply", "--plan", plan["plan_id"],
                           required=False).returncode != 0, "old-run recovery was accepted"
                with tempfile.TemporaryDirectory(prefix="dr-recipient-") as destination:
                    recipient = Path(destination)
                    shutil.copyfile(root / "report.json", recipient / "report.json")
                    config.unlink()
                    state.parent.rename(root / "hidden-state")
                    link.unlink()
                    result = subprocess.run([binary, "diagnose", "report.json", "--json"], cwd=recipient,
                                            capture_output=True, text=True, encoding="utf-8", timeout=20)
                    assert result.returncode == 0, result.stderr
                    diagnosed = json.loads(result.stdout)
                    assert diagnosed["incident"] == incident
                    assert diagnosed["current_health"] == "not-inspected-historical-evidence-only"
                    assert list(recipient.iterdir()) == [recipient / "report.json"]
            except BaseException:
                errors.flush()
                print((root / "stderr").read_text(encoding="utf-8"), file=sys.stderr)
                raise
            finally:
                (root / "finish").touch()
                if child is not None and child.poll() is None:
                    try:
                        stop()
                    except (AssertionError, subprocess.SubprocessError):
                        child.kill()
                        child.wait(timeout=10)
        print("Native v0.9 smoke passed: drift, stopped recovery, no replay, new-run readiness, transferred original evidence")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--worker":
        root = Path(sys.argv[2])
        deadline = time.monotonic() + 90
        while root.exists() and not (root / "finish").exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        return
    if sys.argv[1:] == ["--generate"]:
        Path("generated").write_text(SECRET, encoding="utf-8")
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path("target/debug/devd.exe" if os.name == "nt" else "target/debug/devd"))
    args = parser.parse_args()
    exercise(str(args.binary.resolve(strict=True)))


if __name__ == "__main__":
    main()
