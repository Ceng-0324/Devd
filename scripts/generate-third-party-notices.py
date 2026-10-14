"""Build a deterministic third-party notice from the locked three-platform graph."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
FALLBACKS = {
    ("objc2-core-foundation", "0.3.2"): ("objc2", "objc2.txt"),
    ("objc2-io-kit", "0.3.2"): ("objc2", "objc2.txt"),
    ("rmcp", "3.5.1"): ("rmcp", "rmcp-Apache-2.0.txt"),
}


def notices():
    packages = {}
    selected = set()
    for target in TARGETS:
        result = subprocess.check_output(
            ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
            text=True,
        )
        graph = json.loads(result)
        packages.update({package["id"]: package for package in graph["packages"]})
        selected.update(node["id"] for node in graph["resolve"]["nodes"])
    lines = [
        "# Third-party notices", "",
        "This file lists registry crates in the locked dependency graph for the Linux x86_64,",
        "macOS arm64 and Windows x86_64 MSVC builds, including build and test dependencies.",
        "Some listed crates are not linked into a particular binary. License expressions are",
        "the crates' declarations, not a claim that every alternative license was selected.",
        "License files below preserve the published crate text with normalized LF newlines;",
        "where a crate omitted one, the pinned upstream source is identified explicitly.",
        "The devd code itself is licensed separately under LICENSE.", "",
    ]
    texts = {}
    entries = []
    for package in sorted((packages[id] for id in selected if packages[id]["source"]),
                          key=lambda item: (item["name"], item["version"])):
        root = Path(package["manifest_path"]).parent
        license_files = sorted(path for path in root.rglob("*") if path.is_file() and
                               path.name.lower().startswith(("license", "copying", "notice", "copyright")))
        fallback = FALLBACKS.get((package["name"], package["version"]))
        if not license_files and fallback is None:
            raise RuntimeError(f"no license text for {package['name']} {package['version']}")
        if not package["license"]:
            raise RuntimeError(f"no license expression for {package['name']} {package['version']}")
        references = []
        for path in license_files:
            value = path.read_text(encoding="utf-8")
            digest = hashlib.sha256(value.encode()).hexdigest()
            texts.setdefault(digest, (value, []))[1].append(f"{package['name']} {package['version']}: {path.relative_to(root).as_posix()}")
            references.append(digest)
        if fallback:
            source_key, fallback_file = fallback
            source = json.loads(Path("licenses/upstream/sources.json").read_text())[source_key]
            path = Path("licenses/upstream") / fallback_file
            if source["file"] != fallback_file or not path.is_file():
                raise RuntimeError(f"missing pinned upstream license for {package['name']}")
            value = path.read_text(encoding="utf-8")
            digest = hashlib.sha256(value.encode()).hexdigest()
            texts.setdefault(digest, (value, []))[1].append(f"{package['name']} {package['version']}: {fallback_file} ({source['source']})")
            references.append(digest)
        entries.append((package, references))
    for package, references in entries:
        lines += [f"## {package['name']} {package['version']}", "",
                  f"Declared license: {package['license']}",
                  f"Registry source: {package['source']}",
                  f"Source archive: https://crates.io/api/v1/crates/{package['name']}/{package['version']}/download",
                  "License texts: " + ", ".join(f"[#{digest}](#{digest})" for digest in references), ""]
        if package["name"].startswith("objc2-"):
            lines += ["Distribution alternative selected: Apache-2.0; its complete text is included below.", ""]
    lines += ["# License texts", ""]
    for digest, (value, sources) in sorted(texts.items()):
        lines += [f"## {digest}", "", "Referenced by:"]
        lines += [f"- {source}" for source in sources]
        lines += ["", "```text", value, "```", ""]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    expected = notices()
    output = Path("THIRD_PARTY_LICENSES.md")
    if args.check:
        if not output.is_file() or output.read_text(encoding="utf-8") != expected:
            raise SystemExit("THIRD_PARTY_LICENSES.md is stale; regenerate it")
    else:
        output.write_text(expected, encoding="utf-8", newline="\n")
        print(f"Wrote {output} ({len(expected)} bytes)")


if __name__ == "__main__":
    main()
