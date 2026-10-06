# MVP validation

Run from the repository root on Linux or macOS with Rust 1.95 or newer and `/bin/sh`. No Docker, Python, database, external endpoint, or fixed free port is required.

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo run --locked -- check --config tests/fixtures/simple.yml
cargo build --locked
cargo doc --locked --no-deps
```

The CI matrix runs all test targets and validates the single-service fixture on both Linux and macOS. Run the complete MVP scenario suite separately with:

```bash
cargo test --locked --test integration
```

## Coverage

`config_profiles.rs` covers map inheritance, list/probe replacement, optional-field
clearing, aliases, duplicate and unknown fields, invalid names, added services,
and effective dependency errors. CLI tests run base/dev/staging concurrently,
check cwd/env-file resolution, default/explicit state directory isolation,
case-sensitive names, and control after configuration corruption/deletion.
`snapshots.rs` verifies exact YAML round trips, deleted/invalid source recovery,
new-file-only behavior, safe path handling, symlink rejection, and unchanged live
supervisor process ownership.

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
| Dependency recovery | Opt-in chains after manual/automatic replacement, readiness gating, same-process health-flap exclusion, combined recoveries, shared retry budgets, stop/manual restart during backoff, completed services staying stopped, and slow health probes surviving peer updates; CLI verifies config validation, logs, and cleanup |

`cli.rs` covers command options, malformed input, duplicate supervisors, configuration deletion, control protocol errors, and terminal backpressure. Lower-level lifecycle, orchestration, health, configuration, dependency, and logging suites retain their focused checks.
Graph CLI checks also compare default and explicit text output, verify deterministic DOT/Mermaid nodes and condition-labeled edges, apply the selected profile, and reject invalid formats and dependencies without creating runtime state.

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
runs the short version on Linux and macOS.

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
