# Release checklist

The release workflow is manual and uploads run-scoped artifacts. It does not
create a tag, GitHub Release, or crates.io publication. Publishing is a separate
action after the source revision, checks, and artifacts have been reviewed.

## Version policy

devd follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).
Classify the release against the public interfaces in the bilingual
[compatibility contract](docs/COMPATIBILITY.md), using the highest-impact change
since the previous release:

| Change | Next stable version after `1.2.3` |
| --- | --- |
| Incompatible change to a documented public interface or removal of a deprecated interface | `2.0.0` (MAJOR; reset MINOR and PATCH) |
| Backward-compatible functionality or public-interface deprecation | `1.3.0` (MINOR; reset PATCH) |
| Backward-compatible bug fix | `1.2.4` (PATCH) |

Before `1.0.0`, interfaces were experimental; `1.0.0` starts the stable public
contract. Internal refactoring, tests and documentation corrections do not by
themselves require a version bump. Conventional Commit types describe commits;
choose one version for the combined release, not one bump per commit.

Use `X.Y.Z-alpha.N`, `X.Y.Z-beta.N` or `X.Y.Z-rc.N` when publishing a prerelease,
and mark it as a prerelease on GitHub. Increment the numeric suffix within each
stage; a prerelease sorts below its corresponding stable version. Unpublished
candidate builds may use the intended stable version but must identify their
full source SHA and must not be described as a published release.

`Cargo.toml` is the version source; keep the root package entry in `Cargo.lock`,
CLI version, changelog, release notes, archive `VERSION` and tag `vX.Y.Z` (with
any prerelease suffix) consistent. Build metadata such as `+sha` does not change
SemVer precedence; record provenance in `REVISION` instead. Never replace an
existing release tag or its published contents: publish a new version for fixes.
YAML, evidence schemas and MCP protocol versions are separate compatibility
dimensions; changing their numbers does not exempt a public break from MAJOR.

## Release steps

1. Confirm the Cargo version, changelog, and both READMEs describe the intended
   release and follow the version policy above. All prereleases must be marked
   as prereleases on GitHub.
2. Run `cargo fmt --all -- --check`,
   `cargo clippy --locked --all-targets -- -D warnings`,
   `cargo test --locked --all-targets`,
   `python3 -m unittest discover -s scripts -p 'test_*.py' -v`, and
   `cargo package --locked`.
   Also run `cargo deny --locked check licenses sources` using cargo-deny 0.20.2
   and `python3 scripts/generate-third-party-notices.py --check`. Regenerate the
   third-party notice after dependency changes; review the pinned upstream files
   in `licenses/upstream/` when a published crate omits its license text.
   Verify that local-only documents are absent from `cargo package --list`.
   For a crates.io release, also run `cargo publish --dry-run --locked` and
   confirm the package name, owners, README, license, dependency notices and
   public file list before publishing. After publication, verify the version
   from an empty Cargo root with:

   ```bash
   cargo install --registry crates-io --version '=X.Y.Z' --locked \
     --root <temporary-install-root> devd
   ```
3. Push the release commit and require successful Linux/macOS/Windows CI on that
   exact SHA, including the three-service recovery smoke test and native Windows
   job, probe, control, persistence, and supervisor-death tests. Filesystem/reload coverage also requires
   dangling/cyclic symlinks, replacement, permissions, monitor cancellation,
   invalid/stale reload plans, partial failures, competing controls, and stop
   preemption. Native Windows filesystem tests require symlink creation privileges
   and `icacls.exe`; they must not silently skip unsupported test setup. Cross-compilation
   is useful during development but does not replace native execution. Check
   `top` in an interactive Windows console (quit, restart, cancel/confirm stop)
   before the first Windows release; headless CI cannot validate console rendering.
   v0.7 additionally requires simultaneous worktree discovery, mapped environments
   and TCP readiness, Agent restart/reload/export/stop/clean without affecting the
   other instance, rejection of old-run controls, cleanup shared-path protection
   from foreign working directories, recovery-aware explanations, and bounded
   final readiness output under real pipe backpressure. Run the binding and
   cooperative shutdown fixture regressions as part of the same native suite.
   v0.8 requires resource-chain/declared-impact coverage, retained single-run
   export, incident correlation/retention and writer failures, recorded-evidence
   isolation, missing configuration, repaired targets, partial tails and safe
   reader leases. Run `python scripts/incident-smoke.py` against the selected
   binary: it reproduces a dangling target, verifies unchanged service PIDs,
   exports the original evidence offline, and checks the incomplete boundary
   after abrupt supervisor death. Symlink privileges are mandatory on Windows.
   v0.9 also requires retained relationship validation, transferred-report
   diagnosis without the source project, pinned official MCP Inspector
   interoperability, drift comparison and reviewed stopped-instance recovery.
   Run `python scripts/recovery-smoke.py` to cover baseline drift, original
   incident evidence, shared-resource protection, replay/run guards, new-run
   readiness and transfer to an empty recipient directory. Record fixed-version
   comparison separately from field trials, as described in VALIDATION.md;
   synthetic engineering success is not user-value evidence. Use the shared
   [field testing contract](docs/FIELD_TESTING.md) for maintainer trials in real
   projects and independent developer trials, labeled separately. Record failed
   or abandoned attempts too; no fixed participant count, duration, or requirement
   to try every feature is a release gate.
   v1.0 additionally requires `python scripts/stable-smoke.py` for integrated
   task/coverage/comparison/Agent behavior, frozen v0.9 report compatibility,
   and `python scripts/mcp-smoke.py` for official-client opt-in/retry behavior.
   Review `docs/COMPATIBILITY.md` and its Chinese version for stable interfaces,
   exit meanings and upgrade boundaries. Record independent feedback as unverified
   when absent. Rerun the fixed Process Compose v1.122.0 comparison for native task
   gates; keep unsuccessful/unknown observations in its output.
   v1.1 additionally requires `python scripts/init-smoke.py` with Node/npm for
   bounded read-only scanning, reviewed/no-clobber generation and real task
   completion/failure gates. Run the shared timeout/reload regressions too;
   archive validation invokes the same init smoke against the extracted binary.
4. Trigger **Release artifacts** from `dev` for a candidate (or `main` for a release) with the full source commit SHA in
   its required `revision` input. The workflow verifies the checkout and tests
   the archived release binary on each target platform.
   The packaging entry point is `python scripts/package-release.py <target>`;
   it requires committed tracked sources and an already-built native release
   binary, checks its version against Cargo.toml, writes archives/checksums under
   `target/release-artifacts/`, then verifies the archive and runs the stack, incident, reviewed-recovery, v1 workflow and MCP
   smokes using its extracted binary. Python 3.11+ and Node 22.19+ with npm are required.
   It never publishes them. The thirteen-file allowlist excludes local-only design
   and agent documents.
5. Download the Linux x86_64 and macOS arm64 `.tar.gz` artifacts and Windows
   x86_64 MSVC `.zip`. Verify each archive with `shasum -a 256 -c <file>.sha256`
   (or compare PowerShell `Get-FileHash -Algorithm SHA256` against its checksum).
   Inspect the contents: binary (`devd.exe` on Windows), license, both READMEs, bilingual command guides, usage notes and compatibility contracts under `docs/`,
   `THIRD_PARTY_LICENSES.md`, `VERSION`, and `REVISION`; confirm the latter two match the intended release.
   Local macOS testing does not replace validation of the other artifacts.
   Automate these checks for downloaded archives with:

   ```bash
   python3 scripts/verify-release.py <archive> [<archive> ...] \
     --version 1.0.0 --revision <full-40-character-SHA>
   ```

   Keep each checksum beside its archive. Add `--smoke` for a single native
   archive to verify the executable version and repeat recovery/incident testing.
   The packaging workflow already does this on all three native hosts.
6. Create a draft GitHub Release targeting that SHA, attach all three archives and
   checksums, and verify its notes, tag target, assets, and prerelease flag
   before publishing it. Use ordinary forward commits for any follow-up fixes.
   Use the matching changelog section for notes, state the validated source SHA,
   and keep the file-observation, reload failure, Agent grant and owned-directory
   cleanup boundaries explicit. Resource impact is declared reachability, not
   causation; schema-1 incidents do not retain the complete impact graph. Fresh
   post-trigger observations are non-atomic, abrupt death can leave incomplete
   captures, and default exports still contain potentially sensitive identifiers
   and paths. Recovery commands have the user's OS permissions and are not a
   sandbox; recovery never auto-starts services or rewrites incidents/baselines.
   Optional logs have separate attribution. Candidate validation alone
   does not publish it. Any source change requires CI and artifacts for the new
   SHA before publication.

The published crate carries `[package.metadata.binstall]`. Its template resolves
the Unix `.tar.gz` archives and Windows `.zip` archive from the matching GitHub
Release tag; the executable is at the archive root. Validate the prebuilt path
with a pinned cargo-binstall client and `--strategies crate-meta-data`, so a
source compilation fallback cannot make the check pass accidentally:

```bash
cargo binstall --strategies crate-meta-data --no-confirm \
  --root <temporary-install-root> devd@X.Y.Z
```

Run this after the GitHub Release assets exist and the crates.io version is
discoverable. Compare the installed binary SHA-256 with the binary extracted
from the verified release archive, then run `devd --version` and the release
smoke fixture.

Also run the documented command with cargo-binstall's normal strategy
selection into a separate empty install root. Preserve verbose output in the
workflow log and inspect which strategy it selected; verify the installed
version and fixture there as well. This checks the ordinary user command while
the forced `crate-meta-data` run independently proves the prebuilt-only path.

For a repeatable post-publication check on all three native platforms, dispatch
the **Post-release installation** workflow with the published version and its
full source SHA. It installs the exact crates.io version and the pinned
cargo-binstall 1.24.0 release, checks both binaries against the shared config
fixture, verifies that the forced-strategy binstall executable matches the
verified native archive, and runs the ordinary cargo-binstall command in a
separate root with verbose strategy output. Keep the issue open if any native
job or registry availability check has not completed successfully.

The v0.1 MVP was an internal milestone, not a separately published release.
Historical revisions are not release candidates for the current version.

Check the `devd` name on crates.io and confirm maintainer publishing rights
before the first publication. `cargo install --path . --locked` always installs
this checkout.
