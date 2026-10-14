# v0.9 validation

This document describes repeatable engineering validation for the v0.9 release
candidate. It is not a user study and does not measure productivity, resource
use, diagnosis time, or adoption.

## Native scenario

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
configuration/script line counts. It does not record author setup time as user
time or turn a successful run into a performance claim.

Run it with an independently verified Process Compose binary:

```bash
python3 scripts/compare-process-compose.py \
  --process-compose /path/to/process-compose \
  --devd target/debug/devd \
  --output comparison.json
```

The successful and failed task gates are intentionally included because Process Compose has
native `process_completed_successfully`; devd does not. The devd entry therefore
uses an explicit completion/failure-marker wrapper. The plain composition invokes
producer and consumer sequentially. Failure prevents consumer output for all
three, and actual stack exit codes are retained separately. This is a functional
gap in devd's native dependency model.

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

As of this candidate there is no independent developer feedback from a separate
project. Engineering checks, synthetic comparison, and maintainer demonstration
do not prove usability or sustained value. Recruiting 3–5 independent users is
the next validation activity; feedback should record project shape, setup
friction, outcome, and whether continued use is wanted.
