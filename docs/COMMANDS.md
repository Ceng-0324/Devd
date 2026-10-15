# Command guide

Compatibility, exit codes and upgrades are covered in [the stability contract](COMPATIBILITY.md).

**English** · [简体中文](COMMANDS.zh-CN.md) · [Back to README](../README.md)

The stack is running. Now what? Find the task you need below; the examples get you started, and the details explain what happens when things go wrong.

- [One-shot tasks](#one-shot-tasks)
- [File prerequisites and path rules](#file-prerequisites-and-path-rules)
- [Instance discovery](#instance-discovery)
- [Wait for readiness](#wait-for-readiness)
- [See what diagnostics can cover](#see-what-diagnostics-can-cover)
- [Export diagnostics](#export-diagnostics)
- [Keep bounded incident evidence](#keep-bounded-incident-evidence)
- [Find and export a recorded incident](#find-and-export-a-recorded-incident)
- [Diagnose a report on another machine](#diagnose-a-report-on-another-machine)
- [Compare two transferred reports](#compare-two-transferred-reports)
- [Explicit instance ports and paths](#explicit-instance-ports-and-paths)
- [Detect environment drift](#detect-environment-drift)
- [Run a reviewed recovery recipe](#run-a-reviewed-recovery-recipe)
- [Clean disposable instance data](#clean-disposable-instance-data)
- [Connect an MCP client](#connect-an-mcp-client)
- [A scoped interface for Agents](#a-scoped-interface-for-agents)
- [Named environments](#named-environments)
- [Configuration snapshots](#configuration-snapshots)
- [Dependency diagrams](#dependency-diagrams)
- [Preview and apply configuration changes](#preview-and-apply-configuration-changes)
- [Command reference](#command-reference)

### One-shot tasks

A build or migration is not ready just because its process started. Declare it
as a task and let consumers wait for a successful exit:

```yaml
version: "1"
services:
  generate:
    kind: task
    command: python3 scripts/generate.py
    task-timeout: 2m
  api:
    command: python3 app.py
    depends-on:
      - service: generate
        condition: completed-successfully
```

`kind` defaults to `service`. Tasks publish `succeeded` only after exit code 0,
managed process-tree cleanup, and bounded log draining. They then have no PID.
Every edge to a task must use `completed-successfully`; that condition is
rejected for long-running services. Health probes and `always` restart are
invalid for tasks. Path prerequisites, environment bindings, and read-only
path/resource observation still work; dependency-recovery and resource-limit
restarts are not task actions.

Tasks default to `restart.policy: never`, including an omitted policy inside a
restart block. To retry failures, explicitly set `policy: on-failure`; the
existing delay/backoff and cumulative `max-attempts` apply. A retry can repeat
external effects: devd does not roll back a half-finished database migration.
A failure with no remaining retry stops the whole stack. Stop/cancellation and
timeout never count as task success, even if termination returns exit code 0.

`task-timeout` (alias `task_timeout`) is positive, defaults to `5m`, and starts
after the process is spawned. Timeout initiates normal process-tree shutdown;
cleanup may take additional time. Dependency waiting has its own existing
`start --dependency-timeout` (default `30s`), and `wait --timeout` is separate.
For a long build, increase those waiting deadlines too. `task-timeout` is
rejected on services. Profiles replace `kind` and `task-timeout`; `null` clears
the timeout override. Restart fields still merge by key, so an explicitly
inherited policy remains explicit when a profile changes the kind.

Restarting a consumer reuses a completed task in the same supervisor run.
`restart generate` explicitly reruns the task and invalidates its old success
when accepted; it returns once the replacement process starts (or a very fast
task completes), so use `wait generate` for completion. Consumers already
released are not retroactively stopped; an opted-in long-running consumer can
use `restart-on-dep-recovery` to restart after the new task generation succeeds.
New attempts wait for the new result. A fresh supervisor always executes tasks
again and never trusts a completion file or an old state snapshot.

Reload reruns affected tasks and their downstream consumers, while unrelated
successful tasks retain their results. A task must finish successfully before
reload advances its layer. The layer still uses the configured dependency
waiting deadline; affected tasks do not automatically retry during reload,
matching the existing failure-stops-the-stack contract. A stack containing only
tasks exits 0 when every task succeeds; use that `start` exit status in scripts,
since there may no longer be a live supervisor to answer a subsequent `wait`.

State/export reports include `kind`, and wait evidence includes
`requires_completed`. Existing reports without these fields remain readable.
New `succeeded`, `completed-successfully`, and `task-timed-out` enum values require
a reader that supports tasks; older binaries are not forward-compatible readers.
For a runnable example, see [examples/tasks](../examples/tasks/README.md).

### File prerequisites and path rules

Declare filesystem prerequisites separately from application health checks. They are checked after service dependencies become ready and immediately before launch:

```yaml
services:
  api:
    command: npm run dev
    cwd: ./backend
    requires:
      - {type: file, path: .env.local}
      - {type: directory, path: uploads}
      - {type: symlink, path: current-data}
```

`file` requires a readable regular file, `directory` an accessible directory, and `symlink` a link to an existing file or directory. Relative paths use the service's effective `cwd`; absolute paths are allowed. A failed requirement prevents that service from starting and causes the normal stack cleanup. `devd doctor` checks the same conditions without starting services or reading file contents.

To observe those conditions after launch, set `monitor-requires: true` on a service with a nonempty `requires` list. It defaults to `false`; a profile can turn it off again, and `restart.policy: never` is allowed. devd waits one second between completed samples and reports a transition after two consecutive matching results. Failures, changes of failure reason, and recovery become `path-condition-changed` events and warning/info log entries. Repeated results stay silent. Polling can miss brief changes, and replacing a file or symlink with another valid one does not trigger an event.

Inspect them with `devd events api --type path-condition-changed --json` or `devd explain api`. Explain includes the latest observation for each requirement in that process generation, plus the preceding failure when it has recovered. These observations do not change application health, restart services, or modify files. They cannot establish why an application later failed. Stop or replace a service and its monitor ends with that generation. Retaining events after shutdown still requires `start --persist-events`; query them with `events --stored` or `explain api --stored`.

`cwd` is relative to the configuration file's directory. `env-file` is relative to the service's `cwd`, and explicit `env` values override entries from that file. Commands support quoted arguments. For pipes, redirection, or shell expansion, use `sh -c '...'` explicitly.

### Instance discovery

```bash
devd instances --json                 # Current repository and its worktrees
devd identity --profile staging --json # Selected live supervisor
```

These commands are included in v0.7. `start` registers the configuration, profile,
canonical state directory, project/worktree root, supervisor PID, start time,
and event `run_id`.
`instance_id` hashes the configuration path, canonical state directory, and
profile: it survives supervisor restarts and branch switches, but moving those
paths changes identity. Git branch and commit are observations at startup;
detached HEAD has no branch, and an unborn branch has no commit.

`instances` reads `<worktree-root>/.devd/instances/*.json` for the current Git
repository and its worktrees, including configurations in subdirectories and
explicit `--state-dir` locations registered by `start`. Outside Git, the project
root is the configuration directory; run discovery from that directory. Without
Git installed, only this directory is discoverable. It does not scan the disk or
find supervisors started by older versions without registration. Registration
requires a writable project index, even with an external state directory; an
index setup failure prevents services from starting.

Records are hints. A bounded request must match the live supervisor's identity
before a row is `live`; other rows show `unreachable`, `identity-mismatch`, or
`unsupported`. Unreachable does not prove the process is dead. Stopped records
remain for inspection and are replaced on the next start of the same instance.
Discovery never changes records, adopts PIDs, stops processes, or deletes data.
JSON has `schema_version: 1`, `entries`, `warnings`, and `complete`; `complete`
only means all indexed records in the selected scope were inspected without
index warnings, not that all services are live or all machine instances known.
Limits are 64 worktrees, 256 records, 64 KiB per record, eight concurrent probes,
and 750 ms per endpoint. Slow supervisors may appear unreachable; retry later.

`instances` uses the current directory and rejects nondefault configuration paths,
profiles, and state directories. `identity` uses the same selectors as `status` and still works when
the YAML has been removed. Neither command isolates ports, files, or databases.
Reports include local paths; consider that before sharing them.

### Wait for readiness

Starting a process does not mean the next step can use it. Before running tests
or handing a stack to another tool, wait for the live supervisor's readiness:

```bash
devd wait                              # Whole stack, up to 30 seconds
devd wait api worker --timeout 1m --json
devd wait api --profile staging --state-dir ./runtime
```

This command is included in v0.7. Omit names to select every service
in the supervisor's effective configuration; supplied names match exactly and
duplicates are ignored. Services with a health check need `healthy` and a PID;
other long-running services need `running` and a PID. Tasks need `succeeded`
with exit code 0 and no PID. Use the same `--config`, `--profile`, and
`--state-dir` as `start`. The YAML can be edited or removed: `wait` reads live
state without loading it or running extra probes.

The default deadline is `30s`, including connection setup; `--timeout` accepts
`1ms` through `1h`. Manual and automatic restarts within the same supervisor
can continue into a new process generation. A pending manual restart prevents
the old generation from satisfying the wait. Whole-stack stop, an accepted
reload (even an equivalent configuration), or a disconnected control stream
ends the wait. Previewing or rejecting a reload does not. There is no reconnect
to a replacement supervisor. Ctrl+C cancels only the waiting command.

Exit code `0` means every selected service was ready in one live observation;
it does not reserve those processes or promise they will stay healthy. Other
wait outcomes exit `1`; invalid CLI arguments exit `2`. A stopped/failed selected
service terminates the wait; pending, starting, unhealthy, and restarting
services can continue waiting. An observed whole-stack stop or reload takes
precedence over service readiness. Each supervisor allows eight concurrent
waits; excess callers receive `busy`, leaving capacity for control commands.

`--json` prints one final report on stdout, including unsuccessful waits:
`schema_version: 1`, `outcome`, `instance_id`, `run_id`, `observed_at`,
`elapsed_ms`, `services`, `blocking`, and `message`. Each service includes its
health/completion requirement, observed state, PID, generation, readiness, pending manual
restart, and last error. Outcomes are `ready`, `failed`, `timed-out`,
`cancelled`, `stopping`, `reloaded`, `disconnected`, `unavailable`,
`invalid-service`, or `busy`. Timeout/cancellation/disconnect preserve the last
received evidence, which may be stale; identity and observation fields are null
if no report arrived. `waiting` is internal stream progress, not a final CLI
outcome. Argument errors may occur before a report can be produced.

Final output has a separate five-second write/flush deadline after the readiness
deadline. Ctrl+C can cancel a stalled output pipe too. An output failure exits
nonzero and may leave partial text or JSON; it never stops services.

### See what diagnostics can cover

A `requires` entry can keep a broken service from starting. It does not, by
itself, watch that file after startup. Likewise, keeping events on disk does not
turn on incident recording. Check those gaps before you need the evidence:

```bash
devd coverage                         # declarations in the selected disk YAML/profile
devd coverage --online               # effective config and live recorder observations
devd coverage --online --json         # the same explanation as schema-1 JSON
```

The default mode needs no supervisor. It reads at most 1 MiB of configuration,
applies `--profile`, and explains the declared services, `requires` selectors,
named paths/scopes, health checks and resource limits. `cwd_resolved` describes
the effective working directory, including the default, rather than proof of
an explicit YAML entry. No application path is inspected; missing files,
programs or dotenv files do not prevent a coverage report for valid YAML.
It never starts a service/probe, binds a port, creates cleanup-owned paths,
enables monitoring or grants control. Use `doctor` to inspect prerequisites.

`--online` requires the selected live endpoint and reads its effective config,
instance/run identity, observed service states and recording status. Edits on
disk take effect in this report only after successful reload or a new start;
even removed YAML can still address its original running instance. Recording
options come from that run's startup flags. Old files left in the state
directory cannot establish that a recorder is enabled now. No live connection
means an error, with no static fallback and no stale-health claim.

| Reported evidence | Meaning |
| --- | --- |
| `unknown` | Static inspection did not observe the runtime recording choice |
| `disabled` | This run did not enable that recorder |
| `recording` | The recorder is enabled and has no observed failure; completeness is not guaranteed |
| `failed` | This run observed a recorder failure; previously retained evidence may be incomplete |
| Known gaps | Findings identify memory-window eviction, retained event truncation, missed incident triggers/context, or this recorder's artifact eviction, with their source and consequence |

Events, application logs and incidents have independent states. Event or incident
writer failure leaves services running; the existing disk-log failure policy
stops the stack, so a failed log writer may already be unavailable to inspect.
Incident eviction counts can include artifacts from earlier runs. Event-memory
eviction says nothing about whether a disk copy survived.

Capacity figures describe rotation, not hours of history: events/logs use the
run's maximum bytes per file and archive count; incidents keep at most 20
artifacts of at most 2 MiB each; the event buffer holds 1024 events. This command
does not scan stored evidence. Disk-writer lag, unfinished artifacts and losses
outside the observed counters remain unknown; use `events --stored` and
`incidents ... --stored` after shutdown to inspect retained history.

A configured monitor is not proof of a successful sample. Path monitoring only
runs during managed process generations, ends when a task completes, and can
miss changes between polls. It checks declared conditions, not file contents.
A named path alone enables no monitor, though the same path can separately be
listed in `requires`. CPU/RSS samples cover the leader PID and may be unavailable.
Implicit dependencies remain unknown; coverage is neither a health verdict nor
a reliability score.

Text and JSON share the same findings. JSON marks the source, collection time,
`atomic: false`, and omitted service/resource counts. Live state and recorder
observations are not an atomic snapshot. Reports show at most 256 services and
512 combined requirement/path entries, with a 2 MiB output cap. Collection is
limited to 30 seconds, output to 5 seconds; Ctrl+C cancels only this query.
Exit `0` means a report was produced, even with gaps; exit `1` means collection,
connection, validation, cancellation or output failed (output may be partial).
Invalid CLI syntax exits `2`. Resource path values, commands, environment bodies
and application logs are omitted; service/binding names and the config path can
still be sensitive. This command does not add Agent or MCP tools.

### Export diagnostics

When a failure needs to be shared, capture one bounded report from the selected
live supervisor:

```bash
devd export --output diagnostic.json
devd export --profile staging --state-dir ./runtime --output staging.json --include-logs
```

The new JSON file includes instance and run identity, service state with PID and
generation, up to 1,000 recent lifecycle events, and an explanation for each
current or recently observed service (up to 256). Event cursor, `gaps`, and
`omitted_gaps` expose lost or tail-limited history. `captured_from`,
`captured_to`, `event_watermark_after`, and `stable_during_capture` describe the
sampling interval. A stable bracket means no state or event-watermark change
was observed while collecting; it does not make independent sources atomic or
promise that the processes remain in that state afterward. An unstable report
is still evidence, but its state and events may refer to different moments.

`omitted_fields` lists deliberately excluded free-text fields, so an absent
error string does not mean no error occurred. Application logs are absent by
default. `--include-logs` adds up to 200 raw
in-memory entries; review them before sharing. State error text and reload
failure text are omitted, as are configuration commands and environment values.
This is a field allowlist, not general-purpose secret redaction: service names,
local configuration/state/worktree paths, path-condition paths, Git context,
and any explicitly included logs can still reveal private information. The
report is written as a new file using a temporary file and never overwrites an
existing path. It works after the YAML is removed while the supervisor is
running. Live mode does not read stored history; neither mode controls services.

**The supervisor has already stopped? Export what was actually retained.**
Event persistence must have been enabled for the run you need:

```bash
# Before reproducing a failure:
devd start --persist-events
# After shutdown, even if devd.yml has since been removed:
devd export --stored --run latest --output retained.json
# Pin the UUID from identity/events or a previous report:
devd export --stored --run 123e4567-e89b-12d3-a456-426614174000 --output selected-run.json
```

`--stored` requires `--run UUID|latest`. `latest` means the last run in retained
file order, not the most recent supervisor launch or wall-clock timestamp. An
unknown/fully rotated-away run is an error. Select the same config/profile/state
as at startup; no YAML is loaded, endpoint contacted, process probed or current
resource inspection substituted for missing evidence. Events never cross the
selected run boundary. Explanations follow each service's latest retained event
generation; older generations remain labeled in the event window.

Live CLI and Agent exports remain schema 1, adding `source: live`; old schema 1
reports without source are understood as live. Offline reports use schema 2 and
`source: stored`, reusing the event, explanation and safe-state models. Top-level
`identity` is null and `state` is empty: legacy event storage has run/profile/state
context but no full instance identity or Git snapshot. `stored.selection` records
the requested config/profile/state and derived `selected_instance_id`; this is
query provenance, not proof of historical instance identity. `events.context`
keeps the retained context as recorded, even if files were moved. Capture times
refer to this export, event timestamps to the original observations;
`event_watermark_after` is the retained cursor, `stable_during_capture` is false,
and `stored.atomic` is false.

`stored.historical_state` describes the optional `services.json`: `missing`,
`invalid`, `unknown-run`, `different-run`, or `retained-matching-run`. Only a
matching run contributes allowlisted historical service fields. They remain
`stale: true`; `file_modified_at` is filesystem metadata, not an observation time.
Historical PIDs never authorize control, and historical state does not feed
current-health explanations. `stored.live_state` is `unavailable-offline` and
explanation `status` remains null. This still cannot certify what a process is
doing now, including after a forced supervisor termination.

`begin_retained` and `end_retained` report stored writer markers independently.
`event_coverage_complete` is false for missing markers, event gaps, truncated event
content or the bounded tail. Sequence gaps can indicate rotation loss, while
`unfinished-record` means the current file ends mid-record; that fragment has no
reliable run attribution. Complete malformed records or unfinished archives are
errors. An end marker proves the writer closed its stream, not that every process
stopped successfully. Missing history is never reconstructed.

Offline `--include-logs` adds a separate `stored.unassigned_logs` attachment of
at most 200 raw retained entries. The old log format has no run ID: these entries
may span runs, are explicitly `run_attribution: unknown-no-run-id-in-log-format`,
and never feed the selected run's explanations. Rotation/run coverage stays
unknown; `omitted_entries` and `unfinished_bytes` expose the observed tail limits.
An absent log directory is reported as missing. Default export does not read it.
The attachment is marked potentially sensitive, and the same field allowlist
and path-privacy limitations as live export apply.

Offline collection holds the existing `services.json.lock` in shared mode and
uses the existing event/log reader locks. An active supervisor/writer, missing
state lock, or linked/special/multiply-linked managed file is rejected. It creates
no source directories or lock files. Each history scan is limited to 64 MiB and
101 managed files; event selection additionally limits retained runs to 256,
output events to 1,000, and service names to 256. Historical state is at most
1 MiB, and the final JSON for either mode is at most 16 MiB; exceeding a bound
fails without publishing a partial report.

Both CLI modes use a 30-second collection/preparation deadline and Ctrl+C
cancellation, then publish a temporary file without replacing an existing target.
Cancellation accepted before publication prevents a background task from creating
a late report. Once publication is accepted it is allowed to finish; cancellation
does not remove an already saved report. Checks run between filesystem calls, so
an individual stalled OS call cannot be forcibly interrupted. The saved-file
confirmation has its own five-second output deadline.

### Keep bounded incident evidence

When the failure is intermittent, enable recording before reproducing it:

```bash
devd start --record-incidents
```

This opt-in is independent of event/log persistence, path monitoring, restart and
cleanup permissions. It records nonzero or signal exits not preceded by an
observed stop, spawn/process/actor failures, failed service states, unhealthy
transitions, dependency failures/timeouts, resource-limit violations and failed
path-condition transitions. Successful exits and manual controls alone do not
trigger a capture. File failures are observed at startup or by an explicitly
configured `monitor-requires`; this flag does not enable monitoring by itself.

Triggers for the same service and generation merge into a five-second follow-up
window starting when the collector receives the first trigger. The deadline is
not extended by repeated failures. Each capture retains up to 200 prior and 200
following events, with a separate 256 KiB budget for each tail. At most four
captures may be collecting, queued or writing. Saturation drops new triggers;
subscriber gaps, dropped triggers, merged triggers and omitted context are
counted. Coverage loss is also reported on stderr when the collector finishes.
Stop, reload and generation replacement end existing windows early with their
reason; a later failure may start its own capture. Control events and outcomes
are included only if observed within the window, never inferred from a request.

Files live in the selected instance/profile's `incidents/` directory. Schema 2
JSONL records contain a durable `begin`, a `resources` observation and an `end`.
The beginning preserves instance/run identity, trigger generation, prior events,
allowlisted state and an effective-runtime configuration summary observed after
the trigger. That state/configuration may already belong to a later generation
or reload: observation time and generation are explicit, and trigger-time
equivalence is not guaranteed. Fresh post-trigger inspections reuse the bounded
path/link resolver for at most eight declarations of the triggering service.

Schema 2 also retains the effective declared dependency graph and the recorded
resource-match basis used to derive direct consumers and downstream services.
`incidents show` presents that DAG as a readable tree; a `[ref]` marks a shared
service already expanded elsewhere, while JSON preserves all recorded edges. The
view is a declaration-based impact hypothesis, not access tracing or a causal
claim. The configuration summary retains at most 256 services, 64 dependencies
per service and 128 KiB. The relationship section is capped at 512 KiB with at
most 1,024 edges; resource projections share a batch of at most 256 distinct path
inspections, examine at most 256 declarations per selected resource and retain
at most 64 matches per resource. Tree output is capped at 128 KiB plus coverage
notes. Omissions, uncertain aliases, and interrupted work remain explicit;
an incomplete service graph cannot produce a complete resource projection.
After the target is repaired or YAML is removed, readers use only these retained
facts. Schema-1 artifacts remain readable with declared impact explicitly
`unknown-not-recorded-by-schema-1`; devd never rebuilds them from current files.

Each artifact is limited to 2 MiB, and at most 20 are retained, including partial
files. Existing artifacts are ordered by modification time/name at startup;
new artifacts follow admission order, and active captures are protected from
eviction. Eviction counts are recorded for the current recorder. Only UUID-named
incident files are managed; other files, logs, events and snapshots are preserved.
The directory has a separate writer lock, files are created without overwriting,
and links, reparse points, special files and multiple hard links are rejected.

One bounded subscriber and one blocking worker keep disk I/O off the supervisor
control loop and application output drain. Path checks stop between OS calls at
the deadline or an early ending. Shutdown drains accepted writes before releasing
the state lease; a single stalled filesystem call cannot be forcibly interrupted.
Setup failure starts no services. **A runtime recorder failure disables recording
for this run, reports on stderr, and leaves services running.** Missing `end` or
unfinished JSONL means incomplete evidence. Cancellation reports unavailable
counts as null; a killed supervisor cannot complete the tail retroactively.

Artifacts omit application logs, full YAML, commands, environment/dotenv contents
and free-text errors. Paths, link targets and identifiers can still be sensitive.
`export --stored --run` reads independently persisted event history; incident
queries and `export --incident` read these incident files.

### Find and export a recorded incident

```bash
devd incidents list --json
devd incidents show INCIDENT_UUID
devd export --incident INCIDENT_UUID --output incident.json

# After stopping the supervisor; YAML may already be gone:
devd incidents list --stored --json
devd incidents show INCIDENT_UUID --stored --json
devd export --stored --incident INCIDENT_UUID --output saved-incident.json
```

Online queries bind to the selected supervisor's instance and current run; they
require incident recording in that run. Offline queries select the same
config/profile/state directory and list all retained runs for that instance.
Select one full UUID for show/export; `--incident` and `--run` are exclusive.
Results sort by recorded observation time descending, then UUID. Neither mode
rechecks today's target, reads current YAML or substitutes a newer service state.

Query JSON is schema 1. Incident exports are schema 3, using the existing safe
event/explanation model with an `incident` evidence section; ordinary live
exports and Agent remain schema 1, retained-run exports remain schema 2. Incident
reports have `source: stored` even when fetched online: top-level live identity
and state stay empty. Historical identity, observed state/generation, configuration
summary, resource chains, control outcomes and omissions live in their recorded
sections. Explanations use only the trigger service's captured generation.
Schema-1 artifacts do not retain a complete declared impact view: declared impact is
explicitly `unknown-not-recorded-by-schema-1`, never rebuilt from current config.

A completed window, early ending, missing end, truncated/corrupt suffix and
unsupported schema are distinct. A valid prefix remains inspectable if later
records are incomplete; unsupported versions expose bounded diagnostics, not
unvalidated content. `complete` describes coverage of the bounded recorded
window, not a complete run, root-cause proof or a complete resource impact map.
Retention eviction counts cover only the recorder that wrote them; a missing
UUID may have been evicted, and absence is not proof that no incident occurred.

`--include-logs` is a separate opt-in export attachment, capped at 200 entries.
Online logs belong to the selected current run but are not the incident window;
offline disk logs have unknown run attribution. Neither attachment participates
in the incident explanation. Default reports omit logs, commands, environment
values and free-text failures; paths and identifiers may still be sensitive.
These reports are evidence, not restorable sessions or database backups.

Readers preserve state/writer leases and reject links, reparse points, multiple
hard links and special files. Live reads run on at most two blocking workers,
serialized with recorder writes; stop/disconnect cancels the reader between OS
calls. Offline reads require the supervisor to be stopped. Directory scans are
limited to 256 entries, lists to 20 artifacts (2 MiB each), and output to 16 MiB.
If more than 20 files exist, selection is by filename and the omission is explicit.
File replacement detected during reading is reported instead of returning mixed
evidence. Queries/preparation expire after 30 seconds; terminal output after five.
Ctrl+C cancels reads/output, and exports share the existing cancellation and
no-overwrite publication rules. One stalled OS call cannot be forcibly interrupted.

To reproduce the original kind of failure on Linux/macOS with Python 3, use a
new temporary directory. In terminal A:

```bash
incident_demo=$(mktemp -d)
cd "$incident_demo"
printf 'keep me\n' > target.txt
ln -s target.txt resource
cat > devd.yml <<'YAML'
version: '1'
services:
  worker:
    command: python3 -u -c 'import time; print("ready"); time.sleep(600)'
    restart: {policy: never}
    requires: [{type: file, path: resource}]
    monitor-requires: true
YAML
pwd  # Use this directory in terminal B.
devd start --record-incidents
```

In terminal B, `cd` to that printed directory, run `devd wait --timeout 30s`, then
`rm ./target.txt`. After the monitoring event, `devd incidents list` shows the UUID;
`devd incidents show UUID` shows the captured dangling-link chain. The window
closes after five seconds unless stop/reload/replacement ends it earlier.
Run `devd stop`, then use the `--stored` commands above. Restoring `target.txt`
later does not rewrite the saved observation.

### Compare two transferred reports

Two incidents can look similar while their evidence says different things. Keep
the comparison explicit and offline:

```bash
devd compare before.json after.json
devd compare before.json after.json --json
```

Both files are first passed through the same bounded `diagnose` validator used
for transferred evidence. The command needs no YAML, state directory, original
paths or running supervisor, and never follows a path, URL or command embedded
in either report. It writes no files and has no control or Agent/MCP operation.

The JSON schema is version 1. It reports each side's captured scope and then
labels configuration/profile/instance/platform/scope as `comparable`,
`different`, or `unknown`. Resource observations are keyed by their recorded
selector; requested and observed path identities are hashed, while observation
intervals and evidence source remain visible. A changed link target or
declaration is therefore a retained observation, not a claim about file
contents. Recorded relationship edges, service states, recomputed historical
explanations, incident-trigger data hashes and evidence gaps are compared
separately. Trigger payloads are never printed, and a trigger difference is
not a shared-cause claim. Missing, truncated,
unsupported or incomparable fields become `unknown`, never a fabricated
change. Similar symptoms do not become a shared root cause, recurrence or
recovery-effect conclusion.

Log attachments remain metadata only. Their counts and attribution may differ,
but log text is never used as incident-window evidence. The output keeps at
most 2,048 changes and 16 MiB; each input is the existing 16 MiB regular-file
limit. Collection and output are bounded to 30 and 5 seconds, and Ctrl+C
cancels the comparison. Exit `0` means a comparison report was produced,
including unknowns; malformed, unsupported, inconsistent, over-limit or
unavailable inputs exit `1`, and invalid CLI syntax exits `2`.

### Diagnose a report on another machine

The project can disappear while the evidence stays useful. Copy just the exported
JSON file to another directory or machine with devd installed:

```bash
devd diagnose incident.json
devd diagnose incident.json --json
# Live and retained-run reports work too:
devd diagnose diagnostic.json --json
devd diagnose retained.json
```

`diagnose` reads export schemas 1 (live), 2 (retained run) and 3 (incident),
including older live reports without `source`. It needs no YAML, state directory,
registry or running supervisor. `--config`, `--profile` and `--state-dir` are not
selection options here. It never follows embedded paths or URLs, executes report
content, contacts the original endpoint or controls a process.

Text and JSON use the same historical facts: capture interval, recorded identity
and stale state, sequence-ordered timeline, gaps, recomputed explanations and,
for incidents, retained resource observations and declared impact. Text projects
the recorded DAG as a tree with shared-node references. Missing relationships
remain unknown. Incident explanations stay on the trigger's generation; other
reports explain the latest retained event generation, with captured state shown
separately. Imported explanations are ignored. Recovered services are not blamed
for an earlier generation's failure. No result establishes current health or
proves causation; schema validation does not authenticate a report.

JSON output is diagnosis schema 1 with `source: imported-report`. A valid report
can return success while containing incomplete evidence or an unsupported nested
incident schema; inspect its diagnostics. Unknown export schemas, malformed or
truncated JSON, inconsistent event identities/order and exceeded limits fail.
Missing incident end records and damaged optional relationship sections preserve
valid recorded evidence and explicit gaps. Diagnostics that cannot be rechecked
from the surviving records are labelled `imported-reader-reported`. Raw logs are not displayed or used in
explanations; attachment counts and run attribution stay separate. Paths and
identifiers can still be sensitive.

Input must be a regular single-link file, at most 16 MiB; symlinks, reparse points,
special files and detected replacement during reading are rejected. Limits keep
at most 1,000 events, 256 service names, 129 gap records, and 200 entries per log
attachment; embedded incidents retain their original 2 MiB limit. Preparation
expires after 30 seconds and output after five; Ctrl+C cancels either phase.
Terminal fields are escaped. The command writes only to stdout and does not
create a state directory or repair files.

### Explicit instance ports and paths

Two worktrees should not silently fight over the same API port or write into the
same scratch directory. Declare the addresses and paths your application reads:

```yaml
version: "1"
services:
  api:
    command: python api.py
    ports:
      API_ADDR: 127.0.0.1:3100
    paths:
      API_DATA: {scope: instance, path: api-data}
      SHARED_DATA: {scope: shared, path: ./datasets}
  web:
    command: python web.py
    depends-on: [api]
profiles:
  second:
    services:
      api:
        ports: {API_ADDR: 127.0.0.1:3101}
```

`api`, its script probes, and its direct dependent `web` receive `API_ADDR`,
`API_DATA`, and `SHARED_DATA` in their environment. `API_ADDR` is the complete TCP
address (IPv6 uses `[address]:port`), not a URL or a numeric port alone. Applications
must read it themselves; devd does not rewrite commands, URLs, or arbitrary YAML.
Use concrete unicast addresses for mappings; `listen` still supports wildcard
bind declarations. A script probe can read the same environment; built-in TCP/HTTP
probe fields remain explicit.

An `instance` path resolves beneath `<selected-state-dir>/runtime/`; the example
uses a different `api-data` path for each profile/worktree. Its components must
use lowercase ASCII letters, digits, `_`, `-`, or `.`, without parent traversal,
device names, spaces, or trailing dots. A `shared` path resolves against the
declaring service's `cwd`, including when passed to a dependent. By default,
your application prepares its own runtime directory; shared data stays shared.
Explicit `cleanup: true` creates a disposable directory as described below.
These are configuration scopes, not
a filesystem sandbox or permission to delete existing files or follow symlinks
during cleanup.

Binding names use uppercase ASCII environment names. A binding conflicts with
an explicit `env` entry (case-insensitively) or another binding visible to the
same service; inherited and dotenv values are overridden. Only direct dependency
bindings are injected. `ports` and `paths` are each replaced as a whole by a
profile; `{}` clears that map. Reload previews show the changed fields and
downstream impact, and apply uses the new environment for affected generations.

`check` rejects conflicting address declarations and overlapping instance-owned
paths. `doctor` attempts to bind declared `ports` and `listen` addresses, then
releases them. **That does not reserve a port.** Another process can take it
before startup; application bind errors still appear in its logs and exit/health
evidence. devd does not automatically allocate ports or infer failure causes.

### Detect environment drift

A link can still be valid after it starts pointing at the wrong directory. Readiness
checks catch broken prerequisites; an explicit baseline lets you compare the
resources you chose before and after the environment changed.

```bash
devd baseline capture --service api --resource requires:0 --resource cwd --output baseline.json
devd baseline check baseline.json --json
# After reviewing a change, explicitly capture a new file to accept a new baseline.
devd baseline capture --service api --resource requires:0 --resource cwd --output accepted-baseline.json
```

Repeat `--resource` for `requires:INDEX`, `paths:NAME`, `cwd`, `program` or
`env-file`. `program` inspects only an explicit path in the first command token:
`./bin/api` works, `api` does not. It neither searches PATH nor guesses a shell's
interpreter or scripts. All relative paths use the normal config/service cwd
rules; instance bindings use the selected state directory. Capture does not
check readability, execute programs, read resource contents, create mapped
directories, or contact a supervisor. It writes a new baseline and a small local
origin marker at `<selected-state-dir>/baseline-origin.id`.

`check` uses the selected **on-disk effective configuration**, including
`--profile` and `--state-dir`, not a supervisor's in-memory configuration. It
resolves imported selectors through that configuration; recorded paths never
become arbitrary file-read directives. JSON schema 1 contains both recorded and
current observations, capture intervals, comparison reasons and current declared
consumers/downstream impact. This JSON is a baseline comparison, not an input
format for `diagnose` or the MCP report adapter.

Results distinguish `no-observed-change`, `changed` and `unknown`. Changes or
unknown comparisons return a nonzero exit code after printing the report. Missing
paths can be captured, so loss/recovery is observable; accepting a baseline does
not assert that its resources are healthy. Declaration changes, missing selectors,
foreign context, unsupported chains and observations that changed during inspection
remain unknown. Nothing repairs a file, restarts a process or changes readiness.

Local provenance uses the origin token, marker file identity, platform, host hint
and config/profile/state paths. Checking never creates a missing origin. Moving
or copying a baseline into another context normally makes it incomparable; file
identities are not portable, and cloning the origin/context does not authenticate
a machine. Keep the original baseline; accepting changes requires a new explicit
capture, and existing destinations are never overwritten.

Observations are non-atomic metadata and link targets, **not content equality or
fault causality**. Ancestor directory child activity is ignored; Unix records local
file identity, while Windows records attributes/creation time with file identity
explicitly unavailable (`comparison.complete: false`). Paths and targets may still
be sensitive. Selection is limited to 32 resources, baseline input/output to 2 MiB,
configuration input to 1 MiB, and comparison output to 16 MiB. Path traversal uses
the existing 128-node/40-link limits; impact uses 256 services/1,024 edges and a
shared bounded observation cache, showing incomplete alias coverage. Preparation
has a 30-second cancellable deadline and output a 5-second deadline; a single OS
filesystem call cannot be interrupted. Cancellation before baseline publication
prevents a late baseline file; already accepted publication may complete, and the
origin marker may already exist. Runtime monitoring and recovery remain separate
features.

### Run a reviewed recovery recipe

After diagnosing a lost generated cache, stop the **whole stack** and wait for
the foreground supervisor to exit successfully. Recovery is a separate explicit
action; monitoring, baselines and `cleanup: true` do not run repair commands.
Only current `scope: instance, cleanup: true` paths previously created and
registered by devd are eligible. Shared or unregistered paths are refused.

Create a standalone `recipe.yml`, replacing the program/script paths below with
your own reviewed generator:

```yaml
version: 1
service: api
resources: [API_CACHE]
steps:
  - id: generate-cache
    resource: API_CACHE
    command: ["/absolute/path/python", "/absolute/path/rebuild-cache.py"]
    timeout: 30s
    requires: []
    ensures: [{type: file, path: generated.json}]
```

```bash
devd stop
devd recover --recipe recipe.yml --dry-run --json
devd recover --recipe recipe.yml --apply --plan PLAN_ID --json
# After reviewing a completed result, explicitly restart and verify readiness:
devd start  # foreground, in another terminal
devd wait --timeout 30s
```

Use the same config/profile/state options throughout. A preview writes nothing
and runs nothing. Its five-minute `sha256:...:TIMESTAMP` plan binds configuration
and recipe bytes/file identities, ownership/run records, directory inventory and
program metadata. Apply rechecks those inputs and, before each new operation,
checks the plan deadline and resources again. These observations are not atomic
and do not prove identical file bodies. Keep other writers stopped.

Missing **registered leaf directories** are recreated with a new ownership
marker; their existing parents must remain intact. Existing owned contents are
retained. Replaced directories, links/reparse points, hard links, special files,
interrupted cleanup and overlaps with shared mappings are refused. No built-in
delete action exists. Each command runs in its selected resource directory;
relative program paths resolve from the service's effective cwd, while recipe
file paths resolve from the invoking terminal. Programs must be explicit paths,
including `C:/...` on Windows; PATH lookup and implicit shell expansion are absent.
`requires` and `ensures` accept plain relative file/directory paths within that
resource, reject traversal and links, and never access resource bodies.

**Commands run with your OS permissions, not in a sandbox.** An explicit shell
can still be declared. The command inherits the launcher environment, not the
service's dotenv/env overrides. stdin is closed and stdout/stderr discarded;
previews and receipts omit argument bodies and application output. Paths remain
potentially sensitive. devd holds the instance state lock through execution and
process-tree cleanup, so a new supervisor cannot race recovery. Ctrl+C cancels
remaining steps and stops the owned command tree; timeout or failure also stops
later steps. There is no rollback or automatic service start.

Schema-1 results and per-step accepted/completed/failed/cancelled/timed-out/unknown
evidence are synced to `<state-dir>/recovery-receipts.json`. A successful command
also needs its declared postconditions; that proves neither service readiness
nor a repaired root cause. Reapplying an attempted plan returns its retained
receipt with `already_attempted: true`, without re-executing or certifying today's
files. A recipe digest may be attempted only once per run. For a known failure,
inspect effects and explicitly revise the recipe or complete a new normal run
before making another plan. Forced termination or uncertain cleanup leaves the
instance unsafe for start/clean/recovery: preserve evidence and manually reconcile;
do not edit ownership records to bypass it. Historical PIDs are never signalled.
Recovery does not erase incidents or accept a new drift baseline. Agent/MCP grants
do not expose recovery in this module.

Limits: 16 resources, 32 steps, 128 arguments per command (8 KiB each), 32
preconditions and 1–32 postconditions per step, and 5 minutes per command. Recipe
input is a regular single-link file capped at 256 KiB; configuration is 1 MiB.
At most 64 receipts (1 MiB total) are retained without automatic eviction; reaching
the limit requires explicit archival/reconciliation. File preparation is bounded
to 30 seconds per operation and stdout to 5 seconds; a stalled OS call cannot be
forcibly interrupted and its worker retains the state lease until it exits.

### Clean disposable instance data

The accident that started this project is a good reason to make deletion explicit.
`scope: instance` alone grants no deletion permission. For scratch data you can
throw away, opt in on a dedicated path:

```yaml
services:
  api:
    command: python api.py
    paths:
      API_CACHE: {scope: instance, path: api-cache, cleanup: true}
```

On `start`, devd creates and registers this directory before starting services.
**This authorizes discarding the directory and everything subsequently placed
inside it.** Existing unregistered directories are never adopted. Keep durable
data elsewhere. The option defaults to `false` and is only valid for instance
paths; `check`, `doctor`, and previews do not create these directories.

After stopping the stack, wait for the `start` process to exit successfully:

```bash
devd stop
devd clean --dry-run --json
devd clean --apply --plan sha256:... --json  # use the returned plan_id
```

Use the same `--config`, `--profile`, and `--state-dir` throughout. Both commands
take the instance's existing state lock and require a recorded successful end
to its last owned run. An unreachable supervisor, a forced exit, or a failed run
does not establish that condition: cleanup and reuse refuse until the resources
are manually reconciled. devd never signals a PID from old state to make cleanup
possible. Preserve uncertain data and use a new dedicated state directory if
you need to resume work; do not edit ownership records to bypass the refusal.

A plan checks the current YAML permission, creation records, directory identity,
ownership marker, and a bounded tree inventory. Configuration bytes, run identity,
records, or observed tree changes invalidate it. Apply preflights every candidate
before deleting any. Shared mappings (including resolved aliases), unregistered
paths, logs, events, snapshots, and instance metadata are retained. Revoking
`cleanup` retains the old directory. Changes to cleanup declarations require a
full stop/start; reload previews mark them unavailable for application.

Deletion is not a transaction. A failure can leave partial progress; correct the
reported problem and preview again before retrying. Completed plans can be
reapplied harmlessly, even if a new user directory appears at the same path.
Missing directories only retire their records. JSON uses schema version 1 and
reports `removed`, `already_absent`, `already_applied`, and failures explicitly.

Keep external writers stopped during cleanup. Links/reparse points, hard links,
special files, filesystem crossings, nested devd ownership/state markers, and
non-UTF-8 names are refused. Limits are 128 registered roots, 10,000 entries per
root, and 64 nested directory levels. Inventories inspect metadata, not file
contents; they are not content hashes or protection against a malicious process
running as the same user. Parent directories and ownership records remain so
that retries and subsequent starts can check what happened.

### Connect an MCP client

Give your client a view of the running stack, then grant only the controls it
needs. `devd mcp --stdio` uses MCP JSON-RPC; the JSON-lines `agent` interface below
remains available. Start the supervisor first. For clients that use `mcpServers`,
a read-only configuration looks like this (replace both absolute paths):

```json
{
  "mcpServers": {
    "devd": {
      "command": "/absolute/path/to/devd",
      "args": ["mcp", "--stdio", "--config", "/absolute/project/devd.yml"]
    }
  }
}
```

Add the supervisor's `--profile` / `--state-dir` selectors when needed. To grant
controls, append `"--allow", "restart,stop,reload,clean"` to `args`, selecting only
the required permissions. Optional `--restart-budget SERVICE=COUNT` entries cap
accepted restarts for named services in this adapter session. Add
`--deduplicate` independently when the client can send a stable `operation_key`
for every control. Neither option grants a control; both are off unless the
launcher supplies them. The session binds to one instance and run. Get both
IDs from `devd_identity`; each control takes
`"target": {"instance_id": "…", "run_id": "…"}`. A replacement supervisor requires
a new session. Client confirmations and tool annotations do not grant permission.

| Tool | Arguments / behavior |
| --- | --- |
| `devd_identity`, `devd_status` | No arguments; bound live identity and service states |
| `devd_events` | Optional `tail` (1–1000, default 100), `service`; retains explicit gaps |
| `devd_explain` | Required `service` |
| `devd_wait` | Optional `services` (default all), `timeout_ms` (1–3600000, default 30000) |
| `devd_export` | Optional `include_logs` (default false); returns JSON, never writes a file |
| `devd_incidents_list`, `devd_incidents_show` | List this run's retained incidents; show requires incident UUID `id` |
| `devd_restart`, `devd_stop` | Required `target`; restart also requires `service`; corresponding grant required. With deduplication, controls also require `operation_key` (1–128 bytes) |
| `devd_reload_preview`, `devd_reload_apply` | Optional `candidate`; apply also requires `target`, fresh `plan_id`, and `reload` grant |
| `devd_clean_preview`, `devd_clean_apply` | Same session after normal stop; apply requires `target`, fresh `plan_id`, and `clean` grant |

Tool results contain JSON in one text content block. Check `isError`, then the
report's `outcome`: receiving a report does not prove a wait/reload/cleanup
succeeded. Events and incident evidence retain their provenance and gaps; paths
and diagnostic messages may be sensitive. Explicit logs contain at most 200
recent entries. Relative candidates resolve from the adapter's working directory.

For a transferred report, replace the client arguments with
`["mcp", "--stdio", "--report", "/absolute/path/report.json"]`. This exposes only
`devd_diagnose`, with no arguments. It uses the same validator as `devd diagnose`,
loads only the launcher's chosen report, and labels all evidence as historical.
It needs neither the project nor a supervisor and rejects live selectors/control
grants. It cannot read another path supplied by a tool call.

MCP versions `2025-06-18` and `2025-11-25` are supported. Frames are limited to
16 KiB input and 8 MiB output; supervisor requests still have a 4096-byte limit.
At most eight requests and one control handler may be active. Oversized input,
duplicate outstanding IDs or excess concurrency close the adapter; oversized
output returns a correlated error. Initialization and ordinary calls have a
30-second deadline; wait follows its requested timeout. Output stalls time out
in five seconds. Cancellation, EOF and Ctrl+C end adapter work without stopping
the stack. Accepted controls may continue, so inspect status/events before retrying;
request IDs do not deduplicate actions. When `--deduplicate` is enabled, retry
the same control with the same `operation_key`; the server checks the request
digest, returns conflicts for reused keys with different payloads, and never
replays an unknown result. Entries are session-local, with up to 128 keys. Active entries do not expire;
completed/unknown entries remain for 10 minutes. New keys are rejected while
capacity is full. When a
restart budget is exhausted, authorized stop remains available. This is an adapter capability boundary,
not a same-user sandbox. The fixed-version official Inspector check is documented
in [tests/README.md](../tests/README.md); it does not claim model-driven client acceptance.

### A scoped interface for Agents

An Agent can inspect a stack without being handed its stop button. Start the
stack first, then launch a JSON-lines session with the same instance selectors:

```bash
devd agent --stdio                                   # read-only
devd agent --stdio --allow restart,stop               # selected controls
devd agent --stdio --allow restart --restart-budget worker=3
devd agent --stdio --allow restart,stop --deduplicate
```

The launcher grants permissions once. Requests cannot add permissions or change
config/profile/state selectors. Each session attaches to one **live instance and
run**; it never silently follows a replacement supervisor. Keep that session open
after stopping the stack to preview/apply cleanup. A new session needs a live
supervisor; standalone offline cleanup remains available through `devd clean`.
`--restart-budget SERVICE=COUNT` counts accepted restarts per named service in
this adapter session; it only applies after the normal restart grant and
rejects services outside the declared set. `--deduplicate` is independent and
makes `operation_key` mandatory on controls. Both policies are described below.
Ordinary CLI commands keep their existing permissions. These grants constrain
this adapter, not other programs running as the same OS user. No network daemon,
multi-user RBAC or automatic start is added.

Send one UTF-8 JSON object per line on stdin. stdout contains only JSON replies;
startup/I/O failures go to stderr and exit nonzero. A request looks like:

```json
{"schema_version":1,"id":"inspect-1","operation":{"method":"status"}}
```

Replies always carry `schema_version`, `id`, `instance_id`, `run_id`, and `ok`.
Successful dispatch has `data`; rejection has `error: {code, message}`. Invalid
envelopes use `id: null`. IDs are 1–128 bytes without control characters and are
for correlation, **not deduplication**. Unknown request fields and methods are
rejected. Clients should accept additive response fields and check schema 1.
`ok: true` means a report was returned: inspect `data.outcome` for wait, reload,
and cleanup. A timed-out wait or partial cleanup is not successful work.

| Method | Additional fields inside `operation` | Grant |
| --- | --- | --- |
| `describe` | None; returns capabilities and the identity observed at attachment | Read |
| `identity` | None; queries the bound live supervisor | Read |
| `status` | None; returns `observed_at` and a safe `services` state map | Read |
| `events` | `query: {filter: {}, tail: 100, cursor: null}`; existing service/kinds/since filters and run/sequence cursor apply | Read |
| `explain` | `service` | Read |
| `wait` | `timeout_ms` (1–3600000), optional `services` (default all); one final readiness report | Read |
| `export` | Optional `include_logs` (default false); returns the report without writing a file | Read |
| `incidents` | Optional incident UUID `id`; omitted lists this run’s retained incidents | Read |
| `reload-preview` | Optional `candidate` (default original configuration) | Read |
| `clean-preview` | None; requires the same run to have stopped successfully | Read |
| `restart` | `target`, `service`; uses normal restart/dependency behavior | `restart` |
| `stop` | `target`; acknowledgement means stopping, not completed shutdown | `stop` |
| `reload-apply` | `target`, `plan_id`, optional `candidate`; same candidate as preview | `reload` |
| `clean-apply` | `target`, `plan_id`; retains all directory ownership checks | `clean` |

Get the identity from `describe` or `identity`, and put both identifiers in every
control request. Permission is checked before I/O; the supervisor checks scope
again before execution. Offline cleanup checks the run under its state lock.

For a session launched with `--allow restart --deduplicate`:

```json
{"schema_version":1,"id":"restart-1","operation":{"method":"restart","service":"api","operation_key":"restart-api-1","target":{"instance_id":"<instance_id>","run_id":"<run_id>"}}}
```

`describe` and live `identity` expose `control_policy` limits and remaining
restarts when enabled (default `identity` is unchanged). Budgets allow 1–100
restarts per service, with at most 64 service entries. Validation and definite
supervisor refusal consume no allowance. A reserved allowance is not refunded
while its result is uncertain; cancellation or a lost reply cannot buy another
restart. The policy restricts manual Agent restarts and never initiates recovery.

With deduplication enabled, all four controls require `operation_key` (1–128
UTF-8 bytes, no control characters). The digest binds method, target and payload;
transport IDs are excluded. Keep candidate path spelling unchanged on retries.
Successful dispatch returns `{operation_key, operation_status, result?}` inside
`data` (or MCP's JSON text block). Status is `accepted` while queued in the
adapter, `executing` while forwarding/awaiting the use case, `completed` when a
report exists, or `unknown` when the result cannot be established. These states
describe the adapter's knowledge, not an atomic supervisor acknowledgement.
For completed reload/cleanup, inspect `result.outcome`; completed stop only means
`result.outcome: stopping`, never proof of shutdown. An unknown status may be
returned with `ok: true`: it is evidence availability, not success.

Each adapter session retains at most 128 operations. Accepted/executing entries
do not expire. Completed/unknown entries expire 10 minutes after finishing or
becoming unknown; queries do not extend retention. Capacity rejects new keys,
without evicting active operations. Results are capped at 256 KiB each and 8 MiB
total. An oversized result can be returned once but only an unknown tombstone
is retained, preventing replay without retaining the body. Every cache hit
rechecks grants and the attached instance/run; stopped-run verification is
read-only and does not execute cleanup. Definitive refusals release the key.
After expiry or adapter restart, a key is new again; budgets also reset on a
new session. There is no durable, cross-session or cross-adapter exactly-once
guarantee. Reconcile unknown effects using evidence, rather than submitting a
new key or reconnecting to bypass that uncertainty.

Relative candidate paths use the Agent process's working directory. Ordinary
diagnostics do not reread YAML. `status` and restart replies use the export state
allowlist, omitting free-text failures; `export` follows its existing privacy
rules. Events, explanations, readiness and error messages retain their existing
diagnostic detail, which can include local paths or command text. This interface
is not a universal redactor. Logs are included only on explicit export request.

The protocol processes requests serially. A line is limited to 16 KiB including
its newline; the wrapped supervisor request must fit 4096 bytes, replies are
limited to 8 MiB, and a stalled stdout write times out after 5 seconds. Oversized
input closes the session after an error; other malformed requests leave it open.
EOF drains accepted input; Ctrl+C exits the adapter and cancels a pending wait.
Neither implicitly stops services. Accepted controls, including a blocking
cleanup, may continue after disconnection; check status/events before retrying.
Use a separate session when a long wait must not delay another request.

Stable rejection codes are `invalid-request`, `unsupported-version`,
`permission-denied`, `wrong-instance`, `stale-run`, `operation-failed`,
`invalid-response`, `request-too-large`, `response-too-large`, and `input-failed`.
Control policies additionally return `operation-key-required`,
`operation-key-conflict`, `budget-out-of-scope`, `budget-exhausted`,
`deduplication-capacity`, `busy`, and `outcome-unknown`.
Messages provide detail but are not a parsing contract. Operation reports retain
their existing history gaps, stale-plan checks and partial-progress semantics.

### Named environments

Keep environment differences in the same YAML file:

```yaml
version: "1"
services:
  app:
    command: npm run dev
    env: {MODE: dev, LOG_LEVEL: info}
profiles:
  staging:
    services:
      app:
        command: npm run staging
        env: {MODE: staging}
```

`devd check --profile staging` validates the merged configuration; `devd start --profile staging` runs it. `LOG_LEVEL` is inherited. Use the same `--profile` for `status`, `logs`, `restart`, and `stop`. Omitting it selects the base configuration and a separate instance.

Services merge by name. `env` and `restart` merge by key; other fields, including dependency lists, path requirements, binding maps, and the entire health check, replace the base value. `null` clears optional fields such as `cwd`, `env-file`, and `healthcheck`; empty `env`/`restart` maps inherit, while `ports: {}`, `paths: {}`, `depends-on: []`, and `requires: []` clear their fields. New services need a command. Service deletion and profile inheritance are not supported. All definitions reject unknown fields, including unselected profiles; dependency and readiness validation applies to the selected result. `check` without a profile checks the base result.

Paths retain the usual configuration-directory and service-`cwd` rules. Profile names start with an ASCII letter, digit, or underscore and contain only ASCII letters, digits, `_`, `-`, or `.`. Names are case-sensitive. Runtime files use `.devd/<config-filename>/profiles/<name>/`; uppercase letters are escaped as `~hh` to stay distinct on case-insensitive filesystems. With an explicit `--state-dir`, the same `profiles/<name>/` suffix is appended. This isolates control and state files; service ports and application files still need distinct values when running environments together. `init` rejects `--profile`.

Try the [runnable dev / staging / prod example](../examples/profiles/README.md), which also walks through graph export, filtered logs, and snapshot restoration.

### Configuration snapshots

Save the whole configuration before a risky edit, then restore it under a new filename if you need to go back:

```bash
devd snapshot save before-refactor
devd snapshot restore before-refactor --output devd.recovered.yml
devd check --config devd.recovered.yml
```

The snapshot is an exact copy of the on-disk YAML, including every profile. It lives at `.devd/<config-filename>/snapshots/<name>.yml`, or under the directory selected by `--state-dir`; use the same `--config` and `--state-dir` to restore it even if the original file has been deleted. Names use 1–64 lowercase ASCII letters, digits, `_`, `-`, or `.`, starting with a letter, digit, or `_`.

`--output` must be a new filename in the original configuration directory, so relative `cwd` and environment-file paths keep their meaning. Neither save nor restore overwrites an existing file. Snapshots copy configuration bytes without validation; run `check` on the restored file before using it. They do not save runtime state, reload a running supervisor, or start or adopt processes. Since the YAML contains all profiles, `snapshot` does not accept `--profile`.

### Dependency diagrams

`graph` defaults to a text list of dependencies and parallel startup layers. Export the selected configuration as Graphviz DOT or Mermaid when a visual map is easier to scan:

```bash
devd graph --format dot > dependencies.dot
dot -Tsvg dependencies.dot -o dependencies.svg # Graphviz, if installed
devd graph --profile staging --format mermaid > dependencies.mmd
```

Diagram arrows point from each prerequisite to the service that depends on it; edge labels show the readiness condition. Services without dependencies also appear. `graph` validates the configuration but does not start services or create runtime state. DOT and Mermaid output are source text for their respective renderers, not image files.

### Preview and apply configuration changes

Editing one service can affect half the stack. Before restarting anything, ask the running supervisor what would change:

```bash
devd reload --dry-run
devd reload --dry-run --candidate devd.next.yml --json
devd reload --dry-run --profile staging --candidate devd.next.yml
```

Use the same `--config`, `--profile`, and `--state-dir` as the running instance. The baseline is the effective configuration held by that supervisor, even if its YAML has since changed. The candidate defaults to `--config`; `--candidate` chooses another file without changing the target instance. Its relative `cwd` values resolve against the candidate file's own directory, and the supervisor applies its existing profile to it. The candidate must contain that profile. Preview requires a live supervisor and a readable regular YAML file of at most 1 MiB.

The report lists `added`, `removed`, `modified`, `dependency-affected`, and `unchanged` services, changed field names, and the directly changed prerequisites behind each impact. It follows all downstream edges in both dependency graphs, regardless of `restart-on-dep-recovery`. The conservative plan stops affected old services in reverse dependency layers, then starts affected new services in forward layers. Unrelated services stay out of both lists. Configuration values, commands, and environment contents are excluded from a successful report; validation errors use the usual configuration diagnostics.

`--json` returns `schema_version: 1`, the supervisor `run_id`, base/candidate configuration fingerprints, process generations, and a `plan_id`. Apply explicitly using the ID you reviewed and the same candidate:

```bash
devd reload --apply --plan 'sha256:<64-hex-digits>' --candidate devd.next.yml
# Add --json for an execution report, including partial failure or interruption.
```

Application re-reads and validates the candidate, then recomputes the plan against the live instance. Changed configuration, process generations, or lifecycle states invalidate the ID; preview again. Services must be in stable states. `apply_available: true` describes support, not a guarantee that a later application will succeed. Instance/profile, state-directory, and supervisor logging options stay fixed. The candidate is an in-memory configuration update; neither source YAML nor the default candidate path is rewritten.

Only affected services are stopped, in old reverse dependency order. After they finish, the supervisor commits the candidate baseline and starts new forward layers. Each affected task must succeed; long-running services must start and, when configured, pass their health check; each layer has the existing dependency timeout (30 seconds). Unchanged services retain their processes. No-op configurations restart nothing. Manual restart and a second reload are rejected while application is active. Affected services pause automatic recovery until application completes; unrelated services keep their existing policies. `stop` can interrupt any phase.

A failure stops the whole stack, including unrelated services. There is no automatic rollback: once committed, the new in-memory baseline remains until the supervisor exits. The execution report lists completed stops, observed starts, ready layers, and `config_committed`; it returns a nonzero exit code on failure or interruption. Lifecycle events record the plan and outcome, with progress available through `events` (and `--stored` when event persistence was enabled). Client disconnect or the 60-second response timeout does not cancel an accepted reload; inspect status/events before retrying.

Choose exactly one of `--dry-run` or `--apply`. Preview validates YAML, quoting, settings, and the dependency graph without executing service/probe commands, reading dotenv contents, checking live path/port readiness, or writing runtime state. Only YAML definitions are compared; dotenv contents, inherited environment, and program-file changes need an explicit restart. File watching and automatic reload are deferred.

### Command reference

| Command | Purpose |
| --- | --- |
| `devd start [--persist-logs] [--persist-events] [--record-incidents]` | Start the stack; independently opt into logs, events and bounded incident capture |
| `devd stop` | Request ordered shutdown; the foreground process exits after cleanup |
| `devd restart <service>` | Restart one service using the current effective configuration, rechecking dependencies |
| `devd reload --dry-run / --apply --plan ID [--candidate PATH] [--json]` | Preview or explicitly apply affected service changes |
| `devd status [--json]` | Show live state, PIDs, CPU / RSS, restart counts, and diagnostics |
| `devd identity [--json]` | Show the selected live supervisor's instance and run identity |
| `devd instances [--json]` | Discover registered instances in the current repository and its worktrees |
| `devd incidents list/show [UUID] [--stored] [--json]` | Query recorded incidents in the current run or offline |
| `devd diagnose REPORT.json [--json]` | Diagnose a transferred report without the original project |
| `devd compare LEFT.json RIGHT.json [--json]` | Compare two transferred historical reports offline |
| `devd export --incident UUID [--stored] --output FILE [--include-logs]` | Export one recorded incident |
| `devd export --output FILE [--include-logs]` | Save bounded live diagnostics to a new JSON file |
| `devd export --stored --run UUID/latest --output FILE [--include-logs]` | Export one retained run after shutdown |
| `devd clean --dry-run [--json]` | Preview registered, explicitly disposable directories after successful shutdown |
| `devd clean --apply --plan ID [--json]` | Apply a current cleanup plan; preserve shared and unregistered data |
| `devd mcp --stdio [--allow restart,stop,reload,clean] [--report FILE]` | Serve MCP tools for one live run or an isolated historical report |
| `devd agent --stdio [--allow restart,stop,reload,clean]` | Serve a run-bound JSON interface with explicit control grants |
| `devd top` | Inspect a running stack and its live logs in an interactive terminal |
| `devd events [service] [--type TYPE] [--since DURATION] [--tail N] [--cursor RUN_UUID:NEXT_SEQUENCE] [--json] [--follow \| --stored]` | Query lifecycle facts, cursors and history gaps |
| `devd explain <service> [--json] [--stored]` | Explain the latest deterministic failure evidence for one service |
| `devd baseline capture --service NAME --resource SELECTOR --output FILE` | Explicitly accept selected resource metadata in a new baseline |
| `devd baseline check FILE [--json]` | Compare current disk resources; changes/unknown return nonzero |
| `devd recover --recipe FILE --dry-run [--json]` | Preview a normally stopped instance's explicit recovery recipe |
| `devd recover --recipe FILE --apply --plan ID [--json]` | Execute once with retained results and file postconditions |
| `devd coverage [--online] [--json]` | Explain declared coverage and known evidence gaps without probing or changing the stack |
| `devd doctor [--json]` | Check configured service prerequisites without starting them |
| `devd impact SERVICE --require INDEX/--path NAME [--scope project/worktrees] [--json]` | Show declared resource users and potential downstream impact |
| `devd logs [service] [--tail N] [--level info|warn|error] [--since DURATION] [--grep TEXT] [--follow \| --stored]` | Query live memory or offline disk logs; follow live output |
| `devd check` | Validate configuration, command quoting, dependencies, and supported settings |
| `devd graph [--format text|dot|mermaid]` | Show dependency edges and startup layers, or export a diagram |
| `devd init [--service NAME] [--command CMD]` | Create a checked starter configuration without overwriting an existing file |
| `devd snapshot save <NAME>` | Save the complete on-disk YAML under the project state directory |
| `devd snapshot restore <NAME> --output <FILENAME>` | Restore it to a new file beside the original configuration |

Commands accept `-c / --config <PATH>`, `--profile <NAME>` (except `init` and `snapshot`), `--state-dir <PATH>`, and `--color auto|always|never`. `instances` instead selects its scope from the current directory and rejects nondefault configuration paths, profiles, and state directories. Options work before or after the subcommand:

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

