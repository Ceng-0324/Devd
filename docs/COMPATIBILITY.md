# Compatibility and upgrades

[简体中文](COMPATIBILITY.zh-CN.md)

A stable version should let you upgrade without relearning how your project
starts. This is the compatibility contract for v1.0.0 and subsequent 1.x releases.

## Version numbers

devd follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html):
after `1.0.0`, an incompatible change to the public contract below requires
MAJOR, backward-compatible functionality or deprecation requires MINOR, and a
backward-compatible bug fix requires PATCH. For example, from `1.2.3` these
become `2.0.0`, `1.3.0` and `1.2.4`. Pure documentation, tests and internal
refactoring do not by themselves require a bump.

Published prereleases use `-alpha.N`, `-beta.N` or `-rc.N` and precede the
corresponding stable release. Published versions are immutable. YAML, evidence
and MCP protocol versions are separate from the application version; a schema
bump does not permit a silent public break within 1.x. Historical 0.x releases
were experimental. Release preparation follows the repository's version policy.

## What stays stable

| Surface | Supported contract |
| --- | --- |
| CLI | Documented commands, flags, selectors and exit-code meanings remain compatible within 1.x. New optional flags/commands may be added. Human-readable wording, colors and table layout are not parsing interfaces. |
| YAML | `version: "1"`, named profiles, documented keys and existing snake_case aliases remain readable. Omitted `kind` still means `service`; shorthand dependencies still mean `started`. The optional `dependency-timeout` field controls readiness waits and defaults to 30 seconds. Unknown or conflicting settings are rejected. |
| Agent | JSON-lines schema 1, method names, target/grant checks and documented error codes. Requests reject unknown fields; clients accept additional response fields and check `ok`, then the report outcome. |
| MCP | stdio with protocol negotiation for `2025-06-18` and `2025-11-25`, documented tools and argument shapes. Protocol version is separate from devd and evidence versions. Tool success can carry an unsuccessful report outcome. |
| Transferred reports | `diagnose`, `compare` and offline MCP accept export schemas 1 (live), 2 (stored run) and 3 (incident). Embedded incident artifacts 1/2 retain their historical limits; schema 1 has no retained relationship graph. Unsupported artifact versions remain unknown evidence; unsupported outer export versions are rejected. |
| Other versioned data | Current baseline, snapshot and recovery recipe formats keep their own version checks. A configuration snapshot restores into a new file; it never restores a process. A baseline is metadata evidence, not a backup of contents. |

Optional additions must not silently enable monitoring, persistence, Agent
controls, automatic recovery or directory cleanup. A fix may reject malformed or
unsafe input that slipped through validation; that is not a promise to preserve
a bug. A required incompatible change needs an explicit version change and
migration guidance, not a silent reinterpretation of existing fields.

Structured consumers should ignore additional object fields, check schema
versions, and treat unknown enum/outcome/error values conservatively. Old readers
are not promised to understand new task events or later schemas. Keep the original
report and upgrade the reader when it says a version is unsupported. Missing
fields, truncation and unknown evidence must never become proof of success.

The Rust library API, local supervisor wire protocol, private state files,
plan digests and internal file layout are not stable integration APIs. Use the
CLI, Agent or MCP interfaces. Use the same devd binary for an active supervisor
and its clients; mixed-version live control is not a supported upgrade method.

## Exit codes and results

| Exit | Meaning |
| --- | --- |
| `0` | The command fulfilled its contract. For `coverage`, `diagnose` and `compare`, this means a report was produced, including differences or unknown evidence. It does not mean healthy, identical or complete. |
| `1` | Runtime/configuration/I/O failure, or a negative result where the command is a gate: failed/timed-out `wait`, failed task-only `start`, changed/unknown `baseline check`, incomplete reload/cleanup/recovery. A JSON report may already have been written. |
| `2` | Invalid CLI syntax or argument value detected by argument parsing. |

`restart` acknowledges a new attempt; use `wait` for readiness or task completion.
`stop` requests shutdown; wait for the foreground `start` process to exit before
cleanup or recovery. Signal termination by the OS may have platform-specific
status. Agent/MCP request errors belong to their JSON replies, not one process
exit code per request. Their `ok`/tool-success flags mean that a report exists;
inspect `outcome`, or `result.outcome` inside a deduplicated control envelope.

Stable policy codes include `operation-key-required`, `operation-key-conflict`,
`budget-out-of-scope`, `budget-exhausted`, `deduplication-capacity`, `busy` and
`outcome-unknown`. Existing grant/target failures include `permission-denied`,
`wrong-instance` and `stale-run`. Diagnostic prose is not a machine contract.

## Moving from v0.9

Stop the old supervisor normally and wait for it to exit. Preserve any evidence
you need, install the new binary, run `check` with each profile you use, and start
again. Reconnect adapters and obtain new instance/run targets. Old plans and
receipts must not be treated as authorization for a new run. devd does not adopt
stored PIDs or restore old processes. Do not delete state merely to upgrade.

Existing service YAML needs no task declaration. To replace a build wrapper,
explicitly use `kind: task` and `condition: completed-successfully`; tasks default
to no automatic retry and must not use service-only health/recovery settings.
A prior output file does not satisfy a new task generation. See the
[task guide](COMMANDS.md#one-shot-tasks).

`init --scan node` is an additive CLI workflow. It reads a bounded regular
`package.json`, prints a reviewable candidate by default, and only writes with
`--yes` when the destination does not exist. It does not change existing
`init` behavior or execute package scripts. Generated task stacks use the
`dependency-timeout` field; choose a value at least as long as the generated
5-minute task timeout.

Existing Agent controls keep their default response shape. `--restart-budget`
and `--deduplicate` are independently opt-in launcher choices and do not grant
controls. With deduplication, clients must supply `operation_key` and understand
`{operation_key, operation_status, result?}`. Keep the same key/payload when a
reply is lost; unknown outcomes require evidence review. Sessions and retention
bound the guarantee: reconnecting resets budgets and keys. See the
[Agent guide](COMMANDS.md#a-scoped-interface-for-agents).

## What backs this up

`tests/compatibility.rs` reads unmodified live/stored/incident exports generated
by the published v0.9.0-alpha.1 macOS binary, with hashes and source revision in
`tests/fixtures/compat/v0.9/manifest.json`. It also checks legacy YAML aliases and
profiles without executing their commands. Existing diagnosis tests cover older
missing fields and schema-1 incidents, invalid/future schemas and partial data.
Native lifecycle suites cover migration-sensitive task, reload and control behavior.

The [validation guide](../VALIDATION.md) describes end-to-end and comparison
checks. Compatibility evidence is engineering evidence. Use the shared
[field testing contract](https://github.com/Ceng-0324/Devd/blob/dev/docs/FIELD_TESTING.md)
to record maintainer trials and independent trials separately; current field
evidence and missing feedback are tracked in the validation guide.
