# Changelog

## Unreleased

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
