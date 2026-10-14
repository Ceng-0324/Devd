# Finish the build before starting the next step

From the repository root:

```bash
cargo run --locked -- check --config examples/tasks/devd.yml
cargo run --locked -- start --config examples/tasks/devd.yml
```

Python 3 is required; use `python` instead of `python3` in the YAML if that is
your platform's executable name. devd runs `generate`, waits for its successful
exit and process cleanup, then runs `consume`. Both are tasks, so the supervisor
exits 0 when they finish. The generated text is application output, not a marker
that devd polls. Change the producer to fail and the consumer will not run even
if an old output file remains.

Tasks default to no automatic retry. Add `restart: {policy: on-failure}` only
when repeating the command is appropriate for your application. For a mixed
stack, declare the consumer as a regular service and use `devd wait` before the
next external step. See the [task guide](../../docs/COMMANDS.md#one-shot-tasks)
for rerun, timeout, and reload semantics.
