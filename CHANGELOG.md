# Changelog

## Unreleased

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

Prerelease for local development on Linux, macOS, and Windows. Native CI and
release-binary recovery tests passed on all three platforms for source revision
`a044e60909ee1dec9ec40c49fb6e6ae4ee3de6c2`; archives and SHA-256 checksums were verified.

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
