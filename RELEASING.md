# Release checklist

The release workflow is manual and uploads run-scoped artifacts. It does not
create a tag, GitHub Release, or crates.io publication. Publishing is a separate
action after the source revision, checks, and artifacts have been reviewed.

1. Confirm the Cargo version, changelog, and both READMEs describe the intended
   release. Alpha versions must be marked as prereleases on GitHub.
2. Run `cargo fmt --all -- --check`,
   `cargo clippy --locked --all-targets -- -D warnings`,
   `cargo test --locked --all-targets`, and `cargo package --locked`.
   Verify that local-only documents are absent from `cargo package --list`.
3. Push the release commit and require successful Linux/macOS/Windows CI on that
   exact SHA, including the three-service recovery smoke test and native Windows
   job, probe, control, persistence, and supervisor-death tests. Cross-compilation
   is useful during development but does not replace native execution. Check
   `top` in an interactive Windows console (quit, restart, cancel/confirm stop)
   before the first Windows release; headless CI cannot validate console rendering.
4. Trigger **Release artifacts** from main with the full source commit SHA in
   its required `revision` input. The workflow verifies the checkout and tests
   the release binary on each target platform before archiving it.
   The packaging entry point is `python scripts/package-release.py <target>`;
   it requires committed tracked sources and an already-built native release
   binary, runs recovery smoke, and writes archives/checksums under
   `target/release-artifacts/`. It never publishes them.
5. Download the Linux x86_64 and macOS arm64 `.tar.gz` artifacts and Windows
   x86_64 MSVC `.zip`. Verify each archive with `shasum -a 256 -c <file>.sha256`
   (or compare PowerShell `Get-FileHash -Algorithm SHA256` against its checksum).
   Inspect the contents: binary (`devd.exe` on Windows), license, both READMEs,
   `VERSION`, and `REVISION`; confirm the latter two match the intended release.
   Local macOS testing does not replace validation of the other artifacts.
6. Create a draft GitHub Release targeting that SHA, attach all three archives and
   checksums, and verify its notes, tag target, assets, and prerelease flag
   before publishing it. Use ordinary forward commits for any follow-up fixes.

The v0.1 MVP was an internal milestone, not a separately published release.
Historical revisions are not release candidates for the current version.

The crate name `devd` is not reserved for this repository on crates.io. Do not
promise `cargo install devd` or publish there without checking ownership and
the package name. `cargo install --path . --locked` installs this checkout.
