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
   Verify that local-only documents are absent from `cargo package --list`.
3. Push the release commit and require successful Linux/macOS/Windows CI on that
   exact SHA, including the three-service recovery smoke test and native Windows
   job, probe, control, persistence, and supervisor-death tests. v0.6 also requires
   dangling/cyclic symlinks, replacement, permissions, monitor cancellation,
   invalid/stale reload plans, partial failures, competing controls, and stop
   preemption. Native Windows filesystem tests require symlink creation privileges
   and `icacls.exe`; they must not silently skip unsupported test setup. Cross-compilation
   is useful during development but does not replace native execution. Check
   `top` in an interactive Windows console (quit, restart, cancel/confirm stop)
   before the first Windows release; headless CI cannot validate console rendering.
4. Trigger **Release artifacts** from main with the full source commit SHA in
   its required `revision` input. The workflow verifies the checkout and tests
   the archived release binary on each target platform.
   The packaging entry point is `python scripts/package-release.py <target>`;
   it requires committed tracked sources and an already-built native release
   binary, checks its version against Cargo.toml, writes archives/checksums under
   `target/release-artifacts/`, then verifies the archive and runs recovery smoke
   using its extracted binary. Python 3.11 or newer is required. It never publishes
   them. The six-file allowlist excludes local-only design and agent documents.
5. Download the Linux x86_64 and macOS arm64 `.tar.gz` artifacts and Windows
   x86_64 MSVC `.zip`. Verify each archive with `shasum -a 256 -c <file>.sha256`
   (or compare PowerShell `Get-FileHash -Algorithm SHA256` against its checksum).
   Inspect the contents: binary (`devd.exe` on Windows), license, both READMEs,
   `VERSION`, and `REVISION`; confirm the latter two match the intended release.
   Local macOS testing does not replace validation of the other artifacts.
   Automate these checks for downloaded archives with:

   ```bash
   python3 scripts/verify-release.py <archive> [<archive> ...] \
     --version 0.6.0-alpha.1 --revision <full-40-character-SHA>
   ```

   Keep each checksum beside its archive. Add `--smoke` for a single native
   archive to verify the actual executable version and repeat recovery testing.
   The packaging workflow already does this on all three native hosts.
6. Create a draft GitHub Release targeting that SHA, attach all three archives and
   checksums, and verify its notes, tag target, assets, and prerelease flag
   before publishing it. Use ordinary forward commits for any follow-up fixes.
   Use the matching changelog section for notes, state the validated source SHA,
   and keep the file-observation and reload failure boundaries explicit. Any
   source change requires CI and artifacts for the new SHA before publication.

The v0.1 MVP was an internal milestone, not a separately published release.
Historical revisions are not release candidates for the current version.

The crate name `devd` is not reserved for this repository on crates.io. Do not
promise `cargo install devd` or publish there without checking ownership and
the package name. `cargo install --path . --locked` installs this checkout.
