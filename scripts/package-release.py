"""Test and package the native release binary, recording exact source provenance."""
import hashlib
import argparse
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"])
    parser.add_argument("--output-dir", type=Path, default=Path("target/release-artifacts"))
    args = parser.parse_args()
    target = args.target
    windows = target.endswith("windows-msvc")
    expected_os = {"x86_64-unknown-linux-gnu": "linux", "aarch64-apple-darwin": "darwin", "x86_64-pc-windows-msvc": "win32"}
    if sys.platform != expected_os[target]:
        raise SystemExit("release artifacts must be tested on their native OS")
    binary = Path("target") / target / "release" / ("devd.exe" if windows else "devd")
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise SystemExit("invalid git revision")
    if Path("REVISION").exists() and Path("REVISION").read_text().strip() != revision:
        raise SystemExit("REVISION does not match the checked-out source")
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], text=True).strip():
        raise SystemExit("commit tracked source changes before packaging a release")
    version = subprocess.check_output([str(binary.resolve()), "--version"], text=True).strip()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    version_path = args.output_dir / "VERSION"
    revision_path = args.output_dir / "REVISION"
    version_path.write_text(version + "\n", encoding="utf-8")
    revision_path.write_text(revision + "\n", encoding="utf-8")
    subprocess.run([sys.executable, "examples/local-stack/smoke.py", "--binary", str(binary),
                    "--duration", "5", "--restarts", "2"], check=True)
    files = [(binary, binary.name)] + [(Path(name), name) for name in (
        "LICENSE", "README.md", "README.zh-CN.md")]
    files.extend([(version_path, "VERSION"), (revision_path, "REVISION")])
    archive = args.output_dir / f"devd-{target}.{'zip' if windows else 'tar.gz'}"
    if windows:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for source, name in files:
                output.write(source, name)
        with zipfile.ZipFile(archive) as output:
            assert output.testzip() is None
            assert set(output.namelist()) == {name for _, name in files}
    else:
        with tarfile.open(archive, "w:gz") as output:
            for source, name in files:
                output.add(source, arcname=name)
        with tarfile.open(archive) as output:
            assert set(output.getnames()) == {name for _, name in files}
    with archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    Path(str(archive) + ".sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    print(f"Verified {archive} ({version}, {revision})")


if __name__ == "__main__":
    main()
