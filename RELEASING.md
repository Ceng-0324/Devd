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
3. Push the release commit and require successful Linux/macOS CI on that exact
   SHA, including the three-service recovery smoke test.
4. Trigger **Release artifacts** from main with the full source commit SHA in
   its required `revision` input. The workflow verifies the checkout and tests
   the release binary on each target platform before archiving it.
5. Download the Linux x86_64 and macOS arm64 artifacts. Verify each archive with
   `shasum -a 256 -c <file>.sha256`. Inspect its contents and confirm `VERSION`
   and `REVISION` match the intended release. Local macOS testing does not
   replace validation of the Linux artifact.
6. Create a draft GitHub Release targeting that SHA, attach both archives and
   checksums, and verify its notes, tag target, assets, and prerelease flag
   before publishing it. Use ordinary forward commits for any follow-up fixes.

The v0.1 MVP was an internal milestone, not a separately published release.
Historical revisions are not release candidates for the current version.

The crate name `devd` is not reserved for this repository on crates.io. Do not
promise `cargo install devd` or publish there without checking ownership and
the package name. `cargo install --path . --locked` installs this checkout.
