# Changelog

## Unreleased

### v0.2.0-alpha.1 development snapshot

- Add `logs --follow` with an atomic buffered-tail to live-stream handoff.
- Add `devd init` to create a validated starter config without overwriting files.
- Add Unix socket health checks and `socket-ready` dependencies, resolving relative probe paths against the service working directory.

### v0.1.0 release candidate

- Manage local services from a YAML configuration with dependency-ordered startup and shutdown.
- Check TCP and HTTP readiness, restart failed or unhealthy processes with a fixed delay, and bound retry attempts.
- Collect bounded in-memory logs and expose `start`, `stop`, `restart`, `status`, `logs`, `check`, and `graph` commands.
- Preserve project state and clean child process groups during normal shutdown on macOS and Linux.
- Exercise an API, web frontend, and worker stack with a repeatable failure-recovery smoke test.

This is a pre-release description, not a claim that a v0.1.0 tag or binaries have been published.
