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

devd is a local development service manager written in Rust. **v0.2.0-alpha.1 is a prerelease for Linux and macOS**, built on the v0.1 MVP.

Describe your services and their dependencies in `devd.yml`, then run `devd start` in the foreground. Use another terminal to check status, read logs, or restart a service.

- **Start in dependency order.** Independent services start concurrently. Dependencies can wait for a process to start or for a TCP / HTTP / Unix socket health check to pass.
- **Watch service health.** TCP and Unix socket connection checks and HTTP 2xx probes track consecutive failures and report what went wrong. Socket paths resolve relative to the service working directory; a stale socket file is not healthy.
- **Handle unexpected exits.** Choose `always`, `on-failure`, or `never`, with fixed or exponential retry delays and a limit on automatic restarts.
- **Bring the logs together.** Collect stdout / stderr with timestamps, service names, and colors. Query recent output for a specific service.
- **Clean up on the way out.** Ctrl+C, SIGTERM, or `devd stop` shuts services down in reverse dependency order and cleans up descendants in their process groups.

It's for local projects with an API, a frontend, workers, or other processes that need to run together. Your services keep their existing startup commands; devd coordinates them.

## Get it running

You'll need the Rust toolchain. Install from the repository root:

```bash
cargo install --path . --locked
```

Run `devd init` in a project directory to create a runnable one-service starter configuration. Use `--service` and `--command` to set its service name and command; `--config` chooses the output path. The command refuses to replace an existing file. For a two-service example, use this configuration:

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

`cwd` is relative to the configuration file's directory. `env-file` is relative to the service's `cwd`, and explicit `env` values override entries from that file. Commands support quoted arguments. For pipes, redirection, or shell expansion, use `sh -c '...'` explicitly.

## Commands

| Command | Purpose |
| --- | --- |
| `devd start` | Start the stack in the foreground and stream logs |
| `devd stop` | Request ordered shutdown; the foreground process exits after cleanup |
| `devd restart <service>` | Restart one service using the configuration loaded at startup, rechecking dependencies |
| `devd status [--json]` | Show live state, PIDs, restart counts, and diagnostics |
| `devd logs [service] [--tail N] [--follow]` | Query buffered logs (default 100, N from 1–1000); optionally stream new entries |
| `devd check` | Validate configuration, command quoting, dependencies, and supported settings |
| `devd graph` | Show dependency edges and parallel startup layers |
| `devd init [--service NAME] [--command CMD]` | Create a checked starter configuration without overwriting an existing file |

All commands accept `-c / --config <PATH>`, `--state-dir <PATH>`, and `--color auto|always|never`. Options work before or after the subcommand:

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

## A few things to know

**It runs in the foreground.** devd manages service tasks with Tokio. Commands in other terminals reach the running instance through a Unix socket. Runtime files default to `.devd/<config-filename>/` inside the configuration directory; add `.devd/` to your project's `.gitignore`. If the socket path is too long, choose a shorter `--state-dir`. Use the same configuration and state directory when addressing the same instance.

**Restarts have a defined scope.** A manual restart affects only the named service and uses the configuration loaded at startup. Success means the new process has started; it may still be waiting to pass its health check. Manual restarts can bypass `never` and the automatic retry limit, but don't reset the cumulative restart count. Terminal failures, such as a startup failure with no retries remaining or an exhausted retry budget, trigger cleanup of the whole stack.

**Logs live in memory.** By default, devd retains the latest 1000 entries across the stack, with a 16 KiB limit per line. They can't be queried through `logs` after shutdown. `logs --follow` starts with the requested tail and then streams new entries until Ctrl+C or supervisor shutdown; a lagging follower exits with an error. Up to 16 followers can connect at once, leaving room for control commands. Slow foreground output can lose live entries, with a warning; a broken foreground output pipe triggers service cleanup.

**Diagnostics describe the running instance.** `status` returns an error when the supervisor is offline; leftover state files are for diagnosis. If you break the configuration file while devd is running, you can still use `stop`, `status`, `logs`, and `restart`. `check` performs static validation; executable availability, environment files, and probe endpoints are checked at runtime. Failed commands return a nonzero exit code.

The current scope is local process management. `init` creates a starter file; project scanning and interactive templates are planned. Resource monitoring, hot reload, disk logs, and a TUI are also planned for later versions.

Configuration rejects unknown fields and unsupported `limits` settings. YAML values are literal; `${VAR}` expansion is not implemented. With `backoff: exponential`, retries start at `initial-delay`, double with the cumulative restart count, and cap at `max-delay` (default 60s, must be at least `initial-delay`). Healthy probes do not reset that count. Fixed backoff ignores `max-delay`; either wait can be interrupted by stopping the service.

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
