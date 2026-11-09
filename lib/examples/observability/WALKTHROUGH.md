# Observability demo: follow one HTTP change

A presenter walkthrough for `make observability-demo`. Allow approximately
8-10 minutes. No other scenario or policy change is needed.

## Main message

> We can follow a real data change from source through continuous-query processing
> to actual reaction handling, and measure what happened at each stage.

This walkthrough demonstrates connected tracing, input/result/handled-output
counts, and correlated component logs. There is no demo inspector or
synthetic inspection work.

## Before presenting

1. Make sure Podman is running.
2. Stop a previous demo runner with Ctrl-C before reusing its ports.
3. From the repository root, run:

   ```sh
   make observability-demo
   ```

4. The runner prints dashboard links as soon as its listener is ready. Automatic
   checks continue afterward; wait for **"Demo is live"** before sending events.
5. Open:
   - Dashboard: <http://127.0.0.1:19091/>
   - Jaeger: <http://127.0.0.1:16689/>
6. Confirm the dashboard shows **Follow one HTTP change**, profile **debug**,
   tracing **on**, sampling **always**, metrics **on**, and logs **info**.
   Defaults can be overridden by telemetry environment variables.
7. The displayed and exported service name is **`drasi-observability-demo`**.

The runner automatically sends an HTTP seed event, verifies its trace and
telemetry, exercises insert/update/delete, and resets the experiment baseline.
There will already be traces, logs, and cumulative metrics before presenting.
These are startup/control operations and live self-check events, not bootstrap
snapshots. Reset does not erase historical traces.

The dashboard refreshes every two seconds. Allow a refresh before interpreting
counts, and allow time for a completed trace to reach Jaeger.

## 1. Explain the three components

**Show:** **Connected pipeline**.

```text
HTTP source -> Continuous query -> Application reaction
   http          http-query           http-output
```

| Component | Responsibility |
|---|---|
| HTTP source | Receives real insert/update/delete events from the dashboard. |
| Continuous query | Evaluates the change and produces result-row differences. |
| Application reaction | Handles results through an in-process callback and reports completion. |

All three belong to one `DrasiLib` instance, `source-trace-demo`, and one
ComputationGraph. There is no PostgreSQL source or database in this walkthrough.
The query uses in-memory storage:

```cypher
MATCH (n:trace_items)
RETURN n.id AS id, n.name AS name, n.value AS value
```

The source uses the existing native SDK and is statically registered in the host.
There is no separately loaded demonstration plugin.

The reaction records handling, emits a structured log of result-change counts,
and returns success. It does not send a webhook or make an external business
effect durable.

**Say:**

> This is the normal source-query-reaction pipeline. We are observing real input,
> real evaluation, and actual completion of the application callback.

## 2. Send one change

**Do:**

1. Click **Send insert**.
2. Wait for one handled envelope and row in the expected/observed table.
3. Open the newest corresponding trace in Jaeger.

For this simple insert, the measurements since reset should increase by:

- One query input envelope.
- One newly emitted result-row change.
- One successfully handled output envelope and row.

Do not expect raw cumulative counters to start at zero; they include self-checks.

## 3. Follow the connected trace

With default Debug settings, a fresh insert produces the verified eight-span
path:

```text
source.publish                    (http)
  pipe.send
  component.process               (http-query)
    query.evaluate
    query.commit
    pipe.send
    component.confirm_outputs
    sink.handle                   (http-output; completion=handled)
```

**Point out:**

- `source.publish`: framework publication of the available source envelope.
- `component.process`: the query's complete transformation callback.
- `query.evaluate`: actual evaluation, not the whole transformation.
- `query.commit`: the query's commit operation.
- `pipe.send`: downstream acceptance, distinct from handling.
- `component.confirm_outputs`: the query's output confirmation.
- `sink.handle`: the actual application reaction.
- Component identities, parentage, successful outcomes, and durations.

Click span details to compare `component_id=http-query` and
`component_id=http-output`. There is only one `component.process` in this path.

**Say:**

> We have one causal trace for this change. We can identify the component doing
> the work and distinguish evaluation, commit, forwarding, and actual handling.

The reaction inherits query-output context. Its nesting shows causality, not
that later reaction handling contributes to the query's processing duration.
Similarly, evaluation/commit children do not currently explain every interval
inside `component.process`; do not call the residual time CPU time.

The trace starts at framework source publication, not upstream HTTP/network work.
Incoming HTTP `traceparent` extraction is not demonstrated.

## 4. Show framework metrics

Keep this section to **three metrics**. Use the expected/observed table and
**Live metrics**; there is no need to enable all component and queue metrics.

| What to show | What to say |
|---|---|
| **Query inputs processed** (`drasi_query_events_processed`) | "How many input envelopes has the query processed?" |
| **Result-row changes emitted** (`drasi_query_results_emitted`) | "How many result changes did those inputs produce? One input can produce zero, one, or many." |
| **Output envelopes handled** (`drasi_component_inputs_processed`, filtered to `http-output` and `completion="handled"`) | "How many output envelopes did the application reaction successfully handle, rather than merely accept?" |

For the insert from step 2, point out **one additional input and one additional
result-row change**, followed by **one additional handled output envelope**.
Find the reaction counter in the raw metrics using:

```promql
drasi_component_inputs_processed{component_id="http-output",completion="handled"}
```

The output metric is `drasi_query_results_emitted`, not
`drasi_query_events_emitted`. Query outputs count result-row changes; the reaction
counter counts handled envelopes, so their totals need not match for other queries.
Raw counters include startup self-checks, so compare increases rather than
expecting their totals to be one.

**Say:**

> Traces explain one change. Metrics summarize how much work the query processed,
> how many result changes it produced, and whether the reaction handled its outputs.

## 5. Connect handling logs to traces

**Show:** **Correlated component logs**.

The application reaction logs real handling with readable fields, for example:

```text
Query results handled
Query: http-query · 1 result change handled · 1 updated
[Open trace]
```

These counts come from actual result-change operations; no operation is inferred
from serialized bytes or image counts. Zero operation counts are omitted from
the readable display but retained in the raw log API.

Click **Open trace** to open the corresponding Jaeger trace. Expand
**Trace context** to compare full trace/span IDs. Times are local; hover over a
time for the original UTC timestamp.
Logs also include component diagnostics from source/query scopes when available.
Unsampled contexts have no export link, and missing contexts are identified.

**Say:**

> This message tells us which query results the reaction handled. Its execution
> context connects the message to the exact handling operation in the trace.

Briefly show collection/log-loss counters. An empty native-source span buffer
means nothing is waiting there; zero loss counters mean no reported losses.
This is collection visibility, not an exporter-outage or saturation test.

## 6. Repeat with update and delete

1. Click **Update latest** and observe the new handled result, trace, and log.
2. Click **Delete latest** and observe the deletion's result, trace, and log.
3. Show the advancing input, result, and handling counters.
4. Click **Reset experiment**.

**Say:**

> This query runs continuously. New changes produce new result differences and
> new telemetry; we are not rerunning a one-shot query snapshot.

Reset removes dashboard-owned nodes and establishes a new comparison baseline.
It does not erase traces, log history, or cumulative metrics.
Result-row changes depend on the query; one input does not universally mean one
result-row change.

## Closing and scope

> We followed real input from source to actual handling, kept its trace connected,
> and collected correctly attributed framework metrics and correlated logs.

- Handled-branch end-to-end latency remains off in this walkthrough.
- Debug includes `query.commit` as a span, but the commit-duration **metric**
  belongs to the Persistence profile and is not enabled here.
- Sampling independence is supported, but this Always-sampled run does not
  demonstrate the Never-sampled comparison.
- This walkthrough does not demonstrate independently loaded plugin-internal
  telemetry, bootstrap, persistent recovery, replacement, multi-instance
  isolation, deliberate backpressure, or exactly-once external effects.

Press Ctrl-C when finished; the runner stops the application and removes its own
container. See the [demo README](README.md) for setup, optional scenarios, port
overrides, and verification commands.
