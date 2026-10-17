# Validation

This document describes repeatable engineering validation for the current source.
Published results belong to their release revision; new source scenarios must be rerun. It is not a user study and does not measure productivity, resource
use, diagnosis time, or adoption.

## Stable workflow

```bash
python3 scripts/stable-smoke.py --binary target/debug/devd
python3 scripts/init-smoke.py --binary target/debug/devd
cargo test --locked --test compatibility
python3 scripts/mcp-smoke.py --binary target/debug/devd
```

The native workflow combines a finite producer and long-running consumer, task
rerun with old output present, default coverage gaps, a session restart budget,
same-key retries, fresh reload, authorized stop after exhausted allowance, and
offline report comparison after removing the original configuration/resource.
It also checks failure blocking and CLI exit codes. Deterministic Rust suites
cover cancellation, competing requests, lost replies, unknown outcomes, stale
plan/generation rejection and shutdown races; the smoke does not replace them.
The official MCP client script exercises the selected binary too.

The reviewed-init scenario requires Node/npm. It scans a synthetic project
without running scripts or reading its FIFO `.env` on Unix, compares interactive
and non-interactive output, rejects an existing destination without reporting
publication, and explicitly starts the generated stack to check successful and
failed npm build gates. The script is shared by native CI and extracted-binary
validation; it does not constitute independent project feedback.

The [compatibility contract](docs/COMPATIBILITY.md) links frozen v0.9 exports and
legacy YAML/profile regression coverage. Release packaging runs these native
workflows against the **extracted archive binary**, along with earlier incident
and recovery scenarios. CI and artifact runs must identify the same full SHA;
results for an earlier candidate do not validate later changes. Published source
revisions, run links, checksum verification and limitations are recorded in
[GitHub Releases](https://github.com/Ceng-0324/Devd/releases).
[M5 acceptance](https://github.com/Ceng-0324/Devd/issues/42) retains the earlier
engineering candidate evidence.

## Native incident/recovery scenario

Run the selected native binary through the complete scenario:

```bash
python3 scripts/recovery-smoke.py --binary target/debug/devd
```

The fixture uses only a temporary directory and synthetic data. It starts an
upstream service and declared downstream consumer, captures a baseline through
a symbolic link, retargets it to prove drift detection, then removes its target.
With incident recording enabled it verifies the retained dangling-link evidence
and declared downstream relationship without restarting either service. It then
stops normally, uses a reviewed recovery recipe to rebuild only a registered
disposable directory, rejects replay and an old-run plan, starts a new run and
waits for it, and transfers the original incident report to an empty directory.
The recipient diagnoses saved evidence without the original configuration,
state, link, or resource.

The fixture asserts that shared data and baselines survive recovery, application
secrets never enter plan/result/report output, and repair does not rewrite
historical evidence. Windows requires real symbolic-link privileges; absence of
that privilege is a validation failure.

## Fixed comparison

`scripts/compare-process-compose.py` compares devd with Process Compose
v1.122.0 and a small Python composition using synthetic task, dangling-link and
two-worktree inputs. It records tool version, platform, exit status, produced files, and
configuration/script line counts, binary hashes and checkout revision/dirty state.
Failed scenarios remain in the output with remaining observations marked unknown;
the script exits nonzero after writing the report. It does not record author setup time as user
time or turn a successful run into a performance claim.

Run it with an independently verified Process Compose binary:

```bash
python3 scripts/compare-process-compose.py \
  --process-compose /path/to/process-compose \
  --devd target/debug/devd \
  --output comparison.json
```

The successful and failed task gates use native completion dependencies:
Process Compose uses `process_completed_successfully`; devd uses `kind: task`
and `completed-successfully`. The Python composition invokes producer and
consumer sequentially. Fixture output files are checked as results; devd no
longer uses a marker-wait wrapper to order these tasks. The assertions require
failure to prevent consumer output and retain actual stack exit codes separately.
The v0.9 comparison used a wrapper; its published results do not validate this
new native path.

For link loss, Process Compose runs the same handwritten observer as the plain
script case. Both can preserve a first observation and transfer JSON after
explicit fixture repair; their downstream label is a handwritten declaration,
not a general graph analysis or a devd-compatible report. devd runs the complete
registered-recovery scenario above. The workload sizes and safety contracts
differ, so their run times are not compared.

The worktree cases create real disposable Git worktrees, use separate endpoints,
start both simultaneously and replace one run while preserving its peer. devd
also discovers both and rejects an Agent request carrying the previous run ID.
Process Compose's tested CLI restart addresses the current endpoint; a script
can add an explicit UUID comparison. devd's ordinary CLI also addresses the
current endpoint. This exercise does not evaluate all Process Compose MCP or
remote authorization modes. Docker Compose is not covered because it has a
container resource model. The comparison requires Unix; devd's own native
scenario and regression suites cover all supported release platforms.

## Value evidence

Use the shared [field testing contract](docs/FIELD_TESTING.md)
([简体中文](docs/FIELD_TESTING.zh-CN.md)) and its single feedback record.
Engineering validation, maintainer field trials in real projects, and independent
developer trials are distinct evidence categories. The two field categories
record the same environment, actual work, assistance, friction, outcomes, and
continued-use observations; retain failures and unknowns. There is no fixed
user count, duration, success requirement, or all-features gate.

No completed human-led maintainer field trial has been reported yet; independent
feedback remains **unverified**, as it was at v1.0.0 engineering acceptance.
Agent-driven integration exercises remain engineering evidence and do not
establish human usability or sustained value. Link actual field records here
when available, keeping intent to continue separate from observed repeated use.
