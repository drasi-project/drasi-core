# Native sink standard

This guide states what a production native sink (`NativeComponent` +
`NativeSink`, or a `DeliveryHandler` consumer) must do beyond compiling against
the ABI 4 SDK. It applies to every crate in this directory. The HTTP, gRPC, SSE
and log sinks are the reference implementations. The general rules in the
[native source standard](../sources/NATIVE-SOURCE-GUIDE.md) for management,
configuration, errors and documentation apply unchanged. This guide records
what is different for sinks.

## Lifecycle hooks

| Hook | Required behaviour |
|---|---|
| `initialize` | Compile every template and validator, check that credentials are present, and resolve routes against the bound outputs. Open no connections, bind no listeners, start no workers. |
| `start` | Observe `ctx.cancellation()` and `ctx.deadline()` across every connect, retry and readiness wait. A connection loop must not outlive the start budget. Prefer lazy connection: a destination that is down is a delivery failure, not a start failure, unless the configuration explicitly requires a connection at start. A cancelled start leaves nothing running and the object startable. |
| `pause` | **Host-driven sinks** (no worker of their own: HTTP, gRPC, log) may succeed. The host calls no `handle` while paused and cancels or awaits the in-flight one. **Sinks with workers** (the SSE server, a profiler reporter, driver keepalives) must gate them, or return `lifecycle::unsupported(...)`. Never return success while a worker keeps serving or reporting. |
| `resume` | Reopen exactly what Pause closed. |
| `stop` | `Drain`: let the in-flight delivery or connection finish within `ctx.deadline()`, then close. `Abort`: cancel promptly. On expiry, return a retryable error and keep ownership (`join_owned_worker_gracefully` for servers). Never report an earlier delivery failure as a Stop failure. |
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

## Retries, idempotency and failure policy

- Use `drasi_computation_network_common::retry`: bounded exponential backoff
  with full jitter, a destination-requested delay (`Retry-After`), and a total
  time budget (`retryBudgetMs`). The budget bounds how long a Drain stop can
  wait for an in-flight delivery.
- Classify failures by status code, `tonic::Code` or error kind, never by
  message text. Do not retry permanent failures (authentication, invalid
  request, not found).
- Send a stable delivery identity with every remote request:
  `x-drasi-delivery-id`, built by `config::delivery_id`. It is made from the
  producer's logical sequence and change ordinal, so retries and replays carry
  the same identity and a receiver can deduplicate. It contains no transport
  or configuration identity.
- `failurePolicy: strict` fails the input, and the host retries it.
  `failurePolicy: skip` logs a sanitized record (`config::sanitize`) and
  completes the input. A template that fails to render is a per-change failure
  under that policy. It never falls back silently to another body.

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
  failure policy, log level.
- **Stop (restart):** endpoint, listener address, TLS material, connection
  settings, batching bounds the host reads at start.
- **Replace:** output aliases, a change to the bound inputs
  (`update::inputs_changed`; live rebinding is CW-94), a delivery-profile
  change.

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
suite.configuration_round_trip(&[config.clone()])?;       // configuration() == constructed
suite.assess_matrix(&config, &cases)?;                    // every field's boundary
suite.apply(current, proposed, &Probe).await?;            // live in-place and restart updates
```

Run the suite on both Tokio flavours. Also test the destination protocol, the
security boundaries and the delivery profile's duplicate behaviour, and run the
loaded dynamic library through the Host SDK.

## Documentation

As for sources (`README.md`, `DESIGN.md`, `CONFIGURATION.md`). The sink
`DESIGN.md` also covers:

- input decoding and change projection;
- the delivery profile, with its exact duplicate and loss windows;
- batching;
- retry and classification;
- the idempotency key.

The sink `CONFIGURATION.md` also covers:

- the template reference (context keys, helpers, routing, render-failure
  policy);
- the receiver protocol;
- a mapping from the legacy reaction's configuration.
