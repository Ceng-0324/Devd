"""Synthetic inputs and explicit glue used by the fixed orchestration comparison."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid


def publish(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value), encoding="utf-8")
    temporary.replace(path)


def main():
    mode, directory = sys.argv[1:3]
    root = Path(directory)
    deadline = time.monotonic() + 20
    if mode in ("producer", "failed-producer"):
        time.sleep(0.1)
        (root / ("producer-complete" if mode == "producer" else "producer-failed")).write_text("ok")
        return 0 if mode == "producer" else 7
    if mode == "consumer":
        assert (root / "producer-complete").is_file(), "consumer ran before completion"
        (root / "consumer-complete").write_text("ok")
        return 0
    if mode == "sequence":
        producer = sys.argv[3]
        for step in (producer, "consumer"):
            result = subprocess.run([sys.executable, __file__, step, str(root)], timeout=20)
            if result.returncode:
                return result.returncode
        return 0
    if mode in ("worker", "observer"):
        publish(root / "worker.json", {"pid": os.getpid(), "run_id": str(uuid.uuid4())})
        while not (root / "finish").exists() and time.monotonic() < deadline:
            if mode == "observer" and not (root / "resource").exists():
                evidence = {"failure": "dangling-symlink", "link_target": os.readlink(root / "resource"),
                            "declared_downstream": ["consumer"], "attribution": "fixture-script-declaration"}
                publish(root / "report.json", evidence)
                mode = "worker"  # Preserve the first observation, even after repair.
            time.sleep(0.02)
        return 0
    raise ValueError(f"unknown mode: {mode}")


if __name__ == "__main__":
    raise SystemExit(main())
