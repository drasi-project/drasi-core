# HTTP and PostgreSQL tracing demo

## Run it

From the repository root, with Podman running:

```sh
make observability-demo
```

That's all. The runner:

1. Builds the example (the first build can take a few minutes).
2. Starts disposable PostgreSQL and Jaeger containers.
3. Starts the real native HTTP and PostgreSQL sources, their queries, and handled callbacks.
4. Sends an HTTP event and commits a PostgreSQL insert.
5. Verifies both eight-span traces and prints their clickable Jaeger URLs.

Open **http://127.0.0.1:16689** and select service **`drasi-source-traces`**,
or open either printed trace URL directly.

**Press Ctrl-C when finished.** The application drains its exporters, and the
runner removes only its own containers and anonymous volumes. It refuses to
replace existing containers or use occupied ports.

Requirements: Rust/Cargo, Python 3, curl, OpenSSL, and a running Podman daemon.
The images are `postgres:15-alpine` and `jaeger:2.22.0`; the container engine
downloads them if they are not cached.

## Other commands

Run the same demonstration, verify it, and immediately clean up:

```sh
make observability-demo-smoke
```

Run the script directly, from any directory:

```sh
bash /path/to/drasi-core/lib/examples/observability/demo.sh
```

Docker can be selected explicitly:

```sh
CONTAINER_ENGINE=docker make observability-demo
```

Optional port overrides:

| Variable | Default |
|---|---|
| `HTTP_PORT` | `19090` |
| `PGPORT` | `15437` |
| `JAEGER_UI_PORT` | `16689` |
| `OTLP_HTTP_PORT` | `14318` |

The runner generates a temporary database password and keeps all listeners and
trace export on loopback. No manual credentials or SQL setup are needed.

## What you will see

Each source produces one connected trace:

```text
source.publish
  pipe.send
  component.process
    query.evaluate
    query.commit
    pipe.send
    component.confirm_outputs
    sink.handle (completion=handled)
```

The verifier checks exact span counts, parent relationships, identities, numeric
counts, successful outcomes, and handled completion.

This follows [computation_runtime](../computation_runtime.rs), statically hosting
the native SDK sources. Queries use memory storage, with no bootstrap or
process-restart recovery claim. The host owns logging, sampling, and OTLP export.
Metrics are disabled for this trace-focused demo.

Upstream HTTP `traceparent` extraction, PostgreSQL I/O/decoding spans, and native
plugin-internal span export remain follow-up work.

For manual verification while the demo runs:

```sh
python3 lib/examples/observability/verify_trace.py http YOUR_HTTP_TRACE_ID
python3 lib/examples/observability/verify_trace.py postgres YOUR_POSTGRES_TRACE_ID
```

Saved Jaeger v3 responses can also be verified with `--file path/to/trace.json`.
The underlying Rust binary remains available with
`cargo run --manifest-path lib/examples/observability/Cargo.toml` when services
and its `PGPASSWORD` environment are configured separately.
