# Release checklist

The release workflow is manual and uploads run-scoped artifacts. It does not
create a tag, GitHub Release, or crates.io publication. Publishing is a separate
action after the source revision, checks, and artifacts have been reviewed.

1. Confirm the Cargo version, changelog, and both READMEs describe the intended
   release. Alpha versions must be marked as prereleases on GitHub.
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
   comparison and independent feedback separately, as described in VALIDATION.md;
   synthetic engineering success is not user-value evidence.
4. Trigger **Release artifacts** from main with the full source commit SHA in
   its required `revision` input. The workflow verifies the checkout and tests
   the archived release binary on each target platform.
   The packaging entry point is `python scripts/package-release.py <target>`;
   it requires committed tracked sources and an already-built native release
   binary, checks its version against Cargo.toml, writes archives/checksums under
   `target/release-artifacts/`, then verifies the archive and runs the stack, incident and reviewed-recovery
   smokes using its extracted binary. Python 3.11 or newer is required.
   It never publishes them. The eleven-file allowlist excludes local-only design
   and agent documents.
5. Download the Linux x86_64 and macOS arm64 `.tar.gz` artifacts and Windows
   x86_64 MSVC `.zip`. Verify each archive with `shasum -a 256 -c <file>.sha256`
   (or compare PowerShell `Get-FileHash -Algorithm SHA256` against its checksum).
   Inspect the contents: binary (`devd.exe` on Windows), license, both READMEs, bilingual command guides and usage notes under `docs/`,
   `THIRD_PARTY_LICENSES.md`, `VERSION`, and `REVISION`; confirm the latter two match the intended release.
   Local macOS testing does not replace validation of the other artifacts.
   Automate these checks for downloaded archives with:

   ```bash
   python3 scripts/verify-release.py <archive> [<archive> ...] \
     --version 0.9.0-alpha.1 --revision <full-40-character-SHA>
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

The v0.1 MVP was an internal milestone, not a separately published release.
Historical revisions are not release candidates for the current version.

The crate name `devd` is not reserved for this repository on crates.io. Do not
promise `cargo install devd` or publish there without checking ownership and
the package name. `cargo install --path . --locked` installs this checkout.
