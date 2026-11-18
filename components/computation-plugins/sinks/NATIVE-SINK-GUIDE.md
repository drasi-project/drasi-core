# Native sink standard

This guide states what a production native sink (`NativeComponent` +
`NativeSink`, or a `DeliveryHandler` consumer) must do beyond compiling against
the native SDK (`drasi_computation_plugin_abi::ABI_VERSION`). It applies to every crate in this directory. The HTTP, gRPC, SSE
and log sinks are the reference implementations. The general rules in the
[native source standard](../sources/NATIVE-SOURCE-GUIDE.md) for management,
configuration, errors and documentation apply unchanged. This guide records
what is different for sinks.

## Lifecycle hooks

| Hook | Required behaviour |
|---|---|
| `initialize` | Compile every template and validator, check that credentials are present, and resolve routes against the bound outputs. Open no connections, bind no listeners, start no workers. |
| `start` | Observe `ctx.cancellation()` and `ctx.deadline()` across every connect, retry and readiness wait. A connection loop must not outlive the start budget. Prefer lazy connection: a destination that is down is a delivery failure, not a start failure, unless the configuration explicitly requires a connection at start. A cancelled start leaves nothing running and the object startable. |
| `pause` | **Host-driven sinks** (no worker of their own: HTTP, gRPC, log) may succeed and need no gate. The host calls no `handle` while paused and lets the in-flight one settle before Pause returns. **Sinks with workers** (the SSE server, a profiler reporter, driver keepalives) must gate them with the SDK's `ActivationScope` and `PauseGate` (see the source guide), or return `lifecycle::unsupported(...)`. Never return success while a worker keeps serving or reporting. |
| `resume` | Reopen exactly what Pause closed. |
| `stop` | `Drain`: let the in-flight delivery or connection finish within `ctx.deadline()`, then close. `Abort`: cancel promptly. On expiry, return a retryable error and keep ownership (`join_owned_worker_gracefully` for servers). Never report an earlier delivery failure, or a worker panic already reported through the SDK, as a Stop failure. |
| `dispose` | Implement both intents. Under `Erase`, remove exactly the destination-side state the sink owns (cursor rows, sender ledgers), idempotently. A sink that owns nothing declares `erases_owned_state` with a documented no-op. |
| `handle` / `handle_batch` | Return `Ok` only when the declared completion is met (see below). A failure must leave the sink able to retry the same input without Stop and Start; never latch an "interrupted" flag. A handler holds no state outside its future, because the host cancels an in-flight handle by dropping it. |

## Delivery profile

Each sink declares exactly one delivery profile, documents its duplicate and
loss windows in `DESIGN.md`, and tests it:

| Profile | Meaning |
|---|---|
| `Accepted` | Accepted into a local volatile buffer (SSE). Nothing is retained for subscribers that are not connected. |
| `Handled` | The destination completed the effect before `handle` returned. A crash before the host records completion can repeat the input, and a multi-change input can repeat its completed changes. |
| `Consumer/External` | The host `DeliveryRunner` records the input first and advances only the completed operation prefix. Only the operation in flight can repeat. |

Never report completion on a weaker signal than the profile states. For
example, an HTTP `202 Accepted` is not completion unless it is configured as
success.

## Delivery failures

Every sink uses the graph's one delivery-failure model. A sink never decides
to retry an input later, skip it or stall on it by itself; it classifies why
`handle` failed and the graph acts on that classification.

**Classify.** Carry a `DeliveryFailure` (from `drasi_computation_contracts`,
re-exported by the SDK) in the error `handle` returns. It crosses the plugin
ABI with the error.

| Class | Meaning | Examples |
|---|---|---|
| `DeliveryFailure::transient(e)` | The same input may be delivered later. | Connection refused, timeouts, HTTP 5xx/408/409/425/429, gRPC `UNAVAILABLE`/`RESOURCE_EXHAUSTED`/`ABORTED`, database deadlocks and lost connections. |
| `DeliveryFailure::transient_after(e, delay)` | As above, not before `delay`. | HTTP `Retry-After`, gRPC `grpc-retry-pushback-ms`. |
| `DeliveryFailure::permanent(e)` | This input can never be delivered. | Other HTTP 4xx, gRPC `INVALID_ARGUMENT` and other non-retryable codes, gRPC `success=false`, a template that fails to render (strictly), a change larger than the request limit, a procedure or cursor rejection. |
| `DeliveryFailure::skipped(e)` | This operation can never be delivered and the consumer, declaring `skip`, has already recorded the skip with its own progress, atomically. | Only consumers whose progress the host's `DeliveryRunner` owns (see below). |

An error without a `DeliveryFailure` is transient. Classify by status code,
`tonic::Code`, SQLSTATE or error kind, never by message text. A sink whose
activation cannot deliver again until it is restarted (for example a consumer
with an uncertain transaction) returns a terminal `LifecycleError` instead;
the graph then fails the node.

**Transient: retried in place.** The graph keeps the input, does not
acknowledge it, and delivers the same input again with bounded exponential
backoff and jitter (200 ms doubling to 30 s), never sooner than a retry-after
hint (honoured up to 10 minutes). It never skips the input and never retries
without a delay. Stop and Pause end the wait at once: the input stays retained
on the node and is delivered after Resume, or released unacknowledged on Stop.
While it retries, the node stays `Running`, its health is `Degraded`, and its
failure is the last error as a `Processing`/`Retryable` failure; both clear
when the input is delivered. Reconciliation does not restart a node that is
retrying in place.

**Permanent: the declared policy.** Every sink has the same setting,
`onPermanentFailure` (`ConfigOption::on_permanent_failure()` publishes it), and
declares the parsed value in its description
(`DescribeRequest::unkeyed_sink(policy)` /
`ComponentDescription::with_permanent_failure`). Because it is part of the
description the graph captured at construction, a change to it is a `Replace`.

| Value | Behaviour |
|---|---|
| `stall` (default) | The node records a terminal `Processing` failure and delivers nothing more until an operator acts (fix the destination or configuration, then Stop/Start or Retry, or change `onPermanentFailure`). |
| `skip` | The graph logs the input, acknowledges it, counts it in `ObservedComponent::skipped_inputs` and continues with the next input. |

When a multi-input batch fails permanently, the graph delivers those inputs
one at a time, so only the input that can never be delivered is stalled on or
skipped. A network sink that delivers each change of an input as its own
request uses `retry::InputOutcome`: under `skip` it still delivers the input's
other changes and then reports the input as permanently failed.

A consumer whose progress the host's `DeliveryRunner` owns (the
stored-procedure sinks, `http-delivery`) settles a skip itself, because the
graph cannot advance that progress: under `skip` it records the undeliverable
operation with its destination progress in one atomic step (the
stored-procedure sinks write a dead-letter row in the same transaction that
advances the destination cursor; `http-delivery` has only the runner's ledger)
and returns `DeliveryFailure::skipped`. The runner then records the operation
as completed, delivers the input's other operations, and reports the input as
permanently failed (`DeliveryError::Skipped`), so the graph acknowledges it
and counts the skip; a replay repeats nothing. The host enforces the
declaration: a settled skip from a consumer that declares `stall`, or an
unsettled permanent failure of an operation under `skip` (for example a
destination cursor rejection), fails the node instead. A transactional
consumer supports only `stall`.

If the runner's own storage fails a commit (its outcome then unknown), the
runner is fenced and the delivery fails transiently, so the graph keeps the
input and retries it. Before that retry the host reopens the runner from
storage, which reloads the committed ledger and resolves the uncertainty; the
retry resumes at the completed prefix once storage works again, in the same
activation. A delivery cancelled mid-batch still needs a restart.

**Retries inside a sink.** A sink may make bounded fast retries of one request
before it returns (`drasi_computation_network_common::retry`: exponential
backoff with full jitter, `Retry-After`, a total budget `retryBudgetMs` that
bounds how long a Drain stop waits). What remains after that budget is
returned classified, and the graph's backoff takes over.

**Idempotency.** Send a stable delivery identity with every remote request:
`x-drasi-delivery-id`, built by `config::delivery_id`. It is made from the
producer's logical sequence and change ordinal, so retries, in-place retries
and replays carry the same identity and a receiver can deduplicate. It
contains no transport or configuration identity. A template that fails to
render never falls back silently to another body.

## Operator output and diagnostics

Write diagnostics and operator-visible output with the `log` facade
(`drasi_computation_plugin_sdk::log`). Loaded as a library, the exported
plugin receives the host's logger through the ABI's logging extension, so the
records reach the host's logger with their level and target; statically
linked, the plugin shares the host's facade. Do not print to stdout or stderr
for diagnostics. A sink whose output *is* a standard stream (the log sink's
`stdout`/`stderr` outputs) writes from a dedicated thread so a blocked stream
never blocks the async runtime.

## Server-style sinks

A sink that serves clients (SSE today; dashboard and MCP when ported) uses the
shared listener `drasi_computation_network_common::server` with the listener
security settings:

- bind `127.0.0.1` by default;
- refuse a non-loopback address without a bearer token unless
  `allowUnauthenticated: true` is set;
- configurable CORS origins (none by default);
- a connection cap, a subscriber cap and a header-read timeout;
- optional TLS and mutual TLS;
- a real pause gate.

## Management

As for sources. Sink-specific classification:

- **Pause (in place):** anything read per delivery: templates, routes,
  headers or metadata, credentials sent per request, timeouts, retry policy,
  log level.
- **Stop (restart):** endpoint, listener address, TLS material, connection
  settings, batching bounds the host reads at start.
- **Replace:** output aliases, a change to the bound inputs
  (`update::inputs_changed`; live rebinding is CW-94), a delivery-profile
  change, `onPermanentFailure`.

## Tests

Every sink runs the shared conformance suite from
`drasi-computation-plugin-sdk` (feature `test-support`):

```rust
use drasi_computation_plugin_sdk::testing::{SinkConformance, SinkProbe};

struct Probe; // builds inputs and observes (or breaks) the destination
#[async_trait::async_trait]
impl SinkProbe for Probe {
    fn input(&self, sequence: u64) -> anyhow::Result<InputEnvelope> { /* ... */ }
    async fn expect_delivered(&self, configuration: &serde_json::Value, count: usize) -> anyhow::Result<()> { /* ... */ }
    async fn set_available(&self, available: bool) -> anyhow::Result<bool> { /* ... */ }
}

let suite = SinkConformance::new(&MyFactory, "<component uuid>", inputs)?;
suite.lifecycle(config.clone(), &Probe).await?;           // start, pause, Drain/Abort, restart, dispose
suite.retry_after_failure(config.clone(), &Probe).await?; // a failed input retries without a restart
suite.erase(config.clone()).await?;                       // erase twice succeeds
suite.dispose_matrix(config.clone()).await?;              // every DisposeReason, with Erase when declared
suite.dispose_after_failed_initialize(bad).await?;        // dispose ends a failed initialization
suite.configuration_round_trip(&[config.clone()])?;       // configuration() == constructed
suite.assess_matrix(&config, &cases)?;                    // every field's boundary
suite.apply(current, proposed, &Probe).await?;            // live in-place and restart updates
```

Run the suite on both Tokio flavours, linked and against the built dynamic
library through the Host SDK's `LoadedTarget` with
`SinkConformance::with_target` (see the source guide; a loaded repeated Start is
acknowledged rather than refused). Also test the destination protocol, the
security boundaries and the delivery profile's duplicate behaviour.

## Documentation

As for sources (`README.md`, `DESIGN.md`, `CONFIGURATION.md`). The sink
`DESIGN.md` also covers:

- input decoding and change projection;
- the delivery profile, with its exact duplicate and loss windows;
- batching;
- retry and delivery-failure classification (which failures are transient
  and which permanent);
- the idempotency key.

The sink `CONFIGURATION.md` also covers:

- the template reference (context keys, helpers, routing, render-failure
  policy);
- the receiver protocol;
- a mapping from the legacy reaction's configuration.
