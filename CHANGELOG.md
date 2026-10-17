# Changelog

Release assets, source revisions and validation evidence are on
[GitHub Releases](https://github.com/Ceng-0324/Devd/releases).

## v1.1.0 — Unreleased

This release will combine the verified v1.0.1 patch fixes with the project
scanning, reviewable init, and explicit long-task dependency readiness work
tracked in the v1.1.0 milestone. No standalone v1.0.1 release is planned.

- Render task readiness requirements as `completed-successfully` in `wait`
  text output, preserving service labels, JSON and exit codes.
- Compare event gaps by their published reasons, payload fingerprints and
  counts, and report changes in omitted gap diagnostics separately.
- Mark comparison evidence incomplete when retained timeline events are
  truncated, even when capture is stable and no gaps were recorded.
- Compare bounded retained lifecycle fields, including restart counts and event
  generations, while treating absent fields as unknown.
- Mark explanations incomplete when referenced event content is truncated.
- Reject unsafe state lock paths before open, including Unix FIFOs, so a
  pre-positioned special file cannot block supervisor startup.

- Reject symlinked state temporary files before publication so state writes do
  not overwrite external targets.
- Reject non-regular `env-file` paths before reading, preventing FIFOs from
  blocking supervisor startup and shutdown.
- Compare retained historical explanations even when both reports lack state
  snapshots.

## v1.0.0 — 2026-10-17

- Establish SemVer 2.0.0 release rules for public compatibility, prerelease
  numbering, synchronized version metadata and immutable published versions.

- Add a shared bilingual field-testing guide and feedback template, keeping
  engineering exercises, maintainer trials and independent feedback distinct.
  Correct task documentation to describe the fixed 30-second dependency deadline;
  `wait --timeout` does not extend it and no CLI override is currently available.

- Define bilingual compatibility, upgrade and exit-code contracts. Add frozen
  v0.9 export regressions and an integrated task/evidence/Agent workflow to native
  CI and extracted release-binary checks, including official MCP clients. Preserve
  failed/unknown fixed-version comparison observations and distinguish engineering
  acceptance from still-unverified independent project feedback.

- Add independently authorized Agent/MCP session restart budgets and control request deduplication. Both remain disabled by default; budgets are scoped per named service and session, operation keys are required only when deduplication is enabled, and unknown results are never replayed.

- Add offline `compare LEFT RIGHT [--json]` for two explicitly selected, transferred reports. Reuse the untrusted-report validator, distinguish comparable/different/unknown configuration, scope, state, relationship, resource and evidence-gap fields, hash sensitive resource identities, and keep log attachments separate without inferring common causes or recovery effects.

- Add read-only `coverage [--online] [--json]` explanations of declared resources,
  configured monitoring and independent event/log/incident recording choices.
  Keep static unknowns separate from live recorder failures, bounded retention
  and observed evidence gaps; never enable monitoring or infer implicit dependencies.

- Add explicit one-shot tasks with successful-completion dependency gates,
  bounded execution, opt-in failure retries, generation-scoped manual reruns,
  task-aware wait/reload, and retained completion/timeout diagnostics.
  Preserve existing service defaults and replace the comparison's marker-wait
  wrapper with native task ordering.

## v0.9.0-alpha.1 — 2026-10-14

- Move detailed command, usage and permission notes out of both READMEs into
  linked bilingual guides under `docs/`, included in release archives and the public Cargo package.

- Add a complete native drift/recovery/evidence-handoff scenario to CI and
  extracted release-binary checks. Include generated third-party license texts
  in release archives, check dependency licenses and registries, and publish
  contribution/security guidance and a fixed-version synthetic comparison.
  Keep engineering acceptance separate from independent user-value evidence.

- Add explicit stopped-instance `recover --recipe --dry-run/--apply --plan`
  workflows for registered disposable resources, bounded commands and file
  postconditions. Hold the instance lease, reject stale/expired plans and unsafe
  replacements, retain per-step receipts and refuse automatic replay after
  interruption. Keep commands, recovery permission and service readiness separate;
  exercise failure, timeout, cancellation, crash and output stalls natively.

- Add explicit `baseline capture/check` for selected resource chains and metadata.
  Keep accepted baselines immutable, compare through current disk declarations
  and local provenance, distinguish observed changes from unknown evidence, and
  show current declared consumers/downstream impact. Never read resource bodies,
  execute programs or authorize recovery; add native drift and privacy coverage.

- Add `mcp --stdio` with run-bound diagnostic and individually granted control
  tools, plus isolated `--report FILE` diagnosis. Reuse the JSON-lines Agent's
  authorization and control use cases; add retained incident reads to both
  adapters. Bound framing, concurrency, cancellation and output stalls; preserve
  fresh-plan and old-run guards. Verify interoperability using the pinned official
  MCP Inspector in native CI.

- Add `diagnose REPORT.json [--json]` for transferred live, retained-run and
  incident exports. Recompute historical explanations, show sequence timelines
  and retained declared impact without the original project or endpoint. Bound
  untrusted input/output, escape terminal fields, preserve partial evidence and
  keep log metadata separate. Extend native CI exercises to copy reports into
  an empty recipient directory with original state/resources unavailable.

- Retain bounded capture-time declared dependency/resource relationships in
  schema-2 incident artifacts. `incidents show` projects the preserved DAG as a
  readable tree with shared-node references; JSON keeps the typed graph, match
  basis, explicit limits and coverage gaps. Historical readers never reload YAML
  or inspect repaired paths, while schema-1 artifacts remain explicitly unknown.

## v0.8.0-alpha.1 — 2026-10-12

- Validate archived native binaries with an end-to-end dangling-resource incident
  exercise, including declared downstream impact, unchanged service PIDs,
  privacy, offline export after configuration deletion/target repair, no-clobber
  publication and incomplete evidence after abrupt supervisor death. Run the
  same bounded exercise in Linux/macOS/Windows CI.

- Add `incidents list/show` and `export --incident UUID` for recorded evidence,
  with explicit `--stored` access after shutdown. Pin online queries to the
  current instance/run; preserve historical generations, partial files, schema
  diagnostics and unknown impact coverage without inspecting today's resources.
  Reuse safe locked reads, bounded cancellation and no-clobber publication;
  optional logs remain separate attachments with explicit attribution.

- Add independent opt-in incident capture with `start --record-incidents`.
  Correlate service/generation triggers in bounded five-second windows, preserve
  allowlisted context and fresh path observations separately, and record early
  endings and coverage loss. A single worker manages up to four pending captures
  and twenty non-overwriting 2 MiB artifacts with independent locking and safe
  retention. Runtime recording failure disables the recorder while services
  continue; killed or cancelled captures never masquerade as complete evidence.

- Keep event-follow signal subscriptions alive across receive/output transitions
  so an interrupt cannot fall into a listener registration gap.
- Add `export --stored --run UUID|latest` for one retained run after shutdown,
  including explicit missing/stale state, writer markers and history gaps.
  Offline schema 2 shares safe fields and explanations with live schema 1;
  optional legacy disk logs are a separate attachment with unknown run attribution.
  Preserve reader locks and file checks, bound scans/output, and prevent late
  publication after accepted cancellation. Existing files are never overwritten.

- Add read-only `impact` queries for selected `requires` entries or named `paths`,
  with direct declared users, deduplicated downstream services and explanatory
  dependency paths. Preserve ownership declarations and conservative alias
  observations. Optional worktree discovery labels peer disk candidates and
  unknown runtime coverage explicitly; traversal, peers and output are bounded.

- Extend `doctor` with read-only resolution chains for declared requirements and
  runtime path mappings, including intermediate symlinks, observed failure nodes,
  effective configuration provenance and structured prerequisite results.
  Bound traversal and report size, expose omissions and detected changes, and
  preserve existing readiness, restart and cleanup permissions. Reports are
  non-atomic observations; Windows parent-segment paths and unsupported reparse
  behavior remain explicit unknowns. Doctor output is cancellable with a separate
  five-second write deadline.

## v0.7.0-alpha.1 — 2026-10-11

Prerelease for local development on Linux, macOS and Windows. Adds worktree-aware
instance discovery, readiness waits, diagnostic export, explicit bindings,
an event timeline, owned-directory cleanup and an opt-in Agent control interface.
Cleanup requires explicit ownership and a reviewed plan after normal shutdown;
Agent controls are closed by default. No MCP, automatic port allocation, file
repair, automatic reload or transactional rollback is included.

- Keep final readiness report output cancellable under pipe backpressure, with
  a separate five-second write/flush deadline and explicit incomplete-output errors.

- Base explanations on the current service generation and the latest observation
  of each resource metric. Preserve explicit restart causes and historical events
  without presenting an earlier failure or recovered warning as a current fault.

- Resolve cleanup shared paths against the selected configuration and service
  working directory, including Agent cleanup launched from another directory.
  Reject shared overlaps consistently before preview or deletion.

- Add `agent --stdio`, a schema-1 JSON-lines interface bound to one live instance
  and run. Default to diagnostic reads; allow the launcher to grant restart,
  stop, reload and cleanup separately. Check scope at execution, retain reviewed
  plans and ownership checks, reject unknown request fields, bound I/O, and reuse
  readiness and lifecycle operations. This adapter does not change ordinary CLI
  permissions or introduce MCP, remote access or multi-user authorization.

- Add opt-in instance directory ownership with `paths.<NAME>.cleanup: true`.
  Create and register new disposable directories; never adopt existing data.
  Preview with `clean --dry-run` and apply a current plan after a successful
  shutdown with `clean --apply --plan ID`. Preserve shared/unregistered data,
  diagnostic history and snapshots; reject stale plans, links and uncertain
  shutdowns, checkpoint partial cleanup, and make completed retries harmless.
  Changes to cleanup declarations require a full stop/start.
  Explicitly release state/history file locks at the end of their final lease,
  avoiding transient lock retention during concurrent child-process creation.

- Add a bounded lifecycle event timeline to `top`, with Tab switching between
  logs and events, instance/run identity, event generations, recorded cause
  references, explicit history gaps, and disk recording status. Pin controls
  and log subscriptions to the original run; retain quit and confirmed-stop
  semantics. Page by the visible height and scroll long records horizontally.

- Add explicit service `ports` and scoped `paths` mappings, injecting the same
  environment into owners, script probes, and direct dependents. Isolate instance
  paths under the selected state directory, reject mapping conflicts, include
  mapped ports in doctor, and preserve profile replacement and reload semantics.
  No automatic port allocation; directory creation and cleanup require the
  separate explicit opt-in above.

- Add `export --output FILE [--include-logs]` to save a bounded live diagnostic
  report with instance/run identity, service generations, event gaps and
  explanations. Exclude application logs and free-text failures by default;
  report sampling stability and never overwrite an existing file.
- Add read-only `wait [service…] --timeout 30s [--json]` with live health/PID
  readiness, per-run and generation evidence, bounded concurrent waits, and
  explicit timeout, cancellation, stop, reload, and disconnect outcomes.
  Follow restarts within one supervisor; never reconnect across runs or infer
  readiness from an old state file. Cancelling a wait leaves services running.
- Add read-only `identity` and project/worktree-scoped `instances` commands with
  stable instance identity, per-run identity, startup Git context, profile and
  custom-state-directory registration, bounded endpoint verification, and
  explicit stale/unreachable records and index warnings.
- Make the resource alarm regression test use distinct sample timestamps so
  fast execution cannot accidentally suppress its recovery assertion.
- Exercise simultaneous worktrees through live discovery, TCP readiness,
  mapped environments, Agent restart/reload/export, stopped cleanup, and run
  replacement. Stabilize binding-output and cooperative Unix shutdown fixtures.

## v0.6.0-alpha.1 — 2026-10-08

Prerelease for local development on Linux, macOS, and Windows. No automatic file
repair, file-triggered restart, automatic reload, or transactional rollback is included.

- Apply a reviewed configuration plan with `reload --apply --plan ID`. Reject
  stale plans before process changes, stop affected old services in reverse
  dependency layers, and start/readiness-check the new layers. Unrelated services
  retain their processes on success. Serialize reload against manual restart,
  support stop preemption, and report partial progress through JSON and events.
  Application failures stop the whole stack without automatic rollback; file
  watching remains deferred.

- Preview configuration changes against a live supervisor with `reload --dry-run`,
  an optional candidate file, and text or versioned JSON output. Validate the
  candidate and selected profile, report direct and transitive dependency impact,
  and show conservative stop/start layers with instance/configuration identities.
  Preview does not change processes or runtime state.
- Declare file, directory, and symlink prerequisites with service `requires`;
  check them before spawn and through `doctor`. Optionally observe changes with
  `monitor-requires`, deduplicated events, and evidence in `explain`, without
  authorizing restarts or changing user files.
- Flush followed event and application-log output before waiting for more data,
  including when stdout is redirected to a file.
- Expand native filesystem and reload failure coverage, and verify release
  archive checksums, exact public contents, version, and source revision before
  running recovery smoke tests from the packaged binary.

## v0.5.0-alpha.1 — 2026-10-07

Prerelease for local development on Linux, macOS, and Windows.

- Add read-only `devd doctor` environment reports in text or versioned JSON,
  checking service directories, dotenv files, executable discovery, script
  probe executables, and explicitly declared TCP listen addresses without
  running commands or changing processes. Profiles select the effective
  configuration; bind checks are momentary observations.
- Add optional service `listen` declarations for owned TCP addresses, separate
  from healthcheck targets, and reject ephemeral port zero in configuration.
- Add read-only `explain` reports for deterministic, event-backed service failure
  diagnosis in live and stored history, with cited evidence and explicit gaps.
- Add `events` text/JSON queries with service/type/time filters, filtered tails,
  run-scoped cursors, explicit retention gaps and atomic history-to-live follow.
- Add independent opt-in event persistence (`start --persist-events`,
  `--event-max-size`, `--event-keep`) and offline `events --stored` queries.
  Share safe rotating JSONL file handling with logs; report incomplete runs,
  subscriber losses, repaired tails, and corrupt or unsupported records.
- Keep services running after runtime event-storage failure, disable disk event
  recording for that run, and expose failure through live queries and bounded
  stderr output. Application-log storage retains its existing failure policy.
- Add bounded structured lifecycle events with per-run identity, service
  generations, causal restart evidence, safe failure details, and live
  subscribers. Legacy runtime snapshots remain readable.

## v0.4.0-alpha.1 — 2026-10-06

Prerelease for local development on Linux, macOS, and Windows.

- Add `devd top` for live service status, resource metrics, bounded logs, asynchronous single-service restart, and confirmed whole-stack stop; quitting the view leaves services running.
- Add opt-in JSONL persistence with `start --persist-logs`, per-instance/profile storage, size-based rotation, and configurable archive retention (`--log-max-size` in MiB and `--log-keep`).
- Query offline disk history with `logs --stored`, reusing service, level, time, literal text, and filtered-tail selection without requiring the YAML file.
- Drain and sync disk logs on graceful shutdown; report bounded-buffer losses, recover interrupted final records, reject linked/non-regular managed files, and stop services on storage failures.
- Add CPU and RSS threshold warnings with recovery diagnostics. Resource restarts remain off unless a service explicitly sets `limits.on-exceed: restart`; three consecutive valid samples of the same metric must exceed its threshold. Restarts share the existing backoff and cumulative budget.
- Add external-command script health checks and `script-ready` dependencies, preserving service working directory/environment, serial probe timing, bounded execution, and descendant cleanup on completion or cancellation.
- Add Windows supervision with Job Objects, assignment before child execution, Ctrl+Break/grace-period shutdown, and tree cleanup when the supervisor exits. Use local named pipes with a current-user DACL for control requests.
- Enable cross-platform state/storage locks, safe file access, native PowerShell starter configuration, and cancellable Windows console output. Reject Unix socket probes on Windows before starting services.
- Extend CI and three-service recovery smoke tests to Windows; build Linux/macOS tarballs and a Windows x86_64 MSVC zip with source revision, version, and SHA-256 checksums.

## v0.3.0-alpha.1 — 2026-10-06

Prerelease for local development on Linux and macOS. This release includes the
previously unreleased v0.2 development work as well as the v0.3 modules below.

- Complete the v0.3 documentation with a runnable profile example covering dependency diagrams, filtered logs, configuration snapshots, and isolated instances.
- Filter `logs` history and live follow by exact level, relative start time, and literal message text; apply `--tail` after filtering.
- Export validated dependency graphs as DOT or Mermaid with readiness labels and isolated-service nodes; keep the existing text view as the default.
- Add byte-preserving configuration snapshots with `snapshot save` and `snapshot restore --output`; include all profiles, create new files atomically without overwriting, and keep runtime state and processes untouched.
- Ignore macOS `.DS_Store` files in the repository.
- Add single-file configuration `profiles` and global `--profile` selection for startup, validation, dependency graphs, and live control commands.
- Merge environment and restart settings by key, replace dependency lists and health checks, support clearing optional fields, and validate effective dependencies before startup.
- Isolate profile sockets and state even with an explicit state directory and on case-insensitive filesystems; retain live control when the configuration is corrupted or deleted.
- Add opt-in `restart-on-dep-recovery`: restart a running service when a direct dependency is replaced and dependency readiness is restored, using existing backoff and restart budgets. Same-process health fluctuations do not trigger recovery restarts.
- Keep health probes in flight across dependency and resource snapshot updates; preserve stop/manual restart control during recovery waits.
- Add per-service leader CPU and RSS samples to `status`, JSON responses, and runtime snapshots, sampled approximately every second without blocking lifecycle control.
- Reset CPU baselines on restart, discard late samples from previous generations, and clear metrics on exit or unavailable observations.
- Require Rust 1.95 or newer for the resource-monitoring dependency.

## v0.2.0-alpha.1 — 2026-10-05

First public prerelease for local development on Linux and macOS.

- Add `logs --follow` with an atomic buffered-tail to live-stream handoff.
- Add `devd init` to create a validated starter config without overwriting files.
- Add Unix socket health checks and `socket-ready` dependencies, resolving relative probe paths against the service working directory.
- Add capped exponential restart delays with overflow protection and cancellable waits.

### Included MVP capabilities

- Manage local services from a YAML configuration with dependency-ordered startup and shutdown.
- Check TCP and HTTP readiness, restart failed or unhealthy processes with a fixed delay, and bound retry attempts.
- Collect bounded in-memory logs and expose `start`, `stop`, `restart`, `status`, `logs`, `check`, and `graph` commands.
- Preserve project state and clean child process groups during normal shutdown on macOS and Linux.
- Exercise an API, web frontend, and worker stack with a repeatable failure-recovery smoke test.

### Reliability

- Reject unknown configuration fields and unsupported resource limits.
- Preserve multiline commands when generating starter configurations.
- Reclaim disconnected log followers, reserve control connection capacity, and time out incomplete stream frames.

The v0.1 MVP was an internal milestone; no separate v0.1.0 release was published.
