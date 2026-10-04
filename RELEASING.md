# v0.1 release checklist

The release workflow is manual and uploads run-scoped artifacts. It does not
create a tag, GitHub Release, or crates.io publication. The repository owner
handles those actions after reviewing and testing the artifacts.
The v0.1 candidate is commit `eaa52b0`; later commits start v0.2 development.

1. From the intended commit, run `cargo fmt --all -- --check`,
   `cargo clippy --locked --all-targets -- -D warnings`,
   `cargo test --locked --all-targets`, and `cargo package --locked`.
2. Confirm the v0.1 behavior and limitations in both READMEs and replace the
   changelog's pre-release heading only when the release is actually approved.
3. Trigger **Release artifacts** on that exact commit. Download the Linux
   x86_64 and macOS arm64 artifacts from the workflow run. Verify each archive
   against its `.sha256` file with `shasum -a 256 -c <file>` and test the binary
   on its target platform. A local macOS build cannot validate the Linux job.
4. Review the final commit SHA, changelog, archive contents, checksums, and
   supported platform claims before creating a tag or publishing anything.

The crate name `devd` is not reserved for this repository on crates.io. Do not
promise `cargo install devd` or publish there without checking ownership and
the package name. `cargo install --path . --locked` installs this checkout.
