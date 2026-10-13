"""Real MCP Inspector CLI interoperability; no model account or inference call."""
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

INSPECTOR = "@modelcontextprotocol/inspector@2.10.1"


def exercise(binary):
    npx = shutil.which("npx")
    if not npx:
        raise RuntimeError("Node >=22.19 and npm/npx are required")
    with tempfile.TemporaryDirectory(prefix="dmcp-", dir=None if os.name == "nt" else "/tmp") as directory:
        root = Path(directory)
        script = Path(__file__).resolve()
        command = " ".join(shlex.quote(s) for s in (Path(sys.executable).as_posix(), script.as_posix(), "--worker", root.as_posix()))
        config = root / "devd.yml"
        config.write_text(json.dumps({"version": "1", "services": {"worker": {"command": command, "restart": {"policy": "never"}}}}), encoding="utf-8")
        client_config = root / "client.json"

        def configure(*extra):
            client_config.write_text(json.dumps({"mcpServers": {"devd": {"command": binary, "args": ["mcp", "--stdio", *extra], "cwd": str(root)}}}), encoding="utf-8")

        def inspector(method, name=None, args=None, allow_error=False):
            options = [npx, "-y", INSPECTOR, "--cli", "--config", str(client_config), "--server", "devd", "--method", method, "--format", "json", "--quiet"]
            if method == "tools/list":
                options.append("--strict")
            if name:
                options += ["--tool-name", name, "--tool-args-json", json.dumps(args or {})]
            result = subprocess.run(options, cwd=root, capture_output=True, text=True, encoding="utf-8", timeout=120)
            if result.returncode and not allow_error:
                raise AssertionError(f"Inspector {method} {name}: {result.stderr}\n{result.stdout}")
            value = json.loads(result.stdout)
            return value

        def cli(*args, required=True):
            result = subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, encoding="utf-8", timeout=15)
            if required:
                assert result.returncode == 0, result.stderr
            return result

        def payload(value):
            # CLI JSON mode returns the protocol result under result.
            return value.get("result", value)

        def data(value):
            value = payload(value)
            assert not value.get("isError"), value
            return json.loads(value["content"][0]["text"])

        with (root / "supervisor.stderr").open("w", encoding="utf-8") as errors:
            child = subprocess.Popen([binary, "start", "--record-incidents"], cwd=root, stdout=subprocess.DEVNULL, stderr=errors)
            try:
                deadline = time.monotonic() + 15
                while cli("wait", "--timeout", "100ms", required=False).returncode:
                    assert child.poll() is None and time.monotonic() < deadline, "supervisor unavailable"
                    time.sleep(0.05)
                configure("--config", str(config))
                catalog = payload(inspector("tools/list"))
                assert len(catalog["tools"]) == 14, catalog
                identity = data(inspector("tools/call", "devd_identity"))
                target = {key: identity[key] for key in ("instance_id", "run_id")}
                status = data(inspector("tools/call", "devd_status"))
                assert status["services"]["worker"]["pid"]
                events = data(inspector("tools/call", "devd_events", {"tail":10}))
                assert events["entries"]
                assert data(inspector("tools/call", "devd_incidents_list"))["selected_run"] == identity["run_id"]
                denied = payload(inspector("tools/call", "devd_stop", {"target":target}, allow_error=True))
                assert denied["isError"] is True
                assert json.loads(denied["content"][0]["text"])["code"] == "permission-denied"
                configure("--config", str(config), "--allow", "restart")
                data(inspector("tools/call", "devd_restart", {"target":target,"service":"worker"}))
                assert data(inspector("tools/call", "devd_status"))["services"]["worker"]["pid"] != status["services"]["worker"]["pid"]
                report = root / "report.json"
                report.write_text(json.dumps(data(inspector("tools/call", "devd_export"))), encoding="utf-8")
                cli("stop")
                assert child.wait(timeout=15) == 0
                config.unlink()
                configure("--report", str(report))
                assert len(payload(inspector("tools/list"))["tools"]) == 1
                diagnosis = data(inspector("tools/call", "devd_diagnose"))
                assert diagnosis["source"] == "imported-report"
                assert diagnosis["current_health"] == "not-inspected-historical-evidence-only"
                print(f"{INSPECTOR}: discovery, live reads, denied stop, granted restart, export and isolated offline diagnosis passed")
            finally:
                (root / "finish-workers").touch()
                if child.poll() is None:
                    cli("stop", required=False)
                    try:
                        child.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/devd.exe" if os.name == "nt" else "target/debug/devd"))
    parser.add_argument("--worker", type=Path)
    args = parser.parse_args()
    if args.worker:
        deadline = time.monotonic() + 600
        while args.worker.exists() and not (args.worker / "finish-workers").exists() and time.monotonic() < deadline:
            time.sleep(0.05)
    else:
        exercise(str(args.binary.resolve(strict=True)))
