# Changelog

## Unreleased

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
