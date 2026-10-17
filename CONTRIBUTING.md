# Contributing to devd

devd coordinates local development services. A change should make the boundary
between observation and control clearer, not widen it by accident.

## Getting started

Development happens on `dev`. Branch from `dev` and target it with development
pull requests; `main` is the release branch.

Use Rust 1.95 or newer, Python 3.11 or newer, and Git. From the repository
root, run the checks that cover the code you changed:

```bash
cargo fmt --all -- --check
cargo build --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo test --locked --doc
python3 -m unittest discover -s scripts -p 'test_*.py' -v
python3 scripts/generate-third-party-notices.py --check
```

`tests/README.md` explains platform fixtures and release checks. CI runs the
Rust suite and native smoke scenarios on Linux, macOS, and Windows. Do not
replace a native platform result with cross-compilation.

## Trying devd in a project

Use the [field testing guide](docs/FIELD_TESTING.md)
([简体中文](docs/FIELD_TESTING.zh-CN.md)) and
[Field feedback template](.github/ISSUE_TEMPLATE/field-feedback.yml).
Record the real job, setup friction, assistance, outcome, and whether you used
it again. A failed or abandoned trial is useful; anonymous projects are fine.
Maintainer trials and independent feedback use the same record with their
relationship stated. Tests and prepared examples remain engineering evidence.

## Changes and reviews

Open an issue before a behavior change that affects configuration, protocol,
permissions, or lifecycle semantics. Explain the user-visible problem, the
proposed boundary, and how a test will prove it. Small fixes and documentation
corrections can go straight to a pull request.

Keep commits focused and use Conventional Commits, for example
`feat(recovery): reject replay after an unknown command result`. Include tests
for core behavior and user-visible CLI changes. Update both READMEs when a
command or a safety boundary changes. Never claim a platform, client, or user
workflow was tested without the corresponding evidence.

The supervisor must not infer permission from observation. In particular,
diagnosis, file monitoring, incident capture, exports, Agent/MCP access,
recovery, cleanup, restart, and reload each have separate authorization
boundaries. Preserve run and instance guards; historical state and PIDs are
diagnostic evidence, never a right to control a process.

## Generated and private material

Run `python3 scripts/generate-third-party-notices.py` after a lockfile change
and commit the resulting `THIRD_PARTY_LICENSES.md`. `deny.toml` checks the
supported release targets against the crates.io registry and declared licenses.
Install cargo-deny 0.20.2 (`cargo install --locked cargo-deny@0.20.2`), then run
`cargo deny --locked check licenses sources`. The unmodified MPL-2.0 `colored`
crate's exact source archive is linked in the third-party notice; preserve that
notice when distributing binaries.

`AGENTS.md`, `TECHNICAL_DESIGN.md`, and `COMPETITIVE_RESEARCH.md` are local
private material. Do not add them to a pull request, Cargo package, archive, or
public documentation. Release packaging verifies this boundary.

## Maintenance scope

The latest published release is still alpha; the `dev` branch is preparing v1.0.0.
Compatibility expectations are defined in [the stability contract](docs/COMPATIBILITY.md).
Releases follow [SemVer and the version policy](RELEASING.md#version-policy):
public breaks require MAJOR, compatible features or deprecations MINOR, and
compatible fixes PATCH. Choose the version for the whole release; a commit does
not automatically trigger a bump.
Issues and pull requests remain the public planning record; an accepted issue
is not a promise of a release date or support response. Keep proposals concrete enough to reproduce and
review. For security-sensitive reports, follow `SECURITY.md` instead of opening
a public issue.
