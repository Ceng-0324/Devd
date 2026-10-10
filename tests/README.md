# Validation

Run from the repository root on Linux or macOS with Rust 1.95 or newer and `/bin/sh`. No Docker, Python, database, external endpoint, or fixed free port is required.

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo run --locked -- check --config tests/fixtures/simple.yml
cargo build --locked
cargo doc --locked --no-deps
python3 -m unittest discover -s scripts -p 'test_*.py' -v
```

The CI matrix builds, lints, and runs all applicable test targets on Linux, macOS,
and Windows. Unix shell/PTY scenarios remain Unix-only. Windows runs the shared
configuration, protocol, health, storage, and state-machine tests, plus
`cargo test --locked --test windows`: real Job-owned process trees, stop/restart/
drop, script completion/timeout/cancellation, named-pipe CLI control, duplicate
supervisors, configuration deletion, snapshot restore, stored logs, stalled
foreground-output cancellation, and cleanup after supervisor termination. Shared
storage tests also cover native rotation and hard-link rejection; Windows unit
tests cover reserved filenames and profile directory identities. Its ignored
fixture is invoked by the parent tests.
Windows path tests require symlink privileges and `icacls.exe`; denied setup
fails validation rather than silently skipping coverage. The Python release
checks require Python 3.11 or newer; the Rust suite itself does not need Python.
Run the complete Unix MVP scenario suite separately with:

```bash
cargo test --locked --test integration
```

## Coverage

`top.rs` uses Unix PTYs to exercise the actual terminal: log/event switching,
instance/run identity, generation and cause fields, restart evidence, paging,
cancelled/confirmed stop, quit, Ctrl+C, SIGTERM, disconnect and alternate-screen
restoration. Shared TUI tests cover small terminals, no-color rendering, bounded
history, retention/tail gaps, unavailable causal references, malformed/mixed-run
batches, independent scrolling and confirmation controls. A native supervisor
test runs the TUI's query/subscription/control functions over Unix sockets and
Windows named pipes, including a replacement supervisor at the same endpoint:
old-run stop/restart/log subscription requests must fail without changing its
PID or restart count; correct-run controls and cancelled subscriptions still work.
Its ignored worker fixture is launched by the parent test. Headless Windows CI
tests the model, renderer and native transport, not interactive console appearance;
the manual console check remains in `RELEASING.md`.

`export.rs` runs on all three platforms with a real supervisor: it checks live
identity and generation correlation, missing YAML, default exclusion of raw
log/environment text, explicit log inclusion, no-clobber output, and refusal
after shutdown. Its ignored worker fixture emits a secret sentinel. Core unit
coverage checks free-text failure removal, retained-history gaps and explanation
completeness.

`wait.rs` runs real CLI/supervisor processes on all three platforms: whole-stack
and named readiness, health gates, deduplication, timeout evidence, invalid
names/deadlines, profiles/custom state, deleted YAML, same-run manual/automatic
replacement, no-op reload invalidation, rejected plans, concurrent wait limits,
and stop/status capacity. Unix also sends SIGINT to prove cancellation preserves
service PIDs. Two ignored test functions are executable service/probe fixtures;
no platform shell or external probe server is needed. In-memory protocol tests
cover coalesced reload barriers, pending restarts, EOF and run-ID mismatch,
deadline expiry, terminal states, and prompt permit release after disconnect.

`instances.rs` runs natively on all three platforms: live identity, profiles,
external state directories, restart identity/run separation, removed YAML,
linked/detached worktrees, nested configurations, branch-at-start provenance,
and read-only handling of corrupt/stale records. Unit tests bound registry files
and silent/oversized endpoint responses; Unix adds symlink/FIFO/hard-link rejection.
These tests require Git on PATH for real worktree scenarios.

`path_requirements` unit tests and `doctor.rs` cover file/directory/symlink
requirements, dangling and cyclic links, target replacement, read permissions,
cwd resolution, and startup refusal. `events.rs` exercises runtime loss/recovery,
debouncing, atomic replacement, cyclic links, permissions, generation changes,
and monitor cancellation. Native Windows tests check symlinks and ACL changes,
and verify monitor retirement after restart and stop.

`reload.rs` and the core reload tests cover invalid candidates, stale plans,
selective stop/start order, unchanged processes, partial failure, competing
reload/restart requests, and whole-stack stop before and after configuration
commit. Windows tests additionally verify Job-owned descendant cleanup after
reload success, application failure, and stop preemption.

`scripts/test_release_verification.py` checks tar/zip provenance and rejects
corruption, wrong versions/revisions, private or escaping paths, duplicates,
empty files, and nonregular entries. It also verifies Cargo excludes local-only
documents. The release workflow tests recovery using the verified archive's
binary on each native platform; synthetic archive tests do not replace this.

`config_profiles.rs` covers map inheritance, list/probe replacement, optional-field
clearing, aliases, duplicate and unknown fields, invalid names, added services,
and effective dependency errors. CLI tests run base/dev/staging concurrently,
check cwd/env-file resolution, default/explicit state directory isolation,
case-sensitive names, and control after configuration corruption/deletion.
`snapshots.rs` verifies exact YAML round trips, deleted/invalid source recovery,
new-file-only behavior, safe path handling, symlink rejection, and unchanged live
supervisor process ownership.
The runnable `examples/profiles/devd.yml` also has an end-to-end CLI test for
profile validation, graph export, exact snapshot recovery, filtered logs, and
concurrent instance isolation.

| Scenario | Evidence |
| --- | --- |
| Single-service lifecycle | Load `simple.yml`, invoke public `check`, `graph`, `start`, `status`, `logs`, and `stop`; verify stdout/stderr prefixes, final state, and process cleanup |
| Process failure and recovery | Inject exit 23; observe a new PID, restart count, cached exit code, and the failure in CLI logs; verify the previous leader and descendant disappear |
| Dependency chain | Load `dependency-chain.yml`; hold database and API probes at 503 separately; prove dependents stay Pending without process side effects across repeated probes, then release each gate |
| Health failure and recovery | Change a healthy API probe to 503 and back; observe Unhealthy/Healthy status, diagnostic error, stable PID, and no restart under `never` |
| Ordered shutdown | Fixture event journal records frontend → API → database shutdown; every recorded leader and descendant is gone |
| Startup rollback | Release database readiness into a missing API executable; require a failed foreground exit, persisted error, stopped database, and no frontend launch |
| Signal cancellation | SIGINT/SIGTERM while dependencies are pending; verify no later service starts and the process tree is cleaned |
| Retry exhaustion | Crash all three permitted generations; require Failed status, final exit code, all failure log lines, and no remaining fixture processes |
| Resource monitoring | Sample a busy shell's CPU and RSS through CLI text and JSON, restart it, verify fresh persisted metrics, and confirm shutdown clears samples; unit tests cover warmup, generation cache retirement, late samples, unavailable values, and health-state updates |
| Resource alerts | Validate CPU and memory thresholds and profile replacement/clearing; check crossing, recovery, missing samples, and generation reset with synthetic samples; exercise real RSS alerts, deduplication, manual restart, and stored log queries without automatic process termination |
| Opt-in resource restarts | Strict per-service permission and never-policy conflict; three distinct consecutive samples per metric, missing-value reset and generation isolation; real restart/group cleanup, dependency recovery, shared crash budget, exhaustion diagnostics, interruptible backoff, and stored reason logs |
| Script health checks | Exit/signal/spawn results, service cwd and environment precedence, discarded large output, serial failure/recovery, timeout/cancellation/group cleanup, supervisor stop, static checks without execution, script-ready gating, graph formats, profile replacement, health restarts and dependency recovery |
| Dependency recovery | Opt-in chains after manual/automatic replacement, readiness gating, same-process health-flap exclusion, combined recoveries, shared retry budgets, stop/manual restart during backoff, completed services staying stopped, and slow health probes surviving peer updates; CLI verifies config validation, logs, and cleanup |

`cli.rs` covers command options, malformed input, duplicate supervisors, configuration deletion, control protocol errors, and terminal backpressure. Lower-level lifecycle, orchestration, health, configuration, dependency, and logging suites retain their focused checks.
Graph CLI checks also compare default and explicit text output, verify deterministic DOT/Mermaid nodes and condition-labeled edges, apply the selected profile, and reject invalid formats and dependencies without creating runtime state.
Log filter checks cover matching order and inclusive time boundaries, filtered tail counts, live follow after a restart, and invalid level or duration values.

`persistent_logs.rs` exercises opt-in startup, default memory-only behavior,
offline queries after YAML deletion, cross-run append, profile isolation with
an explicit state directory, final partial-line drain, real size rotation,
invalid arguments, setup failure without process side effects, and storage
failure followed by ordered process cleanup. Storage unit tests cover retention,
filter-before-tail, JSON escaping, interrupted tails, corrupt/oversized records,
file locks, permissions, symlink/hard-link rejection, and explicit lag diagnostics.

## Fixture ownership and determinism

- Each scenario copies YAML and `workload.sh` into its own short temporary directory. The workload records start/stop events and publishes each generation's leader and descendant PIDs atomically. `crash-<service>` files inject a one-shot failure.
- The test-owned HTTP mock binds `127.0.0.1:0` once and holds the listener throughout the test. Tests insert its assigned URL through structured YAML before invoking the CLI. These endpoints simulate readiness/faults; the supervised shell workloads create the real process trees. The placeholder port in `dependency-chain.yml` is only for static validation, not a standalone healthy service.
- Readiness gates and observed probe counts control sequencing. Short polling intervals yield CPU; tests do not assume a service becomes ready after a fixed sleep.
- CLI commands have a 15-second deadline and capture output to temporary files to avoid pipe deadlocks. Scenario predicates have a 12-second deadline and report the last snapshot plus supervisor output when they fail. Supervisor guards send SIGTERM during unwinding, wait up to 8 seconds, then kill and reap if necessary. The HTTP mock shuts down and joins its thread on drop.
- Tests check final state and the OS process table, including previous crash generations. The existing ignored orchestration signal test is a subprocess fixture invoked by another test; it is not missing coverage.

## Manual smoke test

For a working HTTP API, frontend and worker, see the
[local stack example](../examples/local-stack/README.md). Its smoke script also
checks manual restarts, process crashes, health failures and final cleanup; CI
runs the short version on all three platforms. On Windows, use
`python examples/local-stack/smoke.py --binary target/debug/devd.exe --duration 5 --restarts 2`.
The script selects its own Python executable and needs no Unix tools on Windows.

If a test runner cannot launch subprocess tests but a normal terminal can, copy the standalone fixture into a temporary project. If the environment prohibits process creation or loopback networking entirely, run the suite on a supported host; configuration validation alone does not prove lifecycle behavior.

```bash
devd_smoke_dir=$(mktemp -d /tmp/devd-smoke.XXXXXX)
cp tests/fixtures/simple.yml "$devd_smoke_dir/devd.yml"
cp tests/fixtures/workload.sh "$devd_smoke_dir/workload.sh"
printf 'Smoke project: %s\n' "$devd_smoke_dir"
cargo run --locked -- check --config "$devd_smoke_dir/devd.yml"
cargo run --locked -- start --config "$devd_smoke_dir/devd.yml"
```

Keep that terminal open. In a second terminal at the repository root, set `devd_smoke_dir` to the printed directory and run:

```bash
cargo run --locked -- status --json --config "$devd_smoke_dir/devd.yml"
cargo run --locked -- logs worker --config "$devd_smoke_dir/devd.yml"
touch "$devd_smoke_dir/crash-worker"
cargo run --locked -- status --json --config "$devd_smoke_dir/devd.yml"
cargo run --locked -- logs worker --config "$devd_smoke_dir/devd.yml"
cargo run --locked -- stop --config "$devd_smoke_dir/devd.yml"
```

Poll status until the PID changes and `restart_count` is 1; logs should contain `injected crash, exit 23`. After stop, wait for the first terminal to exit successfully and confirm `worker stopped` was printed. Its final `services.json` should have `status: stopped` and a null PID, while a new status command should report no reachable supervisor. The `.pid` files record each leader (in the filename) and descendant (in the contents); verify those PIDs no longer exist with `ps` before removing the temporary directory.

## Instance bindings (v0.7)

`config_bindings.rs` validates port conflicts, portable owned paths, environment
name collisions, and profile replacement/clearing. Its ignored child fixture is
launched by a native test to prove application bind failures retain logs and exit
evidence while the conflicting listener remains held. CLI tests cover shared
path preservation, dotenv precedence, owner/probe/dependent environments, profile
and custom-state-directory isolation, doctor, and selective reload. Windows also
executes a native owner/dependent environment fixture inside supervised Jobs.

## Owned cleanup (v0.7)

`clean.rs` runs native CLI/supervisor fixtures on all three platforms. It covers
explicit authorization, refusing adoption, live-instance exclusion, successful
shutdown, config/tree/run plan invalidation, profile/custom-state isolation,
reload rejection, symlinks, abrupt death, preserved shared/unregistered data and
diagnostics, and idempotent application after a new user directory appears.
Windows also injects a read-only-file failure and retries partial cleanup.
The ignored worker is a child-process fixture, not omitted test coverage.
`core::owned_paths` unit tests exercise ownership replacement/revocation, shared
aliases, hard links, moved state, nested ownership markers, preflight ordering,
already-absent roots and interrupted deletion recovery.

## Agent interface (v0.7)

`agent.rs` launches the real JSON-lines process and supervisor on native platforms.
It verifies read-only diagnostics, independent grants, exact instance/run guards,
profile/custom-state attachment, restart/reload/stop, offline cleanup and stale
plans, malformed/oversized requests, and EOF without service shutdown. Endpoint
replacement is exercised with both a new run and a different configuration at
the same state path. Unix additionally sends SIGTERM while stdin is idle.
Unit tests reject unknown fields, duplicate IDs and privilege injection, verify
per-operation grants before any I/O, and bound stalled output. The ignored worker
is a subprocess fixture. Existing readiness, reload and cleanup suites remain
the source of their lifecycle/partial-progress semantics.
