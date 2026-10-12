"""Exercise native resource failures and retained evidence using one selected binary."""
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


SECRET = "PRIVATE-INCIDENT-SMOKE"


def wait_for(check, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}")


def worker(root, name):
    # The parent smoke owns this sentinel. Even failed supervisor cleanup cannot
    # leave these fixtures running indefinitely or dependent on a reused PID.
    print(SECRET, flush=True)
    deadline = time.monotonic() + 90
    while root.exists() and not (root / "finish-workers").exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    if root.exists():
        (root / f"{name}.done").touch()


def exercise(binary):
    script = Path(__file__).resolve()
    interpreter = shlex.quote(sys.executable.replace("\\", "/"))
    with tempfile.TemporaryDirectory(prefix="di-smoke-", dir=None if os.name == "nt" else "/tmp") as temporary:
        root = Path(temporary)
        config = root / "devd.yml"
        target = root / "target"
        target.write_text(SECRET, encoding="utf-8")
        # Failure to create a native link is a failed prerequisite, never a skip.
        (root / "resource").symlink_to("target")
        services = {}
        for name in ("worker", "downstream"):
            command = f"{interpreter} {shlex.quote(script.as_posix())} --worker {shlex.quote(root.as_posix())} {name}"
            services[name] = {"command": command, "restart": {"policy": "never"}}
        services["worker"].update({"requires": [{"type": "file", "path": "resource"}],
                                   "monitor-requires": True, "env": {"PRIVATE": SECRET}})
        services["downstream"]["depends-on"] = ["worker"]

        def write_config(selected):
            config.write_text(json.dumps({"version": "1", "services": selected}), encoding="utf-8")

        def cli(*arguments, required=True):
            result = subprocess.run([binary, *arguments, "--color", "never"], cwd=root,
                                    capture_output=True, text=True, encoding="utf-8", timeout=15)
            if required and result.returncode:
                raise AssertionError(f"devd {arguments}: {result.stderr}")
            return result

        def read(*arguments):
            return json.loads(cli(*arguments, "--json").stdout)

        def start():
            child = subprocess.Popen([binary, "start", "--record-incidents", "--color", "never"],
                                     cwd=root, stdout=subprocess.DEVNULL, stderr=errors)

            def connected():
                if child.poll() is not None:
                    raise AssertionError("supervisor exited during startup")
                return cli("identity", "--json", required=False).returncode == 0

            try:
                wait_for(connected, "supervisor identity")
            except BaseException:
                child.kill()
                child.wait(timeout=10)
                raise
            return child

        def capture(expected_run):
            entries = read("incidents", "list")["entries"]
            for entry in entries:
                assert entry["identity"]["run_id"] == expected_run
                if entry["trigger"] == "path-condition-changed":
                    return read("incidents", "show", entry["id"])["incident"]
            return None

        def private_free(value):
            text = json.dumps(value)
            assert SECRET not in text, "default evidence leaked private content"
            assert "--worker" not in text, "default evidence leaked command"

        def transferred(report, expected):
            # Only the report reaches the recipient. Hide the source state and
            # resource names as well, so accidental source access cannot pass.
            with tempfile.TemporaryDirectory(prefix="di-recipient-") as destination:
                recipient = Path(destination)
                shutil.copyfile(report, recipient / "evidence.json")
                hidden = []
                for source in (root / ".devd", root / "resource", target):
                    if source.exists() or source.is_symlink():
                        moved = source.with_name(source.name + ".hidden")
                        source.rename(moved)
                        hidden.append((source, moved))
                try:
                    def diagnose(*args, required=True):
                        result = subprocess.run([binary, "diagnose", "evidence.json", *args],
                                                cwd=recipient, capture_output=True, text=True, encoding="utf-8", timeout=15)
                        if required:
                            assert result.returncode == 0, result.stderr
                        return result

                    value = json.loads(diagnose("--json").stdout)
                    assert value["source"] == "imported-report"
                    assert value["current_health"] == "not-inspected-historical-evidence-only"
                    assert value["incident"] == expected
                    assert all(e["run_id"] == expected["beginning"]["capture"]["identity"]["run_id"]
                               for e in value["timeline"]["entries"])
                    text = diagnose().stdout
                    assert "not proven causation" in text or expected["resources"].get("relationships") is None
                    assert "Timeline" in text and "Deterministic explanations" in text
                    private_free(value)
                    assert list(recipient.iterdir()) == [recipient / "evidence.json"]
                    # A broken optional graph must preserve valid path/end facts.
                    damaged = json.loads((recipient / "evidence.json").read_text(encoding="utf-8"))
                    damaged["incident"]["recorded"]["incident"]["resources"]["relationships"] = {"schema_version": 999}
                    (recipient / "evidence.json").write_text(json.dumps(damaged), encoding="utf-8")
                    partial = json.loads(diagnose("--json").stdout)["incident"]
                    assert partial["resources"]["observations"] == expected["resources"]["observations"]
                    assert partial["end"] == expected["end"]
                    assert partial["resources"].get("relationships") is None and partial["complete"] is False
                    assert "invalid-recorded-relationships" in partial["diagnostics"]
                finally:
                    for source, moved in reversed(hidden):
                        moved.rename(source)

        child = None
        with (root / "supervisor.stderr").open("w", encoding="utf-8") as errors:
            try:
                write_config(services)
                child = start()
                cli("wait", "--timeout", "15s")
                identity = read("identity")
                before = read("status")
                impact = read("impact", "worker", "--require", "0")
                assert {v["service"] for v in impact["current"]["direct_consumers"]} == {"worker"}
                assert impact["current"]["downstream"] == [{"service": "downstream", "via_services": ["worker", "downstream"]}]
                assert impact["selected"]["inspection"]["failure"] is None
                target.unlink()
                broken = read("impact", "worker", "--require", "0")
                assert broken["selected"]["inspection"]["failure"] == "dangling-symlink"
                assert broken["current"]["downstream"] == impact["current"]["downstream"]
                recorded = wait_for(lambda: capture(identity["run_id"]), "path-loss incident")
                incident_id = recorded["id"]

                def finished():
                    value = read("incidents", "show", incident_id)["incident"]
                    return value if value["end"] is not None else None

                recorded = wait_for(finished, "follow-up window end")
                assert recorded["end"]["reason"] == "deadline"
                assert recorded["resources"]["atomic"] is False
                assert recorded["resources"]["source"] == "fresh-post-trigger-metadata-and-link-inspection"
                assert recorded["resources"]["observations"][0]["inspection"]["failure"] == "dangling-symlink"
                assert recorded["artifact_schema_version"] == 2
                assert "recorded-declared-relationships" in recorded["declared_impact"]
                relationships = recorded["resources"]["relationships"]
                assert relationships["atomic"] is False
                assert relationships["config_observed_at"] == recorded["beginning"]["capture"]["observed_at"]
                assert relationships["impacts"][0]["view"]["downstream"] == broken["current"]["downstream"]
                private_free(recorded)
                after = read("status")
                for name in services:
                    assert before["services"][name]["pid"] == after["services"][name]["pid"]
                    assert after["services"][name]["restart_count"] == 0
                assert cli("incidents", "list", "--stored", required=False).returncode != 0
                cli("stop")
                assert child.wait(timeout=15) == 0
                child = None
                config.unlink()
                target.write_text("REPAIRED-" + SECRET, encoding="utf-8")
                stored = read("incidents", "show", incident_id, "--stored")
                assert stored["incident"] == recorded, "offline evidence changed with today's filesystem"
                cli("export", "--stored", "--incident", incident_id, "--output", "report.json")
                output = root / "report.json"
                original = output.read_bytes()
                report = json.loads(original)
                assert report["schema_version"] == 3 and report["source"] == "stored"
                assert report["state"] == {} and report["identity"] is None
                assert report["incident"]["recorded"]["incident"] == recorded
                private_free(report)
                assert cli("export", "--stored", "--incident", incident_id,
                           "--output", "report.json", required=False).returncode != 0
                assert output.read_bytes() == original, "existing export was overwritten"
                assert not (root / ".devd/devd.yml/logs").exists()
                assert not (root / ".devd/devd.yml/events").exists()
                transferred(output, recorded)

                for name in services:
                    (root / f"{name}.done").unlink(missing_ok=True)
                write_config(services)
                child = start()
                cli("wait", "--timeout", "15s")
                second_run = read("identity")["run_id"]
                assert second_run != identity["run_id"]
                target.unlink()

                def open_capture():
                    entries = read("incidents", "list")["entries"]
                    assert all(e["identity"]["run_id"] == second_run for e in entries)
                    for entry in entries:
                        value = read("incidents", "show", entry["id"])["incident"]
                        if value["resources"] is not None:
                            assert value["end"] is None, "missed abrupt-shutdown test window"
                            return value
                    return None

                partial = wait_for(open_capture, "in-progress failed-process capture")
                child.kill()
                child.wait(timeout=15)
                child = None
                # Windows Job teardown kills descendants. Unix SIGKILL cannot;
                # release our own fixtures and wait for their explicit exit markers.
                (root / "finish-workers").touch()
                if os.name != "nt":
                    wait_for(lambda: all((root / f"{name}.done").exists() for name in services),
                             "smoke-owned workers after abrupt supervisor death")
                config.unlink()
                incomplete = read("incidents", "show", partial["id"], "--stored")["incident"]
                assert incomplete["collection_status"] == "incomplete"
                assert incomplete["complete"] is False and incomplete["end"] is None
                assert "missing-end-open-or-interrupted-capture" in incomplete["diagnostics"]
                private_free(incomplete)
                cli("export", "--stored", "--incident", partial["id"], "--output", "incomplete.json")
                transferred(root / "incomplete.json", incomplete)
            except BaseException:
                errors.flush()
                print((root / "supervisor.stderr").read_text(encoding="utf-8"), file=sys.stderr)
                raise
            finally:
                (root / "finish-workers").touch()
                if child is not None and child.poll() is None:
                    try:
                        cli("stop")
                        child.wait(timeout=15)
                    except (AssertionError, subprocess.SubprocessError):
                        child.kill()
                        child.wait(timeout=10)
        print("Native incident smoke passed: declared impact, dangling link, privacy, transferred diagnosis, abrupt boundary")


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--worker":
        worker(Path(sys.argv[2]), sys.argv[3])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path("target/debug/devd.exe" if os.name == "nt" else "target/debug/devd"))
    args = parser.parse_args()
    exercise(str(args.binary.resolve(strict=True)))


if __name__ == "__main__":
    main()
