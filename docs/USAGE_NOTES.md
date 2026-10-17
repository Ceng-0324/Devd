# Usage notes

**English** · [简体中文](USAGE_NOTES.zh-CN.md) · [Back to README](../README.md)

Getting the stack running is the easy part. Here's what to expect afterward: what a restart touches, how long evidence lasts, and which actions you have to opt into. For command examples, see the [command guide](COMMANDS.md).

**It runs in the foreground.** devd manages service tasks with Tokio. Commands in other terminals use a Unix socket, or a local Windows named pipe restricted to the current user. Runtime files default to `.devd/<config-filename>/` inside the configuration directory; add `.devd/` to your project's `.gitignore`. If a Unix socket path is too long, choose a shorter `--state-dir`. Use the same configuration and state directory when addressing the same instance.

**Windows uses native process ownership.** Windows 10/11 and Windows Server 2016+ use one Job Object per service generation, covering descendants before the service starts executing. Stop sends Ctrl+Break to that service's console process group, then terminates the job after the grace period; console-less services are terminated directly. Programs must handle Ctrl+Break to shut down gracefully. Closing or killing devd also closes its jobs. TCP, HTTP, script probes, `top`, profiles, snapshots, resource controls, and stored logs work through the same commands. Unix socket probes are rejected by `check` and `start` on Windows; use TCP or a script instead. TUI requires an interactive console.

Commands retain shell-style quoting on Windows. Prefer forward slashes in YAML paths, and quote paths containing spaces, for example `command: "'C:/Program Files/Python/python.exe' app.py"`. Invoke `powershell.exe -NoProfile -File script.ps1` or `cmd.exe /C ...` explicitly when needed. `devd init`, followed by `devd start` in one terminal and `devd status`, `devd top`, or `devd stop` in another, is a Windows quick start without Unix tools.

**Tasks finish; services stay up.** `kind: task` lets a dependent wait for exit code 0 with `completed-successfully`. Tasks do not retry unless you explicitly select `on-failure`; restarting a consumer reuses the current successful result, while `restart TASK` runs the task again. Repeated migrations can repeat side effects, so choose retry policies deliberately. See [one-shot tasks](COMMANDS.md#one-shot-tasks) for timeout, reload, and shutdown behavior.

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

Online mode connects to the running supervisor and does not reread YAML. `--stored` reads retained event files after the writer has stopped, works even when the YAML has been removed, and labels the report `source: stored`; historical PIDs are evidence only. A report with `complete: false` has explicit gaps, omitted gap diagnostics, or truncated event content referenced by its explanation, so its conclusion is bounded by retained history. Without matching events, the report says it cannot determine the cause and points to the next read-only checks. No mode performs automatic remediation.

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

The current scope is local process management. `init` creates a starter file and can scan a bounded Node.js `package.json` to propose a reviewed task-to-consumer configuration; it never installs dependencies, reads `.env`, executes scripts, or overwrites an existing file. File prerequisites can be monitored as described in the [README](../README.md); configuration file watching and automatic reload are not implemented. Configuration changes use explicit selective reload.

Configuration rejects unknown fields and invalid `limits` settings. YAML values are literal; `${VAR}` expansion is not implemented. With `backoff: exponential`, retries start at `initial-delay`, double with the cumulative restart count, and cap at `max-delay` (default 60s, must be at least `initial-delay`). Healthy probes do not reset that count. Fixed backoff ignores `max-delay`; either wait can be interrupted by stopping the service.

