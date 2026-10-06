# Named environments

This example needs only `/bin/sh` and `sleep`. It exercises configuration profiles
in the development checkout; profiles are not in the alpha.1 release binaries.
`prod` is a configuration name, not a claim of production supervision support.

From the repository root:

```bash
cargo run --locked -- --config examples/profiles/devd.yml check --profile dev
cargo run --locked -- --config examples/profiles/devd.yml graph --profile staging
cargo run --locked -- --config examples/profiles/devd.yml start --profile dev
```

Leave that terminal open. In a second terminal:

```bash
cargo run --locked -- --config examples/profiles/devd.yml status --profile dev
cargo run --locked -- --config examples/profiles/devd.yml logs worker --profile dev
cargo run --locked -- --config examples/profiles/devd.yml restart worker --profile dev
cargo run --locked -- --config examples/profiles/devd.yml stop --profile dev
```

The worker prints `worker ready (dev)`. Run `start --profile staging` in another
terminal to keep a second supervisor alongside it. A `stop --profile dev` leaves
staging running. A command without `--profile` addresses the separate base
instance. Profiles isolate control sockets and state, not application ports or
files; choose distinct endpoints in the overlay when services need them.

The base `env` and `restart` maps supply inherited values. Each overlay replaces
only its listed map entries; `cwd`, `env-file`, `command`, `depends-on`, and
`healthcheck` replace their entire value. Optional fields accept `null` to clear
them. Empty `env` or `restart` maps inherit rather than clear; dependency lists
can be cleared with `[]`. Added services need a command; service deletion and
profile inheritance are not supported.

Default runtime directories are `.devd/devd.yml/profiles/<name>/` beside this
configuration. With `--state-dir /tmp/devd-example`, a named profile instead uses
`/tmp/devd-example/profiles/<name>/`. Supply the same config, profile and state
directory to every command addressing that instance.
