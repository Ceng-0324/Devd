"""Exercise reviewed Node init with real npm, without installing project dependencies."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import signal
import tempfile
import time


def exercise(binary):
    with tempfile.TemporaryDirectory(prefix="di-", dir=None if os.name == "nt" else "/tmp") as directory:
        root = Path(directory)
        manifest = {"name": "devd-init-fixture", "private": True, "packageManager": "npm@10",
                    "scripts": {"build": "node fixture.cjs build", "dev": "node fixture.cjs dev"},
                    "workspaces": ["packages/*"]}
        (root / "package.json").write_text(json.dumps(manifest), encoding="utf-8")
        (root / "fixture.cjs").write_text("""const fs = require('node:fs');
const role = process.argv[2];
fs.writeFileSync(role + '-started', 'yes');
if (role === 'build') {
  const timer = setInterval(() => {
    if (fs.existsSync('release')) {
      clearInterval(timer);
      process.exit(fs.existsSync('fail') ? 1 : 0);
    }
  }, 20);
} else {
  if (!fs.existsSync('release')) process.exit(42);
  setInterval(() => {}, 1000);
}
""", encoding="utf-8")
        # Reading this as an env file would block on Unix; init must never do so.
        if os.name != "nt":
            os.mkfifo(root / ".env")
        else:
            (root / ".env").write_text("DO_NOT_READ=secret", encoding="utf-8")

        def cli(*args, ok=True, stdin=None):
            result = subprocess.run([binary, *args, "--color", "never"], cwd=root,
                                    input=stdin, capture_output=True, text=True, timeout=20)
            assert result.returncode == (0 if ok else 1), f"{args}: {result.stderr}\n{result.stdout}"
            return result

        scan = json.loads(cli("init", "--scan", "node", "--json").stdout)
        assert scan["package_manager"] == "npm" and not scan["complete"]
        selection = ("--scan", "node", "--build-script", "build", "--consumer-script", "dev")
        preview = json.loads(cli("init", *selection, "--json").stdout)
        assert not preview["published"] and not (root / "devd.yml").exists()
        assert not list(root.glob("*-started")) and not (root / ".devd").exists()
        if os.name != "nt":
            with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
                waiting = subprocess.Popen([binary, "init", "--scan", "node", "--interactive"],
                                           cwd=root, stdin=subprocess.PIPE, stdout=output, stderr=errors)
                try:
                    deadline = time.monotonic() + 10
                    while True:
                        output.seek(0)
                        if b"Build/task script" in output.read():
                            break
                        assert waiting.poll() is None and time.monotonic() < deadline
                        time.sleep(0.02)
                    waiting.send_signal(signal.SIGINT)
                    assert waiting.wait(timeout=5) != 0
                    assert not (root / "devd.yml").exists()
                finally:
                    waiting.stdin.close()
                    if waiting.poll() is None:
                        waiting.kill()
                    waiting.wait(timeout=5)
        # Confirmation comes after displaying the exact YAML and warnings.
        interactive = cli("init", "--scan", "node", "--interactive", stdin="build\ndev\ny\n")
        assert interactive.stdout.index("completed-successfully") < interactive.stdout.index("Write this configuration?")
        generated = (root / "devd.yml").read_bytes()
        assert not list(root.glob("*-started")) and not (root / ".devd").exists()
        refused = cli("init", *selection, "--yes", "--json", ok=False)
        assert not refused.stdout and (root / "devd.yml").read_bytes() == generated
        published = json.loads(cli("init", "--config", "second.yml", *selection, "--yes", "--json").stdout)
        assert published["configuration"] == preview["configuration"] and published["published"]
        assert (root / "second.yml").read_bytes() == generated
        cli("check")
        cli("graph")
        cli("doctor")
        for failure in (False, True):
            for name in ("release", "build-started", "dev-started", "fail"):
                (root / name).unlink(missing_ok=True)
            if failure:
                (root / "fail").touch()
            with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
                child = subprocess.Popen([binary, "start"], cwd=root, stdout=output, stderr=errors)
                try:
                    deadline = time.monotonic() + 15
                    while not (root / "build-started").exists():
                        assert child.poll() is None, "supervisor exited before build"
                        assert time.monotonic() < deadline, "build did not start"
                        time.sleep(0.02)
                    assert not (root / "dev-started").exists(), "consumer started before task completion"
                    (root / "release").touch()
                    if failure:
                        assert child.wait(timeout=15) != 0
                        assert not (root / "dev-started").exists()
                    else:
                        cli("wait", "--timeout", "10s")
                        deadline = time.monotonic() + 10
                        while not (root / "dev-started").exists():
                            assert time.monotonic() < deadline, "consumer did not run after readiness"
                            time.sleep(0.02)
                        cli("stop")
                        assert child.wait(timeout=15) == 0
                finally:
                    if child.poll() is None:
                        subprocess.run([binary, "stop"], cwd=root, capture_output=True, timeout=15)
                    if child.poll() is None:
                        child.kill()
                    child.wait(timeout=15)
    print("Reviewed init, no-execution scan, equivalent publication and npm task gates passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/devd.exe" if os.name == "nt" else "target/debug/devd"))
    args = parser.parse_args()
    exercise(str(args.binary.resolve()))


if __name__ == "__main__":
    main()
