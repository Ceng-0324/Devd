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

The current development checkout also adds CPU and memory samples to `status`, opt-in restarts after dependency recovery, named configuration profiles, and configuration snapshots; these are not included in the alpha.1 release binaries.

Describe your services and their dependencies in `devd.yml`, then run `devd start` in the foreground. Use another terminal to check status, read logs, or restart a service.

- **Start in dependency order.** Independent services start concurrently. Dependencies can wait for a process to start or for a TCP / HTTP / Unix socket health check to pass.
- **Watch service health.** TCP and Unix socket connection checks and HTTP 2xx probes track consecutive failures and report what went wrong. Socket paths resolve relative to the service working directory; a stale socket file is not healthy.
- **Handle unexpected exits.** Choose `always`, `on-failure`, or `never`, with fixed or exponential retry delays and a limit on automatic restarts.
- **Bring the logs together.** Collect stdout / stderr with timestamps, service names, and colors. Query recent output for a specific service.
- **See what each service uses.** `status` shows CPU usage and resident memory for each service's main process.
- **Clean up on the way out.** Ctrl+C, SIGTERM, or `devd stop` shuts services down in reverse dependency order and cleans up descendants in their process groups.

It's for local projects with an API, a frontend, workers, or other processes that need to run together. Your services keep their existing startup commands; devd coordinates them.

## Get it running

You'll need Rust 1.95 or newer. Install from the repository root:

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

Services merge by name. `env` and `restart` merge by key; other fields, including dependency lists and the entire health check, replace the base value. `null` clears optional fields such as `cwd`, `env-file`, and `healthcheck`; empty maps inherit, while `depends-on: []` clears dependencies. New services need a command. Service deletion and profile inheritance are not supported. All definitions reject unknown fields, including unselected profiles; dependency and readiness validation applies to the selected result. `check` without a profile checks the base result.

Paths retain the usual configuration-directory and service-`cwd` rules. Profile names start with an ASCII letter, digit, or underscore and contain only ASCII letters, digits, `_`, `-`, or `.`. Names are case-sensitive. Runtime files use `.devd/<config-filename>/profiles/<name>/`; uppercase letters are escaped as `~hh` to stay distinct on case-insensitive filesystems. With an explicit `--state-dir`, the same `profiles/<name>/` suffix is appended. This isolates control and state files; service ports and application files still need distinct values when running environments together. `init` rejects `--profile`.

Try the [runnable dev / staging / prod example](examples/profiles/README.md).

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

### Command reference

| Command | Purpose |
| --- | --- |
| `devd start` | Start the stack in the foreground and stream logs |
| `devd stop` | Request ordered shutdown; the foreground process exits after cleanup |
| `devd restart <service>` | Restart one service using the configuration loaded at startup, rechecking dependencies |
| `devd status [--json]` | Show live state, PIDs, CPU / RSS, restart counts, and diagnostics |
| `devd logs [service] [--tail N] [--level info|warn|error] [--since DURATION] [--grep TEXT] [--follow]` | Query or follow filtered in-memory logs |
| `devd check` | Validate configuration, command quoting, dependencies, and supported settings |
| `devd graph [--format text|dot|mermaid]` | Show dependency edges and startup layers, or export a diagram |
| `devd init [--service NAME] [--command CMD]` | Create a checked starter configuration without overwriting an existing file |
| `devd snapshot save <NAME>` | Save the complete on-disk YAML under the project state directory |
| `devd snapshot restore <NAME> --output <FILENAME>` | Restore it to a new file beside the original configuration |

Commands accept `-c / --config <PATH>`, `--profile <NAME>` (except `init` and `snapshot`), `--state-dir <PATH>`, and `--color auto|always|never`. Options work before or after the subcommand:

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

## A few things to know

**It runs in the foreground.** devd manages service tasks with Tokio. Commands in other terminals reach the running instance through a Unix socket. Runtime files default to `.devd/<config-filename>/` inside the configuration directory; add `.devd/` to your project's `.gitignore`. If the socket path is too long, choose a shorter `--state-dir`. Use the same configuration and state directory when addressing the same instance.

**Restarts have a defined scope.** A manual restart targets the named service using the configuration loaded at startup. Success means that process has started, not that its health checks or any opted-in dependent restarts have completed. Manual restarts can bypass `never` and the automatic retry limit, but don't reset the cumulative restart count. Terminal failures, such as a startup failure with no retries remaining or an exhausted retry budget, trigger cleanup of the whole stack.

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

**Logs live in memory.** By default, devd retains the latest 1000 entries across the stack, with a 16 KiB limit per line. They can't be queried through `logs` after shutdown. `logs --follow` starts with the requested tail and then streams new entries until Ctrl+C or supervisor shutdown; a lagging follower exits with an error. Up to 16 followers can connect at once, leaving room for control commands. Slow foreground output can lose live entries, with a warning; a broken foreground output pipe triggers service cleanup.

Filter history or a live stream with `--level`, `--since`, and `--grep`, alone or together. For example, `devd logs api --level error --since 5m --grep database --tail 50 --follow` first shows up to 50 matching retained entries, then matching new ones. Level matches exactly; `--grep` is literal and case-sensitive against the raw message. `--since` accepts `ms`, `s`, `m`, or `h` (for example `500ms` or `2h`) and fixes its cutoff when the command starts. Filtering cannot recover entries evicted from memory.

**Diagnostics describe the running instance.** `status` returns an error when the supervisor is offline; leftover state files are for diagnosis. If you break the configuration file while devd is running, you can still use `stop`, `status`, `logs`, and `restart`. `check` performs static validation; executable availability, environment files, and probe endpoints are checked at runtime. Failed commands return a nonzero exit code.

**Resource samples describe the main process.** The supervisor samples about once per second; child processes launched by a shell or package manager are not added to the totals. CPU uses one fully occupied core as 100%, so multithreaded processes can exceed 100%. Memory is RSS, shown in MiB. Unavailable values display `-`; CPU needs two successful samples after startup or restart. JSON includes optional `resources` with `cpu_percent` (nullable during warmup), `memory_bytes`, and `sampled_at`. Samples are cleared on exit and are observational; `limits` remains unsupported.

The current scope is local process management. `init` creates a starter file; project scanning and interactive templates are planned. Hot reload, disk logs, and a TUI are also planned for later versions.

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
