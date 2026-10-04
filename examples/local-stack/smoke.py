"""Exercise the real example stack and emit a reproducible measurement report."""
import argparse
import json
import math
import os
import platform
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from urllib.request import urlopen
from urllib.error import URLError


def wait_for(check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("timed out waiting for stack condition")


def free_ports():
    # Reserve both at once so they cannot be equal. Services bind immediately
    # after release; a bind collision fails the smoke test rather than hiding it.
    with socket.socket() as first, socket.socket() as second:
        first.bind(("127.0.0.1", 0))
        second.bind(("127.0.0.1", 0))
        return first.getsockname()[1], second.getsockname()[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/devd"))
    parser.add_argument("--duration", type=float, default=60)
    parser.add_argument("--restarts", type=int, default=3)
    args = parser.parse_args()
    if not math.isfinite(args.duration) or args.duration <= 0 or not 0 <= args.restarts <= 8:
        parser.error("duration must be finite and positive; restarts must be between 0 and 8")
    binary = str(args.binary.resolve(strict=True))
    source = Path(__file__).resolve().parent
    api_port, web_port = free_ports()
    known_pids = set()
    with tempfile.TemporaryDirectory(prefix="devd-smoke-", dir="/tmp") as temporary:
        directory = Path(temporary)
        shutil.copy(source / "app.py", directory)
        config = (source / "devd.yml").read_text().replace("8731", str(api_port)).replace("8732", str(web_port))
        (directory / "devd.yml").write_text(config)

        def cli(*arguments):
            return subprocess.run([binary, *arguments, "--color", "never"], cwd=directory,
                                  capture_output=True, text=True, timeout=15, check=True).stdout

        def snapshot():
            if supervisor.poll() is not None:
                raise AssertionError(f"supervisor exited: {supervisor.returncode}")
            try:
                value = json.loads(cli("status", "--json"))
            except subprocess.CalledProcessError:
                return None
            known_pids.update(s["pid"] for s in value["services"].values() if s["pid"])
            return value["services"]

        def healthy():
            services = snapshot()
            expected = {"api": "healthy", "web": "healthy", "worker": "running"}
            return services if services and all(services[name]["status"] == status and services[name]["pid"] for name, status in expected.items()) else None

        def jobs():
            try:
                with urlopen(f"http://127.0.0.1:{web_port}/status", timeout=2) as response:
                    return json.load(response)["completed_jobs"]
            except (URLError, OSError):
                return 0

        cli("check")
        samples = []
        started = time.monotonic()
        with (directory / "output.log").open("w+") as output:
            supervisor = subprocess.Popen([binary, "start", "--color", "never"], cwd=directory, stdout=output, stderr=output)
            try:
                initial = wait_for(healthy)
                startup_seconds = time.monotonic() - started
                wait_for(lambda: jobs() > 0)
                with urlopen(f"http://127.0.0.1:{web_port}/", timeout=2) as response:
                    assert b"Your local stack is running" in response.read()
                for _ in range(args.restarts):
                    before = wait_for(healthy)["api"]["pid"]
                    cli("restart", "api")
                    after = wait_for(healthy)
                    assert after["api"]["pid"] != before
                    assert after["worker"]["pid"] == initial["worker"]["pid"]
                    wait_for(lambda: jobs() > 0)
                before = wait_for(healthy)["api"]["pid"]
                os.kill(before, signal.SIGKILL)
                wait_for(lambda: (value := healthy()) and value["api"]["pid"] != before)
                wait_for(lambda: jobs() > 0)
                # Trigger the configured health policy, then restore the input.
                before = wait_for(healthy)["api"]["pid"]
                (directory / "unhealthy").touch()
                wait_for(lambda: (value := snapshot()) and value["api"]["restart_count"] >= args.restarts + 2)
                (directory / "unhealthy").unlink()
                wait_for(lambda: (value := healthy()) and value["api"]["pid"] != before)
                wait_for(lambda: jobs() > 0)
                jobs_before_soak = jobs()
                deadline = time.monotonic() + args.duration
                while time.monotonic() < deadline:
                    assert healthy()
                    stats = subprocess.check_output(["ps", "-o", "pcpu=,rss=", "-p", str(supervisor.pid)], text=True, timeout=5).split()
                    samples.append({"cpu_percent": float(stats[0]), "rss_kib": int(stats[1])})
                    time.sleep(min(1, max(0, deadline - time.monotonic())))
                assert "job completed" in cli("logs", "worker")
                wait_for(lambda: jobs() > jobs_before_soak)
                cli("stop")
                assert supervisor.wait(timeout=15) == 0
                state = json.loads((directory / ".devd/devd.yml/services.json").read_text())
                assert all(s["pid"] is None and s["status"] == "stopped" for s in state["services"].values())
                for pid in known_pids:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        continue
                    raise AssertionError(f"fixture PID remains after shutdown: {pid}")
                print(json.dumps({"platform": platform.platform(), "binary_version": cli("--version").strip(),
                                  "duration_seconds": args.duration, "manual_restarts": args.restarts,
                                  "startup_seconds": round(startup_seconds, 3), "samples": len(samples),
                                  "supervisor_max_rss_kib": max(s["rss_kib"] for s in samples),
                                  "supervisor_max_ps_cpu_percent": max(s["cpu_percent"] for s in samples)}, indent=2))
            except BaseException:
                output.flush()
                print((directory / "output.log").read_text()[-12000:])
                raise
            finally:
                if supervisor.poll() is None:
                    supervisor.terminate()
                    try:
                        supervisor.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        supervisor.kill()
                        supervisor.wait()
                        for pid in known_pids:
                            try:
                                os.killpg(pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass


if __name__ == "__main__":
    main()
