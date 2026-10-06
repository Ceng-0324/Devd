# A real local stack

This example runs three Python 3.9+ processes using only the standard library:
an HTTP API that counts submitted jobs, a worker that submits a job each second,
and a web page that displays the API's status through a frontend proxy.

From the repository root, with `devd` installed:

```sh
devd check --config examples/local-stack/devd.yml
devd start --config examples/local-stack/devd.yml
```

Open http://127.0.0.1:8732. Ports 8731 and 8732 must be available. The web and
worker services wait for the API's HTTP health check; they retry API calls when
it is temporarily unavailable. The job counter is intentionally in memory and
resets when the API restarts.

In another terminal:

```sh
devd status --config examples/local-stack/devd.yml
devd logs worker --config examples/local-stack/devd.yml
devd restart api --config examples/local-stack/devd.yml
devd stop --config examples/local-stack/devd.yml
```

The supplied `smoke.py` exercises the web page, jobs, repeated manual restarts,
a killed API process, a failing health check, and final cleanup. It copies the
example into a temporary project and selects local ports; it does not modify
your running example. Run from the repository root:

```sh
cargo build --release --locked
python3 examples/local-stack/smoke.py --binary target/release/devd --duration 60 --restarts 3
```

On Windows, use `python examples/local-stack/smoke.py --binary target/release/devd.exe --duration 60 --restarts 3`.
The smoke script selects the interpreter used to run it. For direct startup
with the supplied YAML, replace `python3` with your installed Python command if
necessary. Services handle Ctrl+Break on Windows for graceful shutdown.

The JSON report records startup time and sampled supervisor RSS/CPU from `ps`
on Unix or native process counters on Windows (working set and lifetime CPU).
These are observations on your machine, not performance guarantees; `ps` CPU
averaging differs by platform. Use `--duration 1800` for a longer local soak.
The API is deliberately a local demo with no authentication or persistent data.

On macOS arm64, the release build from the pre-release v0.1 checkout completed
the 60-second run with three manual restarts, one killed API, and one induced
health failure. Startup took 1.226 seconds; 59 `ps` samples reported a maximum
supervisor RSS of 9312 KiB and a maximum CPU reading of 0.5%. The test also
observed resumed job processing and no remaining service PIDs after shutdown.
This single-machine sample does not establish longer-run or cross-platform
performance; the CI smoke test checks behavior separately on Linux, macOS, and Windows.
