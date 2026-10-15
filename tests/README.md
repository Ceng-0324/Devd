# Validation

Run from the repository root with Rust 1.95 or newer and Git on PATH.
Linux/macOS integration tests also require `/bin/sh`; Windows requirements are
listed below. The Rust suite needs no Docker, Python, database, external endpoint,
or fixed free port. Python release checks require Python 3.11 or newer.

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo run --locked -- check --config tests/fixtures/simple.yml
cargo build --locked
cargo doc --locked --no-deps
python3 -m unittest discover -s scripts -p 'test_*.py' -v
python3 scripts/incident-smoke.py
python3 scripts/recovery-smoke.py
python3 scripts/generate-third-party-notices.py --check
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
fails validation rather than silently skipping coverage. On Windows, use your
Python 3.11+ executable (commonly `python`) for the Python command above.
Run the Unix service lifecycle scenario suite separately with:

```bash
cargo test --locked --test integration
```

`process_lifecycle.rs` uses an ignored Rust subprocess fixture for graceful
group shutdown: it registers TERM handling before readiness and verifies that
the descendant received TERM. This avoids depending on shell trap/reaping
behavior while preserving exit-status and process-group cleanup assertions.

## Coverage

`wait/coverage.rs` shares the native process fixtures in `wait.rs` and tests
static read-only inspection, profile overrides, malformed/oversized input,
unavailable instances, effective config across edits/deletion/reload, independent
recorders and reduced retention across runs. Task tests check completed-task
coverage. Incident/event failure fixtures assert live recorder failure and
known artifact eviction without changing service behavior; unit tests cover
missed triggers/context, truncation, bounded inventories and cancellable output.


`scripts/recovery-smoke.py` runs the selected native binary through link-target
drift, dangling-resource capture, stopped-instance recipe recovery, replay and
old-run rejection, new-run readiness and original-evidence handoff. It protects
shared fixture data and accepted baselines. CI and release packaging run this
same scenario; the release path uses the extracted archive binary. Synthetic
competitor comparison and its limitations are described in VALIDATION.md.

`recover.rs` runs native stopped-instance recipe workflows: missing registered
directory reconstruction, multistep conditions, literal argv/privacy, profile/custom
state isolation, preserved shared/history data, configuration/program/tree/run
changes, expired plans, resource replacement/link/hard-link refusal, partial failure,
preconditions/postconditions, timeout, state-lock contention and abrupt interruption
without replay. A full stdout pipe exits within its deadline; Unix SIGINT verifies
command cleanup and retained cancellation evidence. Two ignored executable Rust
fixtures are invoked by parent tests. Contract unit tests reject unknown fields,
unsafe selectors, invalid timeouts, duplicates and excess inputs.

`baseline.rs` runs native CLI/file observations on all three platforms: valid-link
retargeting, missing/recovered targets, metadata replacement, directory activity,
explicit program/cwd/env-file/instance selectors, privacy, no-clobber capture,
current alias consumers/downstream impact, profile/custom-state isolation and
foreign/missing provenance. It rejects linked/special/oversized baseline inputs,
unknown schemas, malformed shapes, duplicates and excess selections; forged
recorded paths never drive IO. A full stdout pipe must time out (or cancel on Unix
SIGINT) without hanging the process. Unit tests cover uncertainty, unavailable metadata,
traversal-time replacement, strict program selection, marker replacement and
cancelled publication. Windows requires symlink privileges; unavailable file
identity is reported separately from supported attribute observations.

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
after shutdown without explicit stored mode. Offline tests cover two persisted runs,
UUID/latest selection, profile/custom-state isolation, missing YAML, separate
runless logs, historical state variants, incomplete tails, active writer locks,
unsafe/special files and existing output. Unix sends SIGINT during a retained scan
and verifies no report is published and the state lease is released. Its ignored
worker fixture emits a secret sentinel. Unit coverage checks privacy fields,
marker/rotation/run-order gaps, report limits and the cancellation/publication
boundary; existing live Agent export tests remain regression coverage.

`wait.rs` runs real CLI/supervisor processes on all three platforms: whole-stack
and named readiness, health gates, deduplication, timeout evidence, invalid
names/deadlines, profiles/custom state, deleted YAML, same-run manual/automatic
replacement, no-op reload invalidation, rejected plans, concurrent wait limits,
and stop/status capacity. Unix also sends SIGINT to prove cancellation preserves
service PIDs. Two ignored test functions are executable service/probe fixtures;
no platform shell or external probe server is needed. In-memory protocol tests
cover coalesced reload barriers, pending restarts, EOF and run-ID mismatch,
deadline expiry, terminal states, and prompt permit release after disconnect.

`incidents.rs` exercises explicit opt-in, unchanged service PIDs after target
deletion, captured dangling-link chains, normal stop/manual restart exclusions,
five-second windows, reload/stop endings, repeated failed generations, retention,
abrupt supervisor death and runtime writer failure without stopping services.
The ignored worker is launched as a real subprocess. Unit tests cover correlation,
four-slot admission and eight-message ownership, subscriber lag, byte/count limits,
privacy, cancellation before publication, active-capture retention, writer locks,
no-clobber creation, writer-side file replacement and unsafe directories/files.
Windows symlink privileges are required; Unix additionally rejects FIFO locks.
Query/export coverage also exercises current-run scoping, all retained runs after
shutdown, deleted YAML, repaired targets without rewriting history, profile/custom
state isolation, separate log attachments, no-clobber output, malformed/partial
files, unsupported schemas, hard links, symlinks and oversized records. Reader
unit tests cover schema/identity/order checks, privacy filtering, scan limits,
replacement during reads, state/writer lease retention, cancellation and output
backpressure. Relationship tests cover shared-node tree references, complete DAG
edges, historical reprojection without files, service/edge capacity gaps, bounded
shared inspections, cycle rejection, schema-1 compatibility and damaged optional
graphs that preserve valid resource/end records. The end-to-end deletion
walkthrough is in both READMEs.
Transferred-report coverage reuses `export.rs`'s real live/recovered/retained
reports, reading them from an empty recipient directory. Diagnose unit tests cover
legacy live schemas, recomputed explanations, stale state, separate log metadata,
identity/order/cause checks, missing sequences, limits, terminal escaping,
cancellation and output backpressure. Unknown outer schemas and truncated JSON
fail; partial incident windows and damaged optional graphs retain valid facts.

`compare` reuses those transferred reports from an empty recipient directory and checks comparable/different/unknown source fields, independent log metadata, hashed resource identities, changed link targets, retained relationship edges, gap differences, output limits and terminal-safe text. It never reads the original project or treats similar historical explanations as a shared cause.

`scripts/incident-smoke.py` executes that workflow against a selected native
binary, including schema-2 retained dependency/resource relationships,
unchanged PIDs, independent recording permissions, offline export after
configuration deletion and target
repair, transferred JSON/text diagnosis with original state/resources hidden,
damaged relationship evidence, privacy, no-clobber output and abrupt-death incomplete evidence. It owns
isolated temporary fixtures and releases its Unix workers after a supervisor kill;
Windows uses Job teardown. CI runs it on all three platforms, and release archive
verification repeats it with the extracted executable rather than a debug build.

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

`resource_inspection.rs` runs doctor on Linux/macOS/Windows against intermediate
directory links, relative and dangling targets, cycles, selected profiles, custom
state paths, structured requirement failures and bounded report coverage. Core
resource inspection tests cover repeated noncyclic links, node/link/path/escaped
JSON limits, deterministic replacement between inspection and recheck, and Unix
permissions, special files and non-UTF-8 targets. Windows parent-segment resolution
is explicitly unsupported rather than assigned Unix semantics. Symlink privileges
(Developer Mode or equivalent on Windows) and non-root Unix permission tests are
required, not silently skipped. Doctor output tests exercise timeout and
cancellation under backpressure; existing native Windows evaluator tests cover ACL
loss/recovery independently of metadata-only observations.

`impact.rs` covers selected profiles, aliases, custom state without provisioning,
invalid selectors, corrupt discovery records, and live linked worktrees whose YAML
is edited, invalidated or removed before shutdown. Core impact tests cover diamond
and cyclic graphs, direct binding recipients, shortest downstream witnesses,
unknown ownership, observation conflicts and capacity limits. CLI unit tests cover
oversized/invalid/special peer files and output timeout/cancellation under
backpressure. The worktree test requires Git; symlink privileges are required on
Windows. Impact observes declarations and disk candidates, never live-config
equivalence or actual file access.

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

- The Unix lifecycle scenarios in `integration.rs` copy YAML and `workload.sh` into their own short temporary directories. The workload records start/stop events and publishes each generation's leader and descendant PIDs atomically. `crash-<service>` files inject a one-shot failure. Other suites use their own native child fixtures and shared support.
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
Unit tests reject unknown fields, duplicate JSON fields (including `id`) and
privilege injection, verify per-operation grants before any I/O, and bound stalled
output. Reusing a transport ID does not deduplicate controls. The ignored
worker is a subprocess fixture. Existing readiness, reload and cleanup suites remain
the source of their lifecycle/partial-progress semantics.

The linked-worktree scenario starts two concurrent TCP-ready instances with
profiles and custom state directories. It verifies discovery, mapped environments,
Agent restart/reload/export, cross-instance control rejection, and stop/cleanup
without changing the other instance's PID, generation or shared data. An old
Agent session must reject a replacement run at the same endpoint.


## MCP interface (v0.9)

`tests/agent.rs` shares supervisor/worker fixtures between the JSON-lines and MCP
adapters. MCP cases cover version negotiation, discovery, strict arguments, denied
controls, wrong instance/old run, stale reload plans, normal-stop cleanup, isolated
report diagnosis after deleting YAML, repeated wait cancellation, EOF, oversized
input and duplicate outstanding IDs. Unix signals are exercised with idle stdin
for both adapters. Transport unit tests bound admission, cancellation/ID reuse,
output size and stalled writes/lock acquisition.

Run a real protocol client against the built binary:

```bash
cargo build --locked
# Node >=22.19 plus npm/npx; downloads the pinned official client on first use.
python3 scripts/mcp-smoke.py
```

The script pins `@modelcontextprotocol/inspector@2.10.1`, checks tool discovery,
live identity/status/events, incident list/show after an actual worker crash,
denied stop, explicitly granted restart, opt-in missing-key/out-of-scope rejection,
export and offline diagnosis with the original YAML removed. It uses temporary
files and cleans up its supervisor. Native Linux/macOS/Windows CI runs this same
exercise with Node 24. This proves Inspector protocol interoperability; it does
not claim a model-driven Codex or Claude conversation was accepted.

### Opt-in Agent control policies

Policy unit tests use a real protocol endpoint with explicit reply barriers to
verify concurrent duplicate lookup, cancellation after dispatch, definite refusal
and allowance refund, unknown-result retention, capacity and byte limits, and
active-entry retention under a virtual clock. Native `agent.rs` tests cover both
adapters, independent opt-in flags and grants, validation without budget loss,
remaining counts, session reset, stop after exhausted restart allowance, cached
stop/cleanup after shutdown, preservation of newly created user data, and rejection
of replacement runs even after their endpoint disappears.

Inspector CLI ends its adapter after one call. `mcp-smoke.py` therefore additionally
runs `mcp-policy-smoke.mjs` using pinned `@modelcontextprotocol/client@2.2.0` in
one live stdio session. Both clients are installed into the temporary fixture
with lifecycle scripts disabled before protocol calls begin. Node invokes their
entrypoints directly; npm installation and protocol calls have separate deadlines,
and protocol timeouts include captured output. It discards the first restart result, retries with the same
operation key, checks one generation change, and verifies conflict/exhaustion
responses. This is same-session interoperability evidence, not a claim of durable
retry identity across reconnections. Adapter cancellation/unknown cases remain
covered by the deterministic protocol tests.

### One-shot task gates

`cargo test --locked --test task_config` checks contextual defaults, profiles,
strict fields and lifecycle/edge validation. `cargo test --locked --test wait tasks::`
runs native CLI fixtures on every supported platform: success/failure ordering,
opt-in retries, timeout/stop/spawn failure, descendant cleanup, manual rerun with
stale-success rejection, selective reload and failed reload, and transferred
completion evidence. The ignored task fixture is launched only by these tests.
