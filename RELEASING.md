# v0.1 release checklist

The release workflow is manual and uploads run-scoped artifacts. It does not
create a tag, GitHub Release, or crates.io publication. The repository owner
handles those actions after reviewing and testing the artifacts.
The v0.1 candidate is commit `eaa52b08e119285e37bd4bc5712ed517bc0c7733`;
later commits start v0.2 development. Run checks on that revision in a separate
checkout or worktree when preparing v0.1; testing current main tests v0.2.

1. From the intended commit, run `cargo fmt --all -- --check`,
   `cargo clippy --locked --all-targets -- -D warnings`,
   `cargo test --locked --all-targets`, and `cargo package --locked`.
2. Confirm the v0.1 behavior and limitations in both READMEs and replace the
   changelog's pre-release heading only when the release is actually approved.
3. After pushing, trigger **Release artifacts** from main and enter the full
   source commit SHA in its required `revision` input. The workflow checks out
   and verifies that revision rather than building main implicitly. For v0.1,
   use the candidate SHA above. Download the Linux
   x86_64 and macOS arm64 artifacts from the workflow run. Verify each archive
   against its `.sha256` file with `shasum -a 256 -c <file>` and test the binary
   on its target platform. Check the archived `VERSION` and `REVISION` files
   against the intended release. A local macOS build cannot validate the Linux job.
4. Review the final commit SHA, changelog, archive contents, checksums, and
   supported platform claims before creating a tag or publishing anything.

The crate name `devd` is not reserved for this repository on crates.io. Do not
promise `cargo install devd` or publish there without checking ownership and
the package name. `cargo install --path . --locked` installs this checkout.
