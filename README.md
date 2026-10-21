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

devd is a local development service manager written in Rust. **v1.2.0 is available for Linux, macOS, and Windows.** [Download binaries and SHA-256 checksums](https://github.com/Ceng-0324/Devd/releases/tag/v1.2.0) for Linux x86_64, macOS arm64, or Windows x86_64 MSVC.

The accident that started this project now has a diagnostic path: declare the files and links a service needs, inspect their resolution chains, and see which declared services sit downstream. Opt into incident recording before reproducing a failure, then inspect or export the saved evidence even after the supervisor stops. devd preserves what it observed; repairing the target later does not rewrite that history.

For parallel development, find instances across worktrees, wait for real readiness, and give an Agent only the controls you choose. Preview configuration changes before applying them; after a normal stop, clean only registered disposable directories. File monitoring reports changes without authorizing repairs or restarts, and reload failures stop the stack without automatic rollback.

Profiles, configuration snapshots, dependency diagrams, filtered logs, optional disk history and a live terminal view support the same workflow. `doctor` checks the environment, `events` shows the timeline, and `explain` connects service failures to recorded evidence.

v1.2.0 adds registry and `cargo-binstall` installation metadata and moves the
source to Rust edition 2024 with MSRV 1.95. The service workflow includes
bounded Node.js project scanning, reviewed task-to-consumer init,
configurable dependency readiness, native task completion gates, diagnostic
coverage, offline report comparison and separately enabled Agent limits. Public interfaces follow SemVer;
see the [compatibility and upgrade contract](docs/COMPATIBILITY.md).
See [validation scope](VALIDATION.md)
for reproducible checks and remaining user-value evidence. To contribute or
report a vulnerability, use [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

Describe your services and their dependencies in `devd.yml`, then run `devd start` in the foreground. Use another terminal to check status, read logs, or restart a service.

- **Start in dependency order.** Independent services start concurrently. Dependencies can wait for a process to start or for a TCP / HTTP / Unix socket / script health check to pass.
- **Watch service health.** TCP and Unix socket connection checks and HTTP 2xx probes track consecutive failures and report what went wrong. Socket paths resolve relative to the service working directory; a stale socket file is not healthy.
- **Handle unexpected exits.** Choose `always`, `on-failure`, or `never`, with fixed or exponential retry delays and a limit on automatic restarts.
- **Bring the logs together.** Collect stdout / stderr with timestamps, service names, and colors. Query recent output for a specific service.
- **See what each service uses.** `status` shows CPU usage and resident memory for each service's main process.
- **Clean up on the way out.** Ctrl+C or `devd stop` shuts services down in reverse dependency order and cleans up owned descendants. Unix also handles SIGTERM; Windows handles Ctrl+Break.

It's for local projects with an API, a frontend, workers, or other processes that need to run together. Your services keep their existing startup commands; devd coordinates them.

## Get it running

You'll need Rust 1.95 or newer. For a source install from the repository root:

```bash
cargo install --path . --locked
```

Once `devd` is published on crates.io, install the released CLI with:

```bash
cargo install devd --locked
```

For a prebuilt release binary, use [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```bash
cargo binstall devd
```

The binstall metadata points to the matching GitHub Release archive and installs
the binary from the archive root. Use `cargo install` for a local Rust build;
use `cargo binstall` when a release archive is available.

Run `devd init` in a project directory to create a runnable one-service starter configuration (using `sh` on Unix and Windows PowerShell on Windows). Use `--service` and `--command` to set its service name and command; `--config` chooses the output path. The command refuses to replace an existing file.

For a Node.js project, `devd init --scan node --json` reviews a bounded
`package.json` and lists candidate scripts. Select a build and consumer script
to generate a reviewed task-to-consumer configuration:

```bash
devd init --scan node --build-script build --consumer-script dev --json
devd init --scan node --build-script build --consumer-script dev --yes
```

The scan is read-only, never installs dependencies or executes scripts, and
does not overwrite an existing configuration. The generated task runs for up
to 5 minutes and defaults to a 10-minute dependency readiness timeout; use
`--dependency-timeout` to choose another readiness window. For builds longer
than 5 minutes, also edit the generated `task-timeout` before starting.
On Windows, generated npm, pnpm and yarn invocations use their `.cmd`
entrypoints.

For a two-service Unix example, use this configuration:

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

`cwd` is relative to the configuration directory; `env-file` is relative to the service's `cwd`. Commands have no implicit shell. Important files can also be declared as launch prerequisites; see the [command guide](docs/COMMANDS.md#file-prerequisites-and-path-rules).

## The everyday loop

Once the stack is running, most days look like this in another terminal:

```bash
devd wait --timeout 30s       # Wait for readiness before running tests
devd status                  # See what's running and what's wrong
devd logs api --tail 50       # Read recent application output
devd explain api             # Follow the recorded failure evidence
devd restart api             # Explicitly restart this service
devd stop                    # Done for the day; shut it down in order
```

Use the same `--config`, `--profile` and `--state-dir` for the same instance. A successful restart means the process started; use `wait` when you need it ready.

## When something breaks

The link is still there. The target is gone. “The service broke” doesn't tell you much. You need to see where the chain broke, which declared services might depend on it, and keep the evidence after the original environment disappears.

Declare important files or links with `requires`. Run `devd doctor` to inspect their resolution chains and `devd impact api --require 0` to see declared downstream services. Opt into `monitor-requires: true` for runtime condition changes, then enable incident recording before reproducing the failure:

```bash
devd start --record-incidents  # Keep this foreground terminal running
```

In another terminal, wait for readiness, reproduce the problem, and inspect what was saved:

```bash
devd incidents list
devd incidents show INCIDENT_UUID
devd export --incident INCIDENT_UUID --output incident.json
# Copy the JSON to another machine or an empty directory:
devd diagnose incident.json
```

Replace `INCIDENT_UUID` with a full ID from the list. The capture window lasts up to five seconds; export after it finishes. Add `--stored` when querying after shutdown. Relationships show declared impact, not proof of a root cause. Fixing the target later doesn't rewrite the evidence.

If the missing resource is a registered, rebuildable instance directory, review a recovery recipe. Stop the whole stack normally and wait for the supervisor to exit successfully before running:

```bash
devd recover --recipe recipe.yml --dry-run
devd recover --recipe recipe.yml --apply --plan PLAN_ID
```

Use the plan ID returned by that preview. Commands run with your OS permissions, so review them yourself; recovery doesn't start services. Check the results, then `start` again and `wait`. Shared resources and unregistered directories aren't taken over, and a failed repair stays a failed repair.

[Full incident reproduction](docs/COMMANDS.md#find-and-export-a-recorded-incident) · [Recovery recipes](docs/COMMANDS.md#run-a-reviewed-recovery-recipe)

## Find the part you need

| What you're doing | Where to look |
| --- | --- |
| Running several worktrees | [Instance discovery](docs/COMMANDS.md#instance-discovery), [ports and paths](docs/COMMANDS.md#explicit-instance-ports-and-paths) |
| Starting or changing an environment | [Readiness](docs/COMMANDS.md#wait-for-readiness), [profiles](docs/COMMANDS.md#named-environments), [reload](docs/COMMANDS.md#preview-and-apply-configuration-changes) |
| Finishing a build before starting consumers | [One-shot tasks](docs/COMMANDS.md#one-shot-tasks), [runnable example](examples/tasks/README.md) |
| Saving and handing off failure evidence | [Coverage and blind spots](docs/COMMANDS.md#see-what-diagnostics-can-cover), [Exports](docs/COMMANDS.md#export-diagnostics), [incidents](docs/COMMANDS.md#keep-bounded-incident-evidence), [offline diagnosis](docs/COMMANDS.md#diagnose-a-report-on-another-machine), [comparison](docs/COMMANDS.md#compare-two-transferred-reports) |
| Comparing, rebuilding or cleaning resources | [Baselines](docs/COMMANDS.md#detect-environment-drift), [recovery](docs/COMMANDS.md#run-a-reviewed-recovery-recipe), [cleanup](docs/COMMANDS.md#clean-disposable-instance-data) |
| Connecting another tool | [MCP](docs/COMMANDS.md#connect-an-mcp-client), [Agent JSON interface](docs/COMMANDS.md#a-scoped-interface-for-agents) |

Full options and examples live in the [command guide](docs/COMMANDS.md). Platform behavior, log retention and permission boundaries are in [usage notes](docs/USAGE_NOTES.md).

## Development and validation

Try the [API, web and worker example](examples/local-stack/README.md) for a complete stack with repeatable failure-recovery exercises.

Trying it on your own project? The [field guide](https://github.com/Ceng-0324/Devd/blob/dev/docs/FIELD_TESTING.md) explains how to try your existing workflow and keep feedback to one record, including the parts that did not work.

Start with [contributing](CONTRIBUTING.md); specific checks live in [tests and manual validation](tests/README.md). See the [release checklist](RELEASING.md) for publishing and the [security policy](SECURITY.md) for vulnerability reports.

[Changelog](CHANGELOG.md) · [MIT license](LICENSE) · [Architecture (中文)](ARCHITECTURE.md)
