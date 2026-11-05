# HTTP, PostgreSQL, and native plugin telemetry demo

## Run it

From the repository root, with Podman running:

```sh
make observability-demo
```

That's all. The runner:

1. Builds the host and a **separate native inspector library** (the first build can take a few minutes).
2. Starts disposable PostgreSQL and Jaeger containers.
3. Starts the HTTP/PostgreSQL sources, their queries, two dynamically loaded inspectors, handled callbacks, and a telemetry dashboard.
4. Sends an HTTP event and commits a PostgreSQL insert.
5. Verifies both twelve-span traces, native metrics, and correlated logs, then prints their URLs.

Open **http://127.0.0.1:19091** for the dashboard:

- **Traces:** links to the real Jaeger traces, including `plugin.inspect` from the cdylib.
- **Metrics:** live scoped counter/gauge/histogram samples, with an option to include framework query/component/queue measurements.
- **Logs:** original plugin timestamps, safe fields, and trace/span IDs.
- **Collection:** current bounded span-buffer usage and explicit span/log loss counts.

Jaeger remains at **http://127.0.0.1:16689**, service **`drasi-source-traces`**.
The host exposes Prometheus text at **http://127.0.0.1:19091/metrics** and
structured native logs at **http://127.0.0.1:19091/logs**. No additional metrics
container, scraper, or global recorder installation is required.

**Press Ctrl-C when finished.** The application stops its producers, drains native
span collection and logs, and shuts down its host exporter. The
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

Show that logs and metrics do not depend on sampled traces:

```sh
TRACE_SAMPLING=never make observability-demo
```

In this mode, the same native inspector work runs, metrics advance, and logs
retain valid **unsampled** trace/span IDs. No event traces are expected in
Jaeger, and the runner skips trace-export checks rather than waiting for them.
Sampling is host-owned; the plugin does not override the decision.

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
| `TELEMETRY_PORT` | `19091` |

The runner generates a temporary database password and keeps all listeners and
trace export on loopback. No manual credentials or SQL setup are needed.

## What you will see

Each source produces one connected trace:

```text
source.publish
  pipe.send
  component.process (query)
    query.evaluate
    query.commit
    pipe.send
    component.confirm_outputs
    component.process (native inspector)
      plugin.inspect
        pipe.send
        sink.handle (completion=handled)
      component.confirm_outputs
```

The verifier checks exact span counts, parent relationships, identities, numeric
counts, successful outcomes, and handled completion. The plugin explicitly
propagates its internal span context with its output; the handled callback
therefore remains connected even after that native span has finished.

This follows [computation_runtime](../computation_runtime.rs), statically hosting
the native SDK sources. The **inspector** is built independently from
[native-plugin/](native-plugin/) and loaded through the Host SDK, not statically
linked or substituted with an in-process test fixture. It depends only on the
native SDK/shared contracts, not the query runtime or legacy SDK.

The same loaded library serves two component bindings (`http-inspect` and
`postgres-inspect`). Host-assigned labels separate their metrics and logs.
The inspector checksums serialized record images without changing query rows
or logging payloads. Its optional internal metrics are:

| Rust instrument | Prometheus instrument | Meaning |
|---|---|---|
| `drasi.plugin.drasi-demo-inspector.record_bytes_inspected` | `drasi_plugin_drasi_demo_inspector_record_bytes_inspected` | Counter of serialized image bytes actually inspected |
| `drasi.plugin.drasi-demo-inspector.inspection.duration` | `drasi_plugin_drasi_demo_inspector_inspection_duration_{bucket,sum,count}` | Histogram of inspection work, in seconds |
| `drasi.plugin.drasi-demo-inspector.inspection_active` | `drasi_plugin_drasi_demo_inspector_inspection_active` | Gauge set during inspection, then returned to zero |

These are not another increment of the framework's processed-input or emitted-row
counters. The host also enables the existing query/reaction/component/queue
metrics. Metrics are applied directly to the host recorder, outside the bounded
native-span queue. Both host and plugin spans share one host-owned OTel batch
processor and resource identity; the plugin owns no exporter.

Queries use memory storage. Bootstrap/recovery/replacement demonstrations and
performance qualification are not included. HTTP `traceparent` extraction and
source-specific PostgreSQL I/O/decoding instrumentation remain separate work.

For manual verification while the demo runs:

```sh
python3 lib/examples/observability/verify_trace.py http YOUR_HTTP_TRACE_ID
python3 lib/examples/observability/verify_trace.py postgres YOUR_POSTGRES_TRACE_ID
python3 lib/examples/observability/verify_telemetry.py
```

Saved Jaeger v3 responses can also be verified with `--file path/to/trace.json`.
The underlying Rust binary remains available with
`cargo run --manifest-path lib/examples/observability/Cargo.toml` when services
and its `PGPASSWORD` environment are configured separately. Build the inspector
first with:

```sh
cargo build --manifest-path lib/examples/observability/native-plugin/Cargo.toml --target-dir target --locked
```

The runner passes its exact library path as `DEMO_PLUGIN_PATH`; a manual run can
set the same variable. `JAEGER_URL` controls the dashboard's links when Jaeger's
UI port is customized. All five listener ports must be distinct.

To generate another HTTP trace while the demo is live:

```sh
curl --fail-with-body http://127.0.0.1:19090/sources/http/events \
  -H 'Content-Type: application/json' \
  --data '{"operation":"insert","element":{"type":"node","id":"http-2","labels":["trace_items"],"properties":{"id":2,"name":"Another demo","value":12}}}'
```

The dashboard refreshes every two seconds. The automatic verification is for the
initial seeded events; manually sending more events increases the metric counts.
