"""Verify release archive contents and provenance, optionally exercising the native binary."""
import argparse
import hashlib
import platform
from pathlib import Path
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import zipfile


TARGETS = {
    "x86_64-unknown-linux-gnu": ("linux", {"x86_64", "amd64"}),
    "aarch64-apple-darwin": ("darwin", {"arm64", "aarch64"}),
    "x86_64-pc-windows-msvc": ("win32", {"amd64", "x86_64"}),
}


def verify_archive(archive, version, revision, smoke=False):
    archive = Path(archive)
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("revision must be a full lowercase commit SHA")
    target = next((name for name in TARGETS if archive.name ==
                   f"devd-{name}.{'zip' if name.endswith('windows-msvc') else 'tar.gz'}"), None)
    if target is None:
        raise ValueError("unexpected release archive name")
    with archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    expected_checksum = f"{digest}  {archive.name}"
    if Path(str(archive) + ".sha256").read_text(encoding="utf-8").strip() != expected_checksum:
        raise ValueError("archive SHA-256 or checksum filename does not match")
    binary = "devd.exe" if target.endswith("windows-msvc") else "devd"
    expected = {binary, "LICENSE", "README.md", "README.zh-CN.md", "VERSION", "REVISION"}
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            entries = source.infolist()
            if len(entries) != len(expected) or {entry.filename for entry in entries} != expected:
                raise ValueError("archive must contain exactly the public release files")
            if any(entry.is_dir() or stat.S_IFMT(entry.external_attr >> 16) not in (0, stat.S_IFREG)
                   for entry in entries):
                raise ValueError("archive members must be regular files")
            contents = {entry.filename: source.read(entry) for entry in entries}
    else:
        with tarfile.open(archive, "r:gz") as source:
            entries = source.getmembers()
            if len(entries) != len(expected) or {entry.name for entry in entries} != expected:
                raise ValueError("archive must contain exactly the public release files")
            if any(not entry.isfile() for entry in entries):
                raise ValueError("archive members must be regular files")
            contents = {entry.name: source.extractfile(entry).read() for entry in entries}
    if contents["VERSION"] != f"devd {version}\n".encode():
        raise ValueError("archive VERSION does not match the release")
    if contents["REVISION"] != f"{revision}\n".encode():
        raise ValueError("archive REVISION does not match the source")
    if any(not contents[name] for name in expected):
        raise ValueError("release files must not be empty")
    if smoke:
        native_os, machines = TARGETS[target]
        if sys.platform != native_os or platform.machine().lower() not in machines:
            raise ValueError("recovery smoke must run on the archive's native OS and architecture")
        # Extract only the validated binary name, never archive-provided paths.
        with tempfile.TemporaryDirectory(prefix="devd-release-") as directory:
            executable = Path(directory) / binary
            executable.write_bytes(contents[binary])
            executable.chmod(0o700)
            actual = subprocess.check_output([str(executable), "--version"], text=True).strip()
            if actual != f"devd {version}":
                raise ValueError("packaged binary version does not match VERSION")
            root = Path(__file__).resolve().parents[1]
            subprocess.run([sys.executable, str(root / "examples/local-stack/smoke.py"),
                            "--binary", str(executable), "--duration", "5", "--restarts", "2"],
                           cwd=root, check=True)
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archives", type=Path, nargs="+")
    parser.add_argument("--version", required=True, help="Cargo version without the v prefix")
    parser.add_argument("--revision", required=True)
    parser.add_argument("--smoke", action="store_true", help="Run the packaged binary on its native host")
    args = parser.parse_args()
    for archive in args.archives:
        try:
            target = verify_archive(archive, args.version, args.revision, args.smoke)
        except (ValueError, OSError, tarfile.TarError, zipfile.BadZipFile, subprocess.SubprocessError) as error:
            raise SystemExit(f"cannot verify {archive}: {error}") from error
        print(f"Verified {archive} ({target}, devd {args.version}, {args.revision})")


if __name__ == "__main__":
    main()
