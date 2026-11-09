# Observability demo suite

The main demo is **HTTP-only**: one source, one continuous query, and one
application reaction (two reactions in the latency lab). PostgreSQL is a
separate, optional CDC demo selected explicitly with a Make command.
Both reuse the host, dashboard, and runner; they never
silently start two source pipelines.

For a single-command presentation of tracing and metrics, use the
[HTTP walkthrough and presenter notes](WALKTHROUGH.md).

| Scenario | Start command | Actions and visible outcome |
|---|---|---|
| Main: follow one HTTP change | `make observability-demo` | Dashboard buttons send real HTTP inserts, updates, and deletes through one observable pipeline. |
| Main: continue an upstream trace | `make observability-demo-incoming-trace` | Seed an HTTP event with W3C context; assert the supplied trace ID and remote parent survive through handled reaction completion. |
| Main: signal controls | `make observability-demo-controls` | Compare the same HTTP workload under tracing, sampling, metrics, and logging policies. |
| Main: latency/bottleneck lab | `make observability-demo-latency` | Pause the slow sink, send up to four HTTP inputs, and release it; acceptance is not handling. |
| Optional: PostgreSQL CDC | `make observability-demo-postgres` | Execute SQL and follow one PostgreSQL replication pipeline. No HTTP source runs. |

All main HTTP scenarios start only Jaeger, not a PostgreSQL container.
The optional demo starts PostgreSQL and Jaeger, but no HTTP listener.

The dashboard supplies topology, action buttons, an expected/observed table,
trace links, and a generated restart command for scenario/policy selection.
Policies are **startup settings**: Ctrl-C the runner before executing that command.
Changing a selector does not silently reconfigure an active graph.

## Scenario experiments

**Follow one change:** the runner first verifies one seeded HTTP trace.
It then exercises real HTTP insert/update/delete actions and resets the experiment.
Use the dashboard for further actions; all input buttons affect HTTP only.
Stop external HTTP producers when checking exact counts.
Startup therefore already produces event traces, plus graph lifecycle traces.
These are self-checks, not bootstrap snapshots. Reset does not erase Jaeger traces.

**Signal controls:** logging is independent of tracing and metrics. `never`
sampling keeps unsampled context propagation but exports no event traces for
context-free inputs; an incoming parent retains its own sampling decision.
`TRACE_ENABLED=false` creates no local event spans, but incoming context can
still propagate. Metrics are not sampled.
Basic removes debug-level graph spans; Persistence adds storage measurements to
Basic rather than retaining Debug spans.
Off disables tracing and metrics, not logging. Metric exclusions are literal
substrings of the original dotted instrument name, not Prometheus names.

```sh
TRACE_SAMPLING=never make observability-demo-controls
TRACE_ENABLED=false make observability-demo-controls
METRICS_ENABLED=false make observability-demo-controls
TELEMETRY_PROFILE=basic make observability-demo-controls
TELEMETRY_PROFILE=persistence make observability-demo-controls
TELEMETRY_PROFILE=off make observability-demo-controls
METRIC_EXCLUDE=drasi.query make observability-demo-controls
DEMO_LOG_LEVEL=off make observability-demo-controls
```

**Latency lab:** ten `demo_copies` nodes prepare a real query fan-out. After
preparation, one inserted `trace_items` node triggers **one input envelope,
ten result-row changes, and one output envelope handled independently by two
sinks**. Query output goes directly to the two application reactions.
Both sinks return actual callback completion; neither is acceptance-only.
The slow callback waits at a controllable gate and then performs a 300 ms delay.
Its physical pipe has capacity two, so a four-input burst retains queued work.

While slow handling is paused, fast samples advance and slow samples do not.
After release, four inputs yield forty result rows and **four latency samples
per sink**, not forty. Update diffs can emit paired changes; the ten-row
assertion deliberately uses inserts. Timing is in seconds, starts at framework
source admission, includes downstream queue/pause/handling time, and excludes
upstream HTTP/database time. It remains explicitly enabled only in the latency
scenario. Sampling Never and tracing Off do not disable these measurements.

The runner automatically verifies pause/queue/release behavior and resets before
leaving the UI live. Reset releases the gate, awaits actual handling, deletes
only dashboard-owned nodes, and captures a new observation baseline. Cumulative
metrics, native log history, prepared fan-out nodes, and manually inserted data
are not erased. Source acceptance is not handling or durable exactly-once delivery.
HTTP action failures are reported explicitly; an uncertain transport failure may
require restarting the disposable experiment rather than blindly retrying.

Actions are also available at `POST /action`, for example:

```sh
curl --fail-with-body http://127.0.0.1:19091/action \
  -H 'Content-Type: application/json' --data '{"action":"insert"}'
```

Main-demo actions: `insert`, `update`, `delete`, `burst`, `reset`; latency also
supports `prepare` (runner-managed once), `pause`, and `release`.
`GET /scenario` reports callback observations and the cumulative-metric baseline.

## Continue an upstream trace

```sh
make observability-demo-incoming-trace
```

This uses the normal main pipeline, with tracing enabled and sampling Always.
The runner generates a fresh upstream trace ID and parent span ID, prints the
actual `traceparent` header, and sends it with `tracestate: demo=upstream`.
It verifies the callback's exact trace ID and all eight exported Debug spans,
including `source.publish` parentage and new local span IDs. The upstream span
itself is not fabricated or exported by Drasi. Normal dashboard actions afterward
have no headers and create independent traces, not children of the seed.

Alternatively, after **"Demo is live"** in `make observability-demo`, send:

```sh
curl --fail-with-body http://127.0.0.1:19090/sources/http/events \
  -H 'Content-Type: application/json' \
  -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' \
  -H 'tracestate: demo=upstream' \
  --data '{"operation":"insert","element":{"type":"node","id":"external-trace-1","labels":["trace_items"],"properties":{"id":777,"name":"Upstream trace","value":7}}}'

python3 lib/examples/observability/verify_trace.py \
  http 4bf92f3577b34da6a3ce929d0e0e4736 --parent-span-id 00f067aa0ba902b7
```

Open <http://127.0.0.1:16689/trace/4bf92f3577b34da6a3ce929d0e0e4736>.
Use a fresh trace ID and element ID on subsequent attempts: reusing a trace ID
combines separate requests in Jaeger, so the exact-span-count verifier rejects it.
Change the final flags to `00` to propagate an unsampled parent: processing and
metrics continue but no event trace is exported, even with local sampling Always.
Missing metadata starts a new trace; malformed metadata warns and starts a fresh
trace without rejecting valid data. Arbitrary business payload `trace_id` fields
are not treated as tracing metadata.

## Optional PostgreSQL CDC demo

```sh
make observability-demo-postgres
```

This mode has exactly one pipeline:

```text
SQL → PostgreSQL logical replication → postgres-query → postgres-output
```

The runner creates an empty `trace_items` table and publication/slot
`drasi_trace_demo`, waits for replication readiness, then seeds a live insert.
It verifies the trace and telemetry, exercises SQL insert/update/delete, and
establishes an observation baseline. There is **no bootstrapper**, HTTP source,
or database-state recovery demonstration.

The dashboard displays the PostgreSQL topology, its own query/handling counters,
and a copyable SQL command instead of HTTP input buttons. With default ports:

```sh
podman exec -i drasi-otel-demo-postgres-15437 psql -U postgres -v ON_ERROR_STOP=1 <<'SQL'
INSERT INTO trace_items VALUES (2, 'SQL demo', 7);
UPDATE trace_items SET value = 8 WHERE id = 2;
DELETE FROM trace_items WHERE id = 2;
SQL
```

Each statement commits separately. The dashboard and runner print the actual
container command when the engine or PostgreSQL port is customized.
**Reset observation baseline** does not delete SQL rows or reset cumulative metrics.
HTTP input actions are rejected in this mode rather than silently affecting a
different pipeline. Stop the old runner with Ctrl-C before switching demos on
the same ports.

To verify the optional demo and immediately clean up:

```sh
make observability-demo-postgres-smoke
```

## Verification

Verify the main scenarios, incoming-trace case and optional CDC mode across fifteen
policy/workload variants, then clean up:

```sh
make observability-demo-suite-smoke
```

Each variant creates and removes only its own disposable containers. This covers
Always/Never/no-tracing latency, Basic/Debug/Persistence/Off controls, disabled
metrics/logging, query metric exclusion, and optional SQL-driven CDC. Exact default and Basic span counts,
parentage, query attribution, row/envelope distinctions, and reset are checked.
This is demo qualification, not complete workspace or performance qualification.

Container-free verifier and host checks:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s lib/examples/observability -p 'test_*.py'
node --test lib/examples/observability/test_dashboard.js
cargo test --manifest-path lib/examples/observability/Cargo.toml --target-dir target/observability-demo --locked
```

Node.js is needed only for the renderer regression tests, not to run the demo.

The standalone host uses `target/observability-demo`, separate from workspace
plugin/test artifacts. Its `lib`+`cdylib` dependencies emit unhashed libraries;
sharing the workspace's target directory can produce conflicting SDK/error
type identities after another workspace build. If `CARGO_TARGET_DIR` is set,
the runner uses its `observability-demo` child directory instead. The first
isolated build may take longer; no workspace cache cleanup is necessary.

## Run it

From the repository root, with Podman running:

```sh
make observability-demo
```

That's all. The runner:

1. Builds the source-query-reaction host (the first build can take a few minutes).
2. Starts a disposable Jaeger container only.
3. Starts one HTTP source, its query, an application reaction, and a telemetry dashboard.
4. Sends one HTTP event.
5. Verifies its eight-span trace, framework metrics, and correlated handling logs; exercises insert/update/delete and resets the experiment.

The runner prints the dashboard, metrics, logs, and Jaeger links as soon as the
dashboard is ready, before automatic verification. Wait for **"Demo is live"**
before sending dashboard events so the initial exact-count checks are not
mixed with manual input. Verification failures are reported explicitly.

Open **http://127.0.0.1:19091** for the dashboard:

- **Traces:** links to real source/query/reaction traces.
- **Metrics:** framework source/query/handling counters and evaluation histograms,
  with an option to include all component/queue measurements.
- **Logs:** actual handling summaries with added/updated/deleted result counts, local timestamps
  with original UTC timestamps on hover, direct Jaeger links, and expandable
  trace/span IDs. Raw structured data remains available at `/logs`.
- **Collection:** current bounded span-buffer usage and explicit span/log loss counts.

Jaeger remains at **http://127.0.0.1:16689**, service **`drasi-observability-demo`**.
The host exposes Prometheus text at **http://127.0.0.1:19091/metrics** and
structured native logs at **http://127.0.0.1:19091/logs**. No additional metrics
container, scraper, or global recorder installation is required.

**Press Ctrl-C when finished.** The application stops its producers, drains native
span collection and logs, and shuts down its host exporter. The
runner removes only its own containers and anonymous volumes. It refuses to
replace existing containers or use occupied ports.

Requirements: Rust/Cargo, Python 3, curl, and a running Podman daemon.
The main demo uses `jaeger:2.22.0`. Only the optional PostgreSQL mode additionally
requires OpenSSL and `postgres:15-alpine`. Images are downloaded if not cached.

## Other commands

Run the same demonstration, verify it, and immediately clean up:

```sh
make observability-demo-smoke
```

Show that logs and metrics do not depend on sampled traces:

```sh
TRACE_SAMPLING=never make observability-demo
```

In this mode, the same source/query/reaction work runs, metrics advance, and logs
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
| `HTTP_PORT` (main demos only) | `19090` |
| `PGPORT` (optional PostgreSQL only) | `15437` |
| `JAEGER_UI_PORT` | `16689` |
| `OTLP_HTTP_PORT` | `14318` |
| `TELEMETRY_PORT` | `19091` |

All listeners and trace export stay on loopback. Only the optional PostgreSQL
runner generates a temporary database password; no manual credentials or SQL
setup are needed.
Container names include their listener ports, and cleanup uses only owned
container IDs. A run still refuses existing containers or occupied active ports.

## What you will see

Each available event produces one connected trace through the selected source:

```text
source.publish
  pipe.send
  query.process (query)
    query.evaluate
    query.commit
    pipe.send
    component.confirm_outputs
    sink.handle (application reaction, completion=handled)
```

The verifier checks exact span counts, parent relationships, identities, numeric
counts, successful outcomes, and handled completion. Query output carries its
processing context directly to the application reaction, which remains connected
even after the query's processing span has finished. A default trace has eight
spans; Basic/Persistence omit Debug spans and have three. The latency scenario
adds a second sink and pipe, producing ten Debug spans.

This follows [computation_runtime](../computation_runtime.rs), statically hosting
the native SDK sources. There is no separately loaded demo inspector or synthetic
inspection work. The final application callback records actual handling and logs
safe result-change counts.

Query components use `query.process` for their complete input-processing
callback; `query.evaluate` remains evaluator-only. Generic transformers and
bounded service work retain `component.process`. This is a span-name change
only: component identities, causal parentage, timing boundaries and metric
names/labels are unchanged. Update query span-name filters or saved searches
that used `component.process`; older traces keep their original names.

Useful framework measurements:

| Rust instrument | Prometheus instrument | Meaning |
|---|---|---|
| `drasi.source.events_dispatched` | `drasi_source_events_dispatched` | Source envelopes accepted by downstream destinations |
| `drasi.query.events_processed` | `drasi_query_events_processed` | Completed query input envelopes |
| `drasi.query.results_emitted` | `drasi_query_results_emitted` | Newly emitted result-row changes |
| `drasi.component.inputs_processed` | `drasi_component_inputs_processed` | Successful component input processing, labeled by completion boundary |
| `drasi.query.engine.duration` | `drasi_query_engine_duration_{bucket,sum,count}` | Actual evaluation time in seconds, excluding commit and handling |
| `drasi.component.up` | `drasi_component_up` | Lifecycle/readiness gauge |
| `drasi.queue.depth` | `drasi_queue_depth` | Pending deliveries per physical queue |

The host owns the metric recorder and OTel batch processor. Metrics are not
sampled with traces and are applied outside the bounded native-source span queue.
The readable component logs show the query ID, total handled result changes,
and nonzero added/updated/deleted counts; the raw log API retains all fields.

Queries use memory storage. Bootstrap/recovery/replacement demonstrations and
performance qualification are not included. HTTP `traceparent` extraction and
source-specific PostgreSQL I/O/decoding instrumentation remain separate work.

For manual verification while the demo runs:

```sh
python3 lib/examples/observability/verify_trace.py http YOUR_HTTP_TRACE_ID
python3 lib/examples/observability/verify_telemetry.py
```

For the optional mode, select `postgres` and `--scenario postgres` in the trace
verifier instead. The telemetry verifier checks only the selected source and
rejects metrics/logs from an inactive pipeline.

Saved Jaeger v3 responses can also be verified with `--file path/to/trace.json`.
The underlying Rust binary remains available with
`cargo run --manifest-path lib/examples/observability/Cargo.toml` when Jaeger is
configured separately. A manual PostgreSQL run additionally needs
`DEMO_SCENARIO=postgres`, its database environment (including `PGPASSWORD`), and
a prepared table/publication.

`JAEGER_URL` controls the dashboard's links when Jaeger's
UI port is customized. The four active listener ports for the selected mode
must be distinct; an inactive source's port is not reserved.

To generate another HTTP trace while the demo is live:

```sh
curl --fail-with-body http://127.0.0.1:19090/sources/http/events \
  -H 'Content-Type: application/json' \
  --data '{"operation":"insert","element":{"type":"node","id":"http-2","labels":["trace_items"],"properties":{"id":2,"name":"Another demo","value":12}}}'
```

The dashboard refreshes every two seconds. The automatic verification is for the
initial seeded events; manually sending more events increases the metric counts.
