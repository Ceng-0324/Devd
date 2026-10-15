"""Capture legacy reports from a verified release binary, never the current writer."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time


def capture(binary, output, revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("revision must be the verified full release SHA")
    version = subprocess.check_output([binary, "--version"], text=True, timeout=15).strip()
    assert version == "devd 0.9.0-alpha.1", version
    output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="devd-compat-", dir="/tmp" if os.name != "nt" else None) as directory:
        root = Path(directory)
        script = Path(__file__).resolve()
        command = " ".join(shlex.quote(s) for s in (Path(sys.executable).as_posix(), script.as_posix(), "--worker"))
        config = {"version": "1", "services": {"worker": {"command": command, "restart": {"policy": "always"}}}}
        (root / "devd.yml").write_text(json.dumps(config), encoding="utf-8")

        def cli(*args):
            result = subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, timeout=20)
            assert result.returncode == 0, result.stderr
            return result.stdout

        child = subprocess.Popen([binary, "start", "--persist-events", "--record-incidents"], cwd=root,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 15
            while subprocess.run([binary, "wait", "--timeout", "100ms"], cwd=root,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5).returncode:
                assert child.poll() is None and time.monotonic() < deadline
                time.sleep(0.05)
            cli("export", "--output", str(output / "live.json"))
            (root / "crash").touch()
            deadline = time.monotonic() + 20
            while True:
                entries = json.loads(cli("incidents", "list", "--json"))["entries"]
                if entries:
                    incident = json.loads(cli("incidents", "show", entries[0]["id"], "--json"))["incident"]
                    if incident["end"] is not None:
                        break
                assert time.monotonic() < deadline
                time.sleep(0.05)
            cli("export", "--incident", incident["id"], "--output", str(output / "incident.json"))
            cli("stop")
            assert child.wait(timeout=15) == 0
            cli("export", "--stored", "--run", "latest", "--output", str(output / "stored.json"))
        finally:
            (root / "finish").touch()
            if child.poll() is None:
                subprocess.run([binary, "stop"], cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
                child.wait(timeout=15)
    manifest = {"version": version, "revision": revision, "platform": sys.platform,
                "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
                "reports": {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(output.glob('*.json'))},
                "notes": "Unmodified exports from a synthetic temporary project; source paths and PIDs are historical, not executable inputs."}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--revision")
    parser.add_argument("--worker", action="store_true")
    args = parser.parse_args()
    if args.worker:
        deadline = time.monotonic() + 60
        while not Path("finish").exists() and time.monotonic() < deadline:
            if Path("crash").exists():
                Path("crash").unlink()
                sys.exit(7)
            time.sleep(0.05)
    else:
        if not args.binary or not args.output or not args.revision:
            parser.error("--binary, --output and verified --revision are required")
        capture(str(args.binary.resolve(strict=True)), args.output.resolve(), args.revision)
