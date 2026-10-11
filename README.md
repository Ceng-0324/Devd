# Devd

**English** · [简体中文](README.zh-CN.md)

***This thing*** keeps your local dev processes in order: start them by dependency, see what's running, find the logs, and shut the whole lot down when you're done.

Honestly, the inspiration came from one **fucking event**.

I accidentally deleted files from a `tmp` directory. Codex's `app-server` then ran into a dangling symlink: the link was still there, but its target was gone. Every session crashed.

Went in to clean up temporary files. Took every session down with them. Great.

That got me thinking about the relationships in a development environment that are easy to ignore until something breaks. Which process depends on which? Does “started” actually mean “ready”? Where do you look when something fails? What needs to come back first?

So I built devd: startup order, health checks, and process lifecycles in a configuration you can actually run.

Broken file dependencies still need fixing. devd takes care of the services you put under its supervision, giving the everyday business of starting, inspecting, restarting, and stopping them a home.

## What it does

devd is a local development service manager written in Rust. **v0.7.0-alpha.1 is available as a prerelease for Linux, macOS, and Windows.**

v0.7 is about keeping parallel development sessions out of each other's way. Find the instance running in each worktree, wait until its services are ready, export evidence when something goes wrong, and give an Agent only the controls you choose. Declare ports and paths explicitly, inspect a live event timeline, then preview and clean only registered disposable data after a normal stop. Shared files stay shared; another worktree keeps running.

v0.6 brings the original accident a little closer to something you can catch: declare required files, directories, and symlinks before a service starts, then opt in to observing them while it runs. When configuration changes, preview the affected services and explicitly apply the reviewed plan. File monitoring reports what changed; it does not repair files or authorize restarts. Reload failures stop the whole stack without automatic rollback. Publication requires native CI and archive verification on Linux, macOS, and Windows for the same source revision.

Published binaries and SHA-256 checksums are on [GitHub Releases](https://github.com/Ceng-0324/Devd/releases/tag/v0.7.0-alpha.1), with Linux x86_64, macOS arm64, and Windows x86_64 MSVC archives. This release passed native CI and archived-binary recovery checks on all three platforms.

The diagnostic tools introduced in v0.5 remain part of that workflow: `events` traces what happened, `explain` connects failures to recorded evidence, and `doctor` checks the environment before starting. The `listen` setting declares owned TCP ports.

It also includes opt-in disk logs, an interactive terminal view, CPU and memory samples, resource warnings and explicitly enabled recovery, custom script health checks, opt-in restarts after dependency recovery, named configuration profiles, configuration snapshots, dependency diagram export, and log filters.

Describe your services and their dependencies in `devd.yml`, then run `devd start` in the foreground. Use another terminal to check status, read logs, or restart a service.

- **Start in dependency order.** Independent services start concurrently. Dependencies can wait for a process to start or for a TCP / HTTP / Unix socket / script health check to pass.
- **Watch service health.** TCP and Unix socket connection checks and HTTP 2xx probes track consecutive failures and report what went wrong. Socket paths resolve relative to the service working directory; a stale socket file is not healthy.
- **Handle unexpected exits.** Choose `always`, `on-failure`, or `never`, with fixed or exponential retry delays and a limit on automatic restarts.
- **Bring the logs together.** Collect stdout / stderr with timestamps, service names, and colors. Query recent output for a specific service.
- **See what each service uses.** `status` shows CPU usage and resident memory for each service's main process.
- **Clean up on the way out.** Ctrl+C or `devd stop` shuts services down in reverse dependency order and cleans up owned descendants. Unix also handles SIGTERM; Windows handles Ctrl+Break.

It's for local projects with an API, a frontend, workers, or other processes that need to run together. Your services keep their existing startup commands; devd coordinates them.

## Get it running

You'll need Rust 1.95 or newer. Install from the repository root:

```bash
cargo install --path . --locked
```

Run `devd init` in a project directory to create a runnable one-service starter configuration (using `sh` on Unix and Windows PowerShell on Windows). Use `--service` and `--command` to set its service name and command; `--config` chooses the output path. The command refuses to replace an existing file. For a two-service Unix example, use this configuration:

```yaml
version: "1"
services:
  worker:
    command: sh -c 'echo worker-ready; exec sleep 3600'
    restart:
      policy: on-failure
      initial-delay: 1s
      max-attempts: 3
  client:
    command: sh -c 'echo client-ready; exec sleep 3600'
    depends-on: [worker]
    restart:
      policy: never
```

```bash
devd check       # Validate the configuration
devd graph       # Inspect dependencies and startup layers
devd start       # Run in the foreground with live logs
```

Leave that terminal running. Open another terminal in the same directory:

```bash
devd status
devd logs worker --tail 50
devd restart worker
devd stop
```

You can also press Ctrl+C in the terminal running `start`. Once it exits, the services have been cleaned up.

## Use your own services

Replace `command` with whatever you already use to start your service. Here's a Node.js API with a `/health` endpoint and a frontend that waits for it:

```yaml
version: "1"
services:
  api:
    command: npm run dev
    cwd: ./backend
    env-file: .env.local
    listen: [127.0.0.1:3000]
    healthcheck:
      type: http
      url: http://127.0.0.1:3000/health
      interval: 2s
      timeout: 1s
      retries: 3
    restart:
      policy: on-failure
      initial-delay: 1s
      max-attempts: 3
  web:
    command: npm run dev
    cwd: ./frontend
    depends-on:
      - service: api
        condition: http-ready
    restart:
      policy: never
```

This assumes you already have `backend`, `frontend`, their `dev` scripts, and `backend/.env.local`. The frontend starts after the API's health endpoint returns 2xx. The default dependency readiness timeout is 30 seconds.

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

## Commands

### Instance discovery (v0.7)

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

### Wait for readiness (v0.7)

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
others need `running` and a PID. Use the same `--config`, `--profile`, and
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
health requirement, observed state, PID, generation, readiness, pending manual
restart, and last error. Outcomes are `ready`, `failed`, `timed-out`,
`cancelled`, `stopping`, `reloaded`, `disconnected`, `unavailable`,
`invalid-service`, or `busy`. Timeout/cancellation/disconnect preserve the last
received evidence, which may be stale; identity and observation fields are null
if no report arrived. `waiting` is internal stream progress, not a final CLI
outcome. Argument errors may occur before a report can be produced.

Final output has a separate five-second write/flush deadline after the readiness
deadline. Ctrl+C can cancel a stalled output pipe too. An output failure exits
nonzero and may leave partial text or JSON; it never stops services.

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

### Keep bounded incident evidence (v0.8)

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

Files live in the selected instance/profile's `incidents/` directory. Schema 1
JSONL records contain a durable `begin`, a `resources` observation and an `end`.
The beginning preserves instance/run identity, trigger generation, prior events,
allowlisted state and an effective-runtime configuration summary observed after
the trigger. That state/configuration may already belong to a later generation
or reload: observation time and generation are explicit, and trigger-time
equivalence is not guaranteed. Fresh post-trigger inspections reuse the bounded
path/link resolver for at most eight declarations of the triggering service;
they never substitute for the historical path-condition event or prove causation.
Configuration summaries cap at 256 services, 64 dependencies per service and
128 KiB. Missing services, skipped declarations and interrupted inspections remain
explicit. No filesystem snapshot or restorable session is promised.

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
Schema-1 artifacts do not retain a complete M2 impact view: declared impact is
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

### Explicit instance ports and paths (v0.7)

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

### Clean disposable instance data (v0.7)

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

### A scoped interface for Agents (v0.7)

An Agent can inspect a stack without being handed its stop button. Start the
stack first, then launch a JSON-lines session with the same instance selectors:

```bash
devd agent --stdio                                   # read-only
devd agent --stdio --allow restart,stop               # selected controls
devd agent --stdio --allow restart,stop,reload,clean   # all four controls
```

The launcher grants permissions once. Requests cannot add permissions or change
config/profile/state selectors. Each session attaches to one **live instance and
run**; it never silently follows a replacement supervisor. Keep that session open
after stopping the stack to preview/apply cleanup. A new session needs a live
supervisor; standalone offline cleanup remains available through `devd clean`.
Ordinary CLI commands keep their existing permissions. These grants constrain
this adapter, not other programs running as the same OS user. No network daemon,
multi-user RBAC, automatic start, or MCP adapter is added.

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
| `reload-preview` | Optional `candidate` (default original configuration) | Read |
| `clean-preview` | None; requires the same run to have stopped successfully | Read |
| `restart` | `target`, `service`; uses normal restart/dependency behavior | `restart` |
| `stop` | `target`; acknowledgement means stopping, not completed shutdown | `stop` |
| `reload-apply` | `target`, `plan_id`, optional `candidate`; same candidate as preview | `reload` |
| `clean-apply` | `target`, `plan_id`; retains all directory ownership checks | `clean` |

Get the identity from `describe` or `identity`, and put both identifiers in every
control request. Permission is checked before I/O; the supervisor checks scope
again before execution. Offline cleanup checks the run under its state lock.

```json
{"schema_version":1,"id":"restart-1","operation":{"method":"restart","service":"api","target":{"instance_id":"<instance_id>","run_id":"<run_id>"}}}
```

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

Try the [runnable dev / staging / prod example](examples/profiles/README.md), which also walks through graph export, filtered logs, and snapshot restoration.

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

### Preview and apply configuration changes (v0.6)

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

Only affected services are stopped, in old reverse dependency order. After they finish, the supervisor commits the candidate baseline and starts new forward layers. Each affected service must start and, when configured, pass its health check; each layer has the existing dependency timeout (30 seconds). Unchanged services retain their processes. No-op configurations restart nothing. Manual restart and a second reload are rejected while application is active. Affected services pause automatic recovery until application completes; unrelated services keep their existing policies. `stop` can interrupt any phase.

A failure stops the whole stack, including unrelated services. There is no automatic rollback: once committed, the new in-memory baseline remains until the supervisor exits. The execution report lists completed stops, observed starts, ready layers, and `config_committed`; it returns a nonzero exit code on failure or interruption. Lifecycle events record the plan and outcome, with progress available through `events` (and `--stored` when event persistence was enabled). Client disconnect or the 60-second response timeout does not cancel an accepted reload; inspect status/events before retrying.

Choose exactly one of `--dry-run` or `--apply`. Preview validates YAML, quoting, settings, and the dependency graph without executing service/probe commands, reading dotenv contents, checking live path/port readiness, or writing runtime state. Only YAML definitions are compared; dotenv contents, inherited environment, and program-file changes need an explicit restart. File watching and automatic reload are deferred.

### Command reference

| Command | Purpose |
| --- | --- |
| `devd start [--persist-logs] [--persist-events] [--record-incidents]` | Start the stack; independently opt into logs, events and bounded incident capture |
| `devd stop` | Request ordered shutdown; the foreground process exits after cleanup |
| `devd restart <service>` | Restart one service using the current effective configuration, rechecking dependencies |
| `devd reload --dry-run / --apply --plan ID [--candidate PATH] [--json]` | Preview or explicitly apply affected service changes (v0.6) |
| `devd status [--json]` | Show live state, PIDs, CPU / RSS, restart counts, and diagnostics |
| `devd identity [--json]` | Show the selected live supervisor's instance and run identity (v0.7) |
| `devd instances [--json]` | Discover registered instances in the current repository and its worktrees (v0.7) |
| `devd incidents list/show [UUID] [--stored] [--json]` | Query recorded incidents in the current run or offline |
| `devd export --incident UUID [--stored] --output FILE [--include-logs]` | Export one recorded incident (v0.8) |
| `devd export --output FILE [--include-logs]` | Save bounded live diagnostics to a new JSON file |
| `devd export --stored --run UUID/latest --output FILE [--include-logs]` | Export one retained run after shutdown (v0.8) |
| `devd clean --dry-run [--json]` | Preview registered, explicitly disposable directories after successful shutdown |
| `devd clean --apply --plan ID [--json]` | Apply a current cleanup plan; preserve shared and unregistered data |
| `devd agent --stdio [--allow restart,stop,reload,clean]` | Serve a run-bound JSON interface with explicit control grants |
| `devd top` | Inspect a running stack and its live logs in an interactive terminal |
| `devd events [service] [--type TYPE] [--since DURATION] [--tail N] [--cursor RUN_UUID:NEXT_SEQUENCE] [--json] [--follow \| --stored]` | Query lifecycle facts, cursors and history gaps |
| `devd explain <service> [--json] [--stored]` | Explain the latest deterministic failure evidence for one service |
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

## A few things to know

**It runs in the foreground.** devd manages service tasks with Tokio. Commands in other terminals use a Unix socket, or a local Windows named pipe restricted to the current user. Runtime files default to `.devd/<config-filename>/` inside the configuration directory; add `.devd/` to your project's `.gitignore`. If a Unix socket path is too long, choose a shorter `--state-dir`. Use the same configuration and state directory when addressing the same instance.

**Windows uses native process ownership.** Windows 10/11 and Windows Server 2016+ use one Job Object per service generation, covering descendants before the service starts executing. Stop sends Ctrl+Break to that service's console process group, then terminates the job after the grace period; console-less services are terminated directly. Programs must handle Ctrl+Break to shut down gracefully. Closing or killing devd also closes its jobs. TCP, HTTP, script probes, `top`, profiles, snapshots, resource controls, and stored logs work through the same commands. Unix socket probes are rejected by `check` and `start` on Windows; use TCP or a script instead. TUI requires an interactive console.

Commands retain shell-style quoting on Windows. Prefer forward slashes in YAML paths, and quote paths containing spaces, for example `command: "'C:/Program Files/Python/python.exe' app.py"`. Invoke `powershell.exe -NoProfile -File script.ps1` or `cmd.exe /C ...` explicitly when needed. `devd init`, followed by `devd start` in one terminal and `devd status`, `devd top`, or `devd stop` in another, is a Windows quick start without Unix tools.

**Extend health checks with your own command.** When a connection or HTTP status cannot tell you whether a service is ready, use `type: script`. An executable or script in any language can act as the probe; exit code `0` means healthy. For example, given your application's `scripts/check_ready.py`:

```yaml
services:
  api:
    command: python3 app.py
    cwd: backend
    env-file: .env
    healthcheck:
      type: script
      command: python3 scripts/check_ready.py
      interval: 5s
      timeout: 2s
      retries: 3
  web:
    command: npm run dev
    cwd: frontend
    depends-on:
      - service: api
        condition: script-ready
```

Declaring the script probe enables its execution under the same user as devd. It inherits the service's working directory and environment: explicit `env` overrides `env-file`, which overrides the inherited environment. The environment file is read on each probe. Commands use the same argument quoting as service commands and have no implicit shell; use `sh -c '...'` explicitly for pipelines or shell expansion. Standard input is closed, and stdout/stderr are discarded; return status drives health and failure reasons appear in `status`.

Probes start immediately and run serially. Defaults are `interval: 10s`, `timeout: 2s`, and `retries: 3`. A nonzero exit, signal, execution failure, or timeout counts as a failed check; success resets the failure count. The existing restart policy applies when the failure threshold is reached. `script-ready` waits for the first successful check and requires a script probe on the prerequisite. The timeout includes environment loading, execution, and normal process-tree cleanup. On timeout, cancellation, service restart, or shutdown, devd kills the probe's Unix process group or Windows Job. Unix probes must keep subprocesses in that group. Normal completion also cleans up background descendants. `check` and `graph` validate without executing probes. Profiles replace the entire health check as usual.

**Restarts have a defined scope.** A manual restart targets the named service using the current effective configuration, including any committed reload. Success means that process has started, not that its health checks or any opted-in dependent restarts have completed. Manual restarts can bypass `never` and the automatic retry limit, but don't reset the cumulative restart count. Terminal failures, such as a startup failure with no retries remaining or an exhausted retry budget, trigger cleanup of the whole stack.

**A service can restart when its dependencies come back.** Add the following to a service that already declares `depends-on`:

```yaml
restart-on-dep-recovery: true
restart:
  policy: on-failure
  initial-delay: 1s
  max-attempts: 3
```

The default is off. An opted-in running service restarts when a direct dependency has a new process generation and all its dependencies satisfy their configured readiness conditions. Both manual and automatic dependency restarts count; health recovery within the same process does not. The service keeps running while dependencies are unavailable. Initial startup and already completed services do not trigger extra restarts. Each link in a chain must opt in to propagate recovery further.

Recovery restarts use the service's backoff and share its cumulative `max-attempts` budget with other restarts; exhaustion cleans up the stack. Enabling this with `policy: never`, or without dependencies, is a configuration error. Recoveries observed before the next startup readiness check completes are combined into one restart. A dependency that changes again after that point can trigger another; this is per-service recovery, not an atomic restart of an entire dependency graph. Stop interrupts the wait, and a manual restart of the dependent can supersede its pending backoff.

**Find out what happened.** `devd events` records structured startup, exit, dependency, health, resource and restart decisions, with run IDs, process generations and causal references.

```bash
devd events api --type restart-decision --since 10m --tail 20
devd events --json --follow
devd start --persist-events --event-max-size 10 --event-keep 3
# After shutdown:
devd events --stored --json
```

Memory retains 1024 events across the stack, with a 16 KiB serialized limit per event. `--tail` defaults to 100 (1–1000), after filtering. Repeat `--type` to match any selected type; service matching is exact and `--since` is an inclusive UTC threshold fixed when the command starts. Events remain ordered by sequence, even if the wall clock changes. Live queries work without loading YAML. Logs and events share a limit of 16 followers; Ctrl+C only exits the query.

`--json` emits a schema-versioned batch with `source`, `context`, `entries`, `gaps`, `first_available`, `cursor` and live `persistence` status (`disabled`, `recording`, `failed`). Follow emits one batch per JSON line, including empty entries when only the watermark or persistence status changes. Resume with `--cursor RUN_UUID:NEXT_SEQUENCE`; the sequence is inclusive and the returned cursor is the next inspected position, even when filters match nothing. The first batch captures history and subscribes atomically. Later follow batches leave `first_available` null. Expired history, run changes and tail truncation produce explicit gaps; an ahead-of-run cursor is an error. A lagging follower reports its gap and exits with an error; reconnect with the last cursor to recover whatever memory still retains. An old cursor against a live new run returns that new run with a run-change gap.

Event persistence requires its own `--persist-events` opt-in. Files live under the instance/profile's `events/` directory, independently of `logs/`, with the same locks, file protections and rotation rules. Defaults are 10 MiB per file plus three archives (40 MiB); `--event-max-size` accepts 1–1024 MiB and `--event-keep` accepts 1–100. Both require the opt-in. Startup storage failure starts no services. **A runtime event write failure disables disk event recording for that run, reports on stderr and through live event queries, and leaves services running.** Restart the supervisor with persistence enabled to try recording again.

`events --stored` requires the writer to have stopped, uses the same instance/profile selection and works after YAML removal. It reads retained runs oldest first, with filtering and tail across files; context, earliest available sequence and cursor describe the latest retained run. A cursor in retained history continues through later runs; a missing run returns an explicit gap and no events, so omit the cursor to inspect remaining history. An unknown offline service has no matches. Offline output is labeled `stored` and never treats historical PIDs as live processes.

Graceful shutdown drains and syncs the writer. Slow disk subscribers record gaps; incomplete runs and unfinished final records are marked, and the next persistent start repairs only the unfinished tail with a recovery marker. Corrupt complete records and unsupported schemas fail explicitly. Queries retain at most 128 gap diagnostics; `omitted_gaps` counts older discarded diagnostics. Rotation and forced termination can lose history, so these records are diagnostic evidence, not a complete audit trail. Commands, URLs and environment contents are excluded from event payloads; context still contains the instance state path, and path-condition events contain resolved requirement paths (but no file contents or resolved symlink targets).

**Explain one failure without guessing.** `devd explain <service>` is a read-only, deterministic report built from the current supervisor snapshot and structured lifecycle events. It classifies dependency blocks, startup and health failures, resource limits, restart-budget exhaustion, stops, and healthy/running states, then cites the event sequence, generation, cause and timestamp behind the conclusion. It never probes a service, starts or restarts a process, or executes a suggested next step.

```bash
devd explain api
devd explain api --json
# After a persisted run has stopped:
devd explain api --stored --json
```

Online mode connects to the running supervisor and does not reread YAML. `--stored` reads retained event files after the writer has stopped, works even when the YAML has been removed, and labels the report `source: stored`; historical PIDs are evidence only. A report with `complete: false` has explicit gaps or omitted gap diagnostics, so its conclusion is bounded by retained history. Without matching events, the report says it cannot determine the cause and points to the next read-only checks. No mode performs automatic remediation.

Explanations follow the current service generation. Without a live service, they use the latest retained generation; stored `status` stays null. A recovered CPU or memory warning is cleared independently for that metric. Explicit causes can still explain an in-progress restart, but an old failure does not override a newer running or healthy generation. Export and Agent reports use the same rules.

**Check the launch environment before starting.** `devd doctor` checks service working directories, declared `requires` paths, dotenv files, whether service and script-probe programs can be found, and explicitly declared TCP listen addresses. It does not start services, execute commands or probes, or change files and processes. `requires` checks readable files, accessible directories, and symlinks with existing file or directory targets using the same evaluator as startup. Declare ports with `listen` (for example `listen: [127.0.0.1:3000]`); healthcheck targets are not assumed to belong to the service. Listen checks briefly bind and release each address, so they report only whether it was available at that instant. Without declarations the report says `not-checked`. Use `--profile` to inspect the selected merged configuration and `--json` for a versioned machine-readable report. Failures return a nonzero exit code; a clean report cannot guarantee that a later launch will succeed.

```bash
devd doctor
devd doctor --profile staging --json
```

Doctor also shows how declared `requires` and `paths` resolve, including intermediate
directory links, relative targets and the observed failing node. JSON adds a
`resources` section (schema 1): `entries`, `captured_from/to`, `complete`, `omitted`
and `limits`. Each entry identifies the service, effective declaration (`requires`
index/type or `paths` name/scope), declared path, effective cwd, requested path and
inspection. The report's `config`, `profile` and resource `state_dir` locate the
selected configuration; this is effective-field provenance, not YAML line numbers
or a claim that a field was overridden by the profile. Requirement entries include
the existing evaluator's `condition.satisfied/failure` independently of the chain.

An inspection contains `nodes` with path/type/link target, `resolved`, `failure`
and `failed_node` (a zero-based node index, or null if unavailable). It reads metadata
and link targets, not file contents. `atomic` is always false; `change_detected`
records changes found during rechecks, and false cannot rule out races. Missing,
dangling, wrong-type, access-denied and cyclic paths are distinct. Unsupported
reparse behavior, Windows parent-segment paths, non-UTF-8 paths and exhausted limits
produce incomplete observations. `display_paths_lossy` flags substituted display
paths; they must not be used as exact identifiers. Windows supports ordinary
symbolic links; other reparse types are not assumed to resolve like them.

Coverage is capped at 128 resources, 128 observed nodes and 40 followed links per
resource, 4096 bytes per path, 64 KiB of compact node JSON per chain and 1 MiB of
compact entry JSON overall. Formatting and existing doctor checks are additional.
Omitted entries and detected changes set `resources.complete: false`; complete
coverage does not mean every resource exists. These limits bound traversal work,
not the duration of an individual OS call on a stalled filesystem. Existing
prerequisite checks still run, and `ready` keeps its original meaning: absent
ordinary path mappings do not become launch blockers. Doctor never creates even
`cleanup: true` directories. Output has a separate five-second write/flush deadline
and can be cancelled with Ctrl+C; interrupted output may be incomplete. Paths and
link targets may contain sensitive information.

**See what depends on a path.** Select one effective declaration, then follow its
potential impact through the service graph:

```bash
# Index 0 means the first requires entry after profile merging.
devd impact api --require 0 --profile staging
# DATA is a name from the selected service's paths map.
devd impact database --path DATA --scope worktrees --json
```

The default `--scope project` reads only the selected on-disk configuration.
`requires` names its declaring service as a direct user; `paths` names its provider
and direct dependency recipients. The report deduplicates downstream services and
includes a shortest service-edge path explaining each result. Declarations match
by the same requested path or the same successfully observed resolution endpoint;
this does not detect hard-link identity, directory containment or dynamic file
access. `shared` and `instance` remain declaration scopes; requirements have unknown
ownership even when they alias a scoped path. Temporary-location hints never grant
cleanup rights. Nothing is started, restarted, provisioned or deleted.

Explicit `--scope worktrees` reuses the current repository's registered instances
across its worktrees (only the current project outside Git). Unregistered instances
are outside coverage. A live identity only establishes which instance answered:
peer YAML is labeled `disk-candidate`, and `runtime_equivalence: unknown` means edits
or reloads may make it differ from memory. Unreachable, replaced or incompatible
instances have no inferred impact. Missing/invalid peer YAML, corrupt registries
and omitted peers are visible. `registry_complete` describes index coverage;
`impact_complete` stays false when any peer exists because its runtime configuration
is unverified. Default queries likewise make no claim about a running configuration.

JSON schema 1 records config/profile/state, the derived `instance_id` (not proof of
a live run), capture interval, selected resolution chain, `current.matches`,
`direct_consumers`, `downstream`, and optional `discovery.peers`. Each match gives its
basis and scope; contradictory observations set `conflicts_with_selection` and
make the view incomplete. `complete` describes bounded declared coverage, not
resource health or an atomic snapshot (`atomic` is always false). Potential impact
is not evidence of application failure causation.

Each configuration is limited to 256 services, 1024 dependency edges, 256 inspected
declarations and 64 matches. Graph overflow or a selected declaration outside the
scan limit is an error; other omissions and uncertain aliases are counted. Wider
queries analyze at most eight peers, each with a regular YAML file of at most
1 MiB, within discovery's existing 64-worktree/256-record limits. The complete
rendered report is limited to 2 MiB; output has a five-second write/flush deadline
and Ctrl+C cancellation. Filesystem calls have the same non-atomic and stalled-I/O
limitations as doctor. Paths themselves may contain sensitive information.

**Logs stay in memory by default.** devd retains the latest 1000 entries across the stack, with a 16 KiB limit per line. Without persistence they cannot be queried after shutdown. `logs --follow` starts with the requested tail and then streams new entries until Ctrl+C or supervisor shutdown; a lagging follower exits with an error. Up to 16 followers can connect at once, leaving room for control commands. Slow foreground output can lose live entries, with a warning; a broken foreground output pipe triggers service cleanup.

**Keep logs after shutdown when you need them.** Start with `devd start --persist-logs`, then use `devd logs --stored` after the supervisor exits. For example:

```bash
devd start --persist-logs --log-max-size 10 --log-keep 3
# After stopping the foreground supervisor:
devd logs api --stored --level error --since 1h --tail 50
```

The instance's state directory contains `logs/current.jsonl` and `logs/archive-1.jsonl` (newest archive), up to the configured count. Each record preserves UTC time, service, process generation, level, message and truncation status. Defaults are 10 MiB per file and three archives, at most 40 MiB of log data; rotation happens before a complete record would exceed the limit. `--log-max-size` accepts 1–1024 MiB and `--log-keep` accepts 1–100 archives; both require `--persist-logs`. Lower retention removes surplus managed archives on the next persistent start. Lowering the size limit does not rewrite existing archives; they age out through normal rotation. New directories/files use permissions 0700/0600 on Unix and inherit directory ACLs on Windows; keep Windows projects/state directories in your user account. Logs can contain application secrets; retention also applies across supervisor runs, which append to the current file.

Use the same `--config`, `--profile` and `--state-dir` as startup. Disk logs are isolated by instance/profile; `--stored` works even if the YAML has been deleted, requires the persistent writer to have stopped, and cannot be combined with `--follow`. A missing directory is an error; an unknown service returns no matching history. Normal `logs` continues to query only the current run's memory. Stored queries apply the same filters and tail limit across retained files.

Disk writes run independently of service capture using a bounded subscription. A slow disk can lose complete entries; the file records a `devd` WARN with the skipped count. Storage failures stop the stack and return an error. Graceful shutdown drains and syncs accepted entries; forced termination or power loss can lose unsynced data. An interrupted final JSONL record is ignored by offline queries and removed with a warning on the next persistent start; corrupt complete records make queries fail. This is bounded development logging, not an audit log.

Filter history or a live stream with `--level`, `--since`, and `--grep`, alone or together. For example, `devd logs api --level error --since 5m --grep database --tail 50 --follow` first shows up to 50 matching retained entries, then matching new ones. Level matches exactly; `--grep` is literal and case-sensitive against the raw message. `--since` accepts `ms`, `s`, `m`, or `h` (for example `500ms` or `2h`) and fixes its cutoff when the command starts. Filtering cannot recover entries evicted from memory.

**Diagnostics describe the right layer.** `status` returns an error when the supervisor is offline; leftover state files are for diagnosis. If you break the configuration file while devd is running, you can still use `stop`, `status`, `logs`, `restart`, and `top`. `check` validates configuration structure and relationships; `doctor` checks launch prerequisites without executing them; health probes run only under the supervisor. Failed commands return a nonzero exit code.

`devd top` connects to the same running instance as `status` and `logs`. The header identifies the instance, supervisor run, profile and configuration path; the service table shows state, PID, restart count and CPU/RSS. **Tab switches between Logs and Events.** Up/Down (or j/k) selects a service for `r` to restart; both panes show the whole stack. Page Up/Down scrolls the active pane, End follows the latest entries, Left/Right scrolls long records and header fields horizontally, and Home resets horizontal scrolling. `s` asks for confirmation before stopping the entire stack; Enter or `s` confirms, Esc or `n` cancels. `q` and Ctrl+C only close the view. It requires an interactive terminal of at least 60×16 and does not start a supervisor.

The event timeline polls live memory about once per second and retains at most 1,000 rows, including gap markers. Records follow run-local `#sequence` order; `g` is the generation's initial event sequence, not the PID or restart count. `cause=#…` is a recorded direct causal link, `cause=-` means no link was recorded, and `(unavailable)` means the referenced event is outside the current view's retained history. Adjacent records alone do not establish a cause. Supervisor retention/tail gaps, local row eviction and disk recording failures remain visible in the summary; the latest gap stays summarized after its row is evicted. This view neither enables event persistence nor reads stored history. State, events and application logs are separate observations, not an atomic snapshot. A disconnect or run change closes the view with an error; controls and log subscriptions are pinned to the original run so an old view cannot act on a replacement supervisor.

**Resource samples describe the main process.** The supervisor samples about once per second; child processes launched by a shell or package manager are not added to the totals. CPU uses one fully occupied core as 100%, so multithreaded processes can exceed 100%. Memory is RSS, shown in MiB. Unavailable values display `-`; CPU needs two successful samples after startup or restart. JSON includes optional `resources` with `cpu_percent` (nullable during warmup), `memory_bytes`, and `sampled_at`. Samples are cleared on exit.

Optional `limits` raise a warning when a sampled value exceeds its threshold and an info entry when it returns within range:

```yaml
services:
  api:
    command: ./run-api
    limits: {cpu: '150%', memory: 512MiB}
```

CPU must be a positive integer percentage. Memory accepts a positive integer followed by `B`, `KB`, `MB`, `GB`, `KiB`, `MiB`, or `GiB`; decimal and binary units differ. Each threshold crossing is logged once, including after a process restart. Missing samples and CPU warmup do not clear an active warning. Descendants are not included, and these thresholds do not enforce CPU or memory caps.

**Resource restarts require explicit permission per service.** Omitting `limits.on-exceed` defaults to `warn` and leaves the process running. To enable automatic recovery:

```yaml
services:
  api:
    command: ./run-api
    limits:
      memory: 512MiB
      on-exceed: restart
    restart:
      policy: on-failure
      backoff: exponential
      initial-delay: 1s
      max-attempts: 3
```

The same metric must exceed its threshold in 3 consecutive valid samples (roughly one sample per second). A value within range or a missing value resets that metric's count; CPU warmup resets only CPU's count. The decision is retained until that process generation ends. devd stops the process group, drains logs, applies the existing backoff, and rechecks dependencies before starting again. Resource, crash, health, and dependency recovery restarts share the cumulative restart budget; exhaustion fails the service and shuts down the stack. Stop interrupts backoff, and manual restart can supersede it. The reason appears in logs and failure diagnostics. `on-exceed: restart` conflicts with `restart.policy: never` and is rejected before startup. Profiles replace the entire `limits` block, so a replacement that omits `on-exceed` returns to `warn`; `limits: null` disables thresholds. Configuration changes take effect on the next supervisor start or through explicit selective reload, without root privileges or an interactive permission prompt.

The current scope is local process management. `init` creates a starter file; project scanning and interactive templates are not implemented. File prerequisites can be monitored as described above; configuration file watching and automatic reload are not implemented. Configuration changes use explicit selective reload.

Configuration rejects unknown fields and invalid `limits` settings. YAML values are literal; `${VAR}` expansion is not implemented. With `backoff: exponential`, retries start at `initial-delay`, double with the cumulative restart count, and cap at `max-delay` (default 60s, must be at least `initial-delay`). Healthy probes do not reset that count. Fixed backoff ignores `max-delay`; either wait can be interrupted by stopping the service.

## Development and validation

Try the [API + web + worker example](examples/local-stack/README.md) for a working
stack and a repeatable failure-recovery smoke test.

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
```

End-to-end tests launch real child processes and verify dependency readiness, failure recovery, logs, status, and process cleanup after shutdown:

```bash
cargo test --locked --test integration
```

[Tests and manual validation](tests/README.md) · [Release checklist](RELEASING.md) · [Changelog](CHANGELOG.md) · [MIT license](LICENSE) · [Architecture (中文)](ARCHITECTURE.md)
