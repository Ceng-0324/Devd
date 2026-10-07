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

devd is a local development service manager written in Rust. **v0.4.0-alpha.1 is available for Linux, macOS, and Windows.**

v0.4 brings opt-in disk logs, an interactive terminal view, resource warnings and explicitly enabled recovery, custom script health checks, and Windows process supervision. Download Linux x86_64, macOS Apple Silicon, or Windows x86_64 binaries and their SHA-256 checksums from [GitHub Releases](https://github.com/Ceng-0324/Devd/releases/tag/v0.4.0-alpha.1), or install from source. The release passed native CI and recovery smoke tests on all three platforms.

It also includes CPU and memory samples, opt-in restarts after dependency recovery, named configuration profiles, configuration snapshots, dependency diagram export, and log filters.

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

### Command reference

| Command | Purpose |
| --- | --- |
| `devd start [--persist-logs] [--log-max-size MiB] [--log-keep N]` | Start the stack in the foreground, optionally retaining disk logs |
| `devd stop` | Request ordered shutdown; the foreground process exits after cleanup |
| `devd restart <service>` | Restart one service using the configuration loaded at startup, rechecking dependencies |
| `devd status [--json]` | Show live state, PIDs, CPU / RSS, restart counts, and diagnostics |
| `devd top` | Inspect a running stack and its live logs in an interactive terminal |
| `devd logs [service] [--tail N] [--level info|warn|error] [--since DURATION] [--grep TEXT] [--follow \| --stored]` | Query live memory or offline disk logs; follow live output |
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

**Diagnostics describe the running instance.** `status` returns an error when the supervisor is offline; leftover state files are for diagnosis. If you break the configuration file while devd is running, you can still use `stop`, `status`, `logs`, `restart`, and `top`. `check` performs static validation; executable availability, environment files, and probe endpoints are checked at runtime. Failed commands return a nonzero exit code.

`devd top` connects to the same running instance as `status` and `logs`. It shows service state, PID, restart count, CPU/RSS and a bounded live log tail. Use Up/Down (or j/k) to select a service, `r` to restart it, Page Up/Down to scroll logs, and End to return to the latest entries. `s` asks for confirmation before stopping the entire stack; Enter or `s` confirms, Esc or `n` cancels. `q` and Ctrl+C only close the view. It requires an interactive terminal and does not start a supervisor.

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

The same metric must exceed its threshold in 3 consecutive valid samples (roughly one sample per second). A value within range or a missing value resets that metric's count; CPU warmup resets only CPU's count. The decision is retained until that process generation ends. devd stops the process group, drains logs, applies the existing backoff, and rechecks dependencies before starting again. Resource, crash, health, and dependency recovery restarts share the cumulative restart budget; exhaustion fails the service and shuts down the stack. Stop interrupts backoff, and manual restart can supersede it. The reason appears in logs and failure diagnostics. `on-exceed: restart` conflicts with `restart.policy: never` and is rejected before startup. Profiles replace the entire `limits` block, so a replacement that omits `on-exceed` returns to `warn`; `limits: null` disables thresholds. Configuration changes take effect on the next supervisor start, without root privileges or an interactive permission prompt.

The current scope is local process management. `init` creates a starter file; project scanning and interactive templates are planned. Hot reload remains a later module.

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
