# Try devd on a real project

[简体中文](FIELD_TESTING.zh-CN.md) · [Back to README](../README.md)

A working example proves that the example works. The useful question is what
happens when you replace two terminals, a readiness guess, and a log hunt in your
own project. If getting started takes longer than the job itself, record that
too. The tool does not need you to make excuses for it.

This is the shared contract for field feedback. Engineering acceptance still
follows [VALIDATION.md](../VALIDATION.md) and [RELEASING.md](../RELEASING.md);
each release does not need another questionnaire.

## Keep the evidence identifiable

| Evidence | What it establishes | What it does not establish |
| --- | --- | --- |
| Engineering validation: tests, examples, synthetic failures, pinned comparisons | Assertions hold for a specified version and scenario | Usability, adoption, or time saved |
| Maintainer field trial: a maintainer doing real work in another project | Friction, outcomes, and actual continued use in that project | Independent endorsement or general applicability |
| Independent field trial: a non-maintainer doing real work in their own project | That person's experience and observed repeated use | Universal benefits or unmeasured performance gains |

The two field categories use the same record. State the relationship and any
assistance, including an Agent preparing configuration. Deliberately injected
faults remain injected faults even in a real project; an automated smoke remains
engineering evidence. Missing independent feedback stays **unverified**. A
maintainer trial is useful evidence of its own, without filling that gap.

There is no fixed user count, duration, success requirement, or all-features
checklist. Keep failures and abandoned attempts. Use not tested/not applicable
for unused features and unknown for unmeasured time. An intention to continue
is not observed repeated use. File separate bugs when useful and link them to
the same trial, without counting them as additional users.

## Prepare a trial in your project

Choose a project and a task you already need to do. Record its current revision,
local changes, original startup commands, required runtimes, and any setup help.
Use the intended devd binary and record its version and full source SHA or
release artifact provenance; newer source commands may be absent from old releases.

Keep trial databases, reports, and runtime state separate from existing data.
Review each service command, working directory, application bind address, and
readiness check. Relative service working directories resolve from the config
file, so place the configuration accordingly. Explicitly declare dependencies;
a passing health endpoint is only one observation, not proof of business behavior.

Run `check`, `graph`, `doctor`, and static `coverage` before starting. Use the
same config/profile/state selection for `start`, `wait`, `status`, logs, controls,
and exports. Keep the foreground supervisor open. After startup, inspect
`coverage --online` without enabling collectors just to clear its findings.

Complete the actual task through the application's real backend. Check the
workflow you use: for example, create and refresh data, make an intended edit,
inspect aggregated logs, or restart a service and wait for readiness again.
Record existing application failures and any workaround separately from devd
behavior. A mock or demo session does not validate backend integration.

Expand to task gates, incident recording, Agent/MCP, or recovery only when the
job calls for it. These capabilities retain their separate opt-in permissions;
a first trial does not require every feature. Preserve failures, untested areas,
and unknown observations instead of manufacturing a successful checklist.

When finished, stop normally and wait for the supervisor to exit. Review saved
reports before sharing, then remove only trial-owned configuration, data, and
artifacts. Preserve pre-existing files, dependencies, and local changes. Refer to
the [command guide](COMMANDS.md) for command-specific limits and behavior.

## One record is enough

Send the record to the maintainer or use the English
[Field feedback issue template](../.github/ISSUE_TEMPLATE/field-feedback.yml).
Anonymous projects are welcome; private repositories, tokens, environment
contents, and full logs are not required. Follow [SECURITY.md](../SECURITY.md)
for vulnerability reports.

```text
Date and relationship: maintainer field trial / independent field trial; who helped?
Environment: devd version, full SHA/artifact provenance, local changes, OS/architecture; project revision and stack.
Original workflow: previous startup commands and the actual job to complete.
Steps and scope: what was done, used, not tested, or not applicable; unmeasured time is unknown.
Friction: original errors, failed steps, workarounds, reasons for stopping; none if absent.
Outcome and evidence: completed/partial/blocked/abandoned; reports or issue links; real incident/injected fault/no incident.
Next use: continue/defer/stop/undecided and why; append dates and real tasks if used again.
```

Update [Value evidence](../VALIDATION.md#value-evidence) when actual field
records arrive. Agent-driven integration exercises remain engineering evidence;
a plan to try the tool is not a completed human field trial.
