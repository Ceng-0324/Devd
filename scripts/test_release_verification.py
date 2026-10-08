"""Negative checks for release archive provenance and public-content boundaries."""
import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import subprocess
import tempfile
import unittest
import warnings
import zipfile

spec = importlib.util.spec_from_file_location("verify_release", Path(__file__).with_name("verify-release.py"))
verify_release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify_release)


class ReleaseVerificationTests(unittest.TestCase):
    revision = "a" * 40
    version = "0.6.0-alpha.1"

    def archive(self, directory, windows=False, changes=None, duplicate=False, symlink=False, fifo=False):
        target = "x86_64-pc-windows-msvc" if windows else "aarch64-apple-darwin"
        archive = Path(directory) / f"devd-{target}.{'zip' if windows else 'tar.gz'}"
        files = {
            "devd.exe" if windows else "devd": b"binary",
            "LICENSE": b"license", "README.md": b"English", "README.zh-CN.md": b"Chinese",
            "VERSION": f"devd {self.version}\n".encode(),
            "REVISION": f"{self.revision}\n".encode(),
        }
        files.update(changes or {})
        entries = list(files.items()) + ([("VERSION", files["VERSION"])] if duplicate else [])
        if windows:
            with zipfile.ZipFile(archive, "w") as output, warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                for name, data in entries:
                    entry = zipfile.ZipInfo(name)
                    if symlink and name == "LICENSE":
                        entry.external_attr = 0o120777 << 16
                    if fifo and name == "LICENSE":
                        entry.external_attr = 0o010600 << 16
                    output.writestr(entry, data)
        else:
            with tarfile.open(archive, "w:gz") as output:
                for name, data in entries:
                    entry = tarfile.TarInfo(name)
                    entry.size = len(data)
                    if symlink and name == "LICENSE":
                        entry.type = tarfile.SYMTYPE
                        entry.linkname = "README.md"
                    if fifo and name == "LICENSE":
                        entry.type = tarfile.FIFOTYPE
                    output.addfile(entry, io.BytesIO(data))
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        Path(str(archive) + ".sha256").write_text(f"{checksum}  {archive.name}\n", encoding="utf-8")
        return archive

    def test_archives_accept_exact_public_files_and_provenance(self):
        for windows in [False, True]:
            with self.subTest(windows=windows), tempfile.TemporaryDirectory() as directory:
                archive = self.archive(directory, windows)
                verify_release.verify_archive(archive, self.version, self.revision)

    def test_archives_reject_invalid_contents_even_with_valid_checksum(self):
        for windows in [False, True]:
            for options in [
                {"changes": {"VERSION": b"devd 0.5.0-alpha.1\n"}},
                {"changes": {"REVISION": ("b" * 40 + "\n").encode()}},
                {"changes": {"TECHNICAL_DESIGN.md": b"private"}},
                {"changes": {"../escape": b"outside"}},
                {"changes": {"LICENSE": b""}},
                {"duplicate": True}, {"symlink": True}, {"fifo": True},
            ]:
                with self.subTest(windows=windows, options=options), tempfile.TemporaryDirectory() as directory:
                    archive = self.archive(directory, windows, **options)
                    with self.assertRaises(ValueError):
                        verify_release.verify_archive(archive, self.version, self.revision)

    def test_cargo_package_excludes_local_only_documents(self):
        root = Path(__file__).resolve().parents[1]
        files = subprocess.check_output(
            ["cargo", "package", "--list", "--locked", "--allow-dirty"], cwd=root, text=True
        ).splitlines()
        self.assertTrue({"Cargo.toml", "README.md", "README.zh-CN.md"}.issubset(files))
        self.assertTrue({"TECHNICAL_DESIGN.md", "AGENTS.md"}.isdisjoint(files))

    def test_archives_reject_corrupt_bytes_or_checksum_filename(self):
        for windows in [False, True]:
            for corrupt in [False, True]:
                with self.subTest(windows=windows, corrupt=corrupt), tempfile.TemporaryDirectory() as directory:
                    archive = self.archive(directory, windows)
                    if corrupt:
                        with archive.open("ab") as output:
                            output.write(b"corruption")
                    else:
                        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
                        Path(str(archive) + ".sha256").write_text(f"{checksum}  wrong-name\n", encoding="utf-8")
                    with self.assertRaises(ValueError):
                        verify_release.verify_archive(archive, self.version, self.revision)


if __name__ == "__main__":
    unittest.main()
