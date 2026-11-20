# Native source standard

This guide states what a production native source (`NativeComponent` +
`NativeSource`) must do beyond compiling against the ABI 0.1 SDK. It applies to
every crate in this directory. The HTTP and gRPC sources are the reference
implementations. For the SDK contracts themselves, see the
[native SDK guide](../../computation-plugin-sdk/README.md).

## Lifecycle hooks

| Hook | Required behaviour |
|---|---|
| `initialize` | Validate instance prerequisites that need no external connection (secret presence, paths, route collisions). Start no workers and bind no listeners. |
| `start` | Observe `ctx.cancellation()` and `ctx.deadline()` across every await, including waits for progress readiness. Never use a configuration timeout as the overall start budget. A failed or cancelled start leaves nothing running, and the object can be started again after Stop. |
| `pause` | **Either** gate the source's independent work (no new admission or external reads, in-flight work settled, memory bounded, liveness keepalives maintained) **or** return `lifecycle::unsupported(...)`. Never return `nothing_to_do()` while workers keep producing or admitting. A source with no independent worker, whose output only the host pulls, may succeed, but must say so in its design document. A cancelled or expired Pause must leave the source running as before. |
| `resume` | Reopen exactly what Pause closed. Recover from upstream timeouts that fired while paused by re-establishing from the last committed position. |
| `stop` | `Drain`: finish the in-flight unit (request, frame, transaction) within `ctx.deadline()`, then close gracefully. `Abort`: cancel promptly. On expiry, return a retryable error and keep ownership of unfinished work, so a later Stop completes it. Report an earlier processing failure once, through `next()` or status, not as a Stop failure; a worker panic is already reported by the SDK when it happens (register workers with `workers::spawn_owned_worker` inside an operation), so a joined `WorkerCleanupError::Join` is not a Stop failure either. |
| `dispose` | Implement both intents. Under `Erase`, remove exactly the state the component owns (replication slots, journal tables, producer registrations), never borrowed or shared state. Make it idempotent: "already gone" is success, and a retry repeats safely. Declare `erases_owned_state` whenever removal with erase should succeed. A component that owns nothing declares it with a documented no-op. |

Own independent workers with the SDK's `ActivationScope` (re-exported from
`drasi_computation_plugin_sdk::lifecycle`), created in `start`:

- Every worker that reads, admits or emits on its own takes a `PauseGate` from
  `scope.gate()` and calls `gate.wait_running().await` at points where it holds
  no open unit of work. `scope.pause(ctx)` returns only once every gate has
  acknowledged; on expiry or cancellation the request is withdrawn and the
  workers keep running.
- An admission path that is not a worker (a request handler) observes
  `scope.pause_requests()` and refuses new work while paused.
- `scope.stop(ctx)` cancels the token, closes the gates and joins the workers
  within the deadline. Unjoined workers stay owned, and the retryable error lets
  a later Stop finish the job.

A source whose only work is network admission refuses new input while paused
(for example, HTTP `503`, bridge `Busy`); it needs no worker gate.

## Declared services

- Declare every Host service once, in `Capabilities::services`
  (`source_progress`, `consumer_progress`, `source_admission`,
  `bootstrap_coordination`, `activation_credentials`). `Factory::create`
  receives every bound service together in `ServiceBindings`; there are no
  per-service create hooks.
- Return the factory-level implementations exactly when declared:
  `Factory::activation_credentials` and `Factory::bootstrap_coordination`. The
  library is refused at load otherwise.
- Only contradictory declarations are refused: single-owner with
  consumer-group progress, admission with a progress owner, coordination
  without exactly one progress mode.

## Management

- Implement `UpdatePlanner::assess` and return it from `Factory::update_planner`. The
  assessment must be pure: configuration only, with no I/O, no clock and no
  external state.
- Implement `NativeManagement` and declare `management: true` with every
  supported `update_modes` entry.
- Classify every configuration field by the weakest safe boundary, and take the
  strongest boundary among the changed fields. `drasi_computation_network_common::update::BoundaryPlanner`
  does this for the network sources.
  - **Pause (in place):** values read per request or per poll.
  - **Stop (restart):** values bound when the component starts.
  - **Replace:** anything that changes identity, output streams or recovery
    bindings. Choose the data treatment explicitly: `Preserve`, `Rebootstrap`
    or `Unsupported`.
- `apply` must check three things before changing anything, and return
  `Refused` (leaving the object unchanged) when any fails:
  - the request's current configuration equals the object's;
  - the plan still matches;
  - the proposed configuration validates.
- An in-place apply requires a completed Pause; a restart apply requires a
  stopped component.
- `configuration()` must return exactly the configuration the object runs with,
  including after an update.

## Configuration

- Keys are lowerCamelCase at every level (`intervalMs`, `tlsCaPem`); database
  plugins name their connection `host`, `port`, `database`, `user`, `password`.
  No snake_case aliases. The conformance kit's `configuration_round_trip`
  checks the published keys (`testing::ensure_camel_case_configuration`).
- `#[serde(deny_unknown_fields)]` on every configuration struct, including
  nested ones. Generate the published schema from the same types or option
  list the parser uses; a provider that shares a source's settings publishes
  the source's options (from the shared `*-common` `definition` module), never
  a hand-maintained copy or a name-to-type match.
- Publish `configuration_details` (required): describe every schema field;
  `PluginDefinition::new` refuses an undescribed field. Every published default
  must equal the serde default. A computed default uses `default_description` instead of a literal.
- Mark secrets with `ConfigOption::secret()`. The host resolves references only
  for **top-level** fields, so keep each secret top-level. Never mark a whole
  nested object secret.
- `Debug` must never print a secret. Use a redacting wrapper such as
  `config::SecretString`.
- Listeners bind loopback by default. A non-loopback listener that admits data
  must require authentication, or an explicit `allowUnauthenticated` opt-in.

## Errors, retries and observability

- Return typed, classified failures (`IngressErrorKind`, `LifecycleError`
  kinds), not bare strings. Keep causes.
- Retry transient upstream failures with bounded backoff, aware of cancellation
  and Stop. Never silently fall back to a different position.
- Log through the `log` facade, with the stderr fallback for a dynamically
  loaded library (see `drasi_computation_network_common::diag!`). Log
  per-request rejections at debug level.
- Never use `unwrap` or `expect` on a path reachable from FFI, except on
  constants that a unit test proves valid.

## Tests

Every source runs the shared conformance suite from
`drasi-computation-plugin-sdk` (feature `test-support`):

```rust
use drasi_computation_plugin_sdk::testing::{Offer, SourceConformance, SourceProbe};

struct Probe; // offers one event through the source's external interface
#[async_trait::async_trait]
impl SourceProbe for Probe {
    async fn offer(&self, configuration: &serde_json::Value) -> anyhow::Result<Offer> { /* ... */ }
}

let suite = SourceConformance::new(&MyFactory, "<component uuid>")?;
suite.lifecycle(config.clone(), &Probe).await?;      // start, restart, pause gating, Drain/Abort, dispose
suite.cancelled_start(config.clone(), &Probe).await?; // a pre-cancelled start leaves nothing running
suite.erase(config.clone()).await?;                   // erase twice succeeds
suite.dispose_matrix(config.clone()).await?;          // every DisposeReason, with Erase when declared
suite.dispose_after_failed_initialize(bad).await?;    // dispose ends a failed initialization
suite.configuration_round_trip(&[config.clone()])?;   // configuration() == constructed; defaults agree
suite.assess_matrix(&config, &cases)?;                // every field's boundary
suite.apply(current, proposed, &Probe).await?;        // in-place and restart updates on a live source
```

Run the suite on both Tokio flavours. Run the same scenarios against the built
dynamic library through the Host SDK (feature `test-support`):

```rust
use drasi_host_sdk::computation::conformance::LoadedTarget;

let target = LoadedTarget::load("target/debug/plugins/libmy_source.dylib", "acme.example/my-source")?;
let suite = SourceConformance::with_target(target, "<component uuid>")?;
suite.lifecycle(config.clone(), &Probe).await?;
```

Loaded runs differ from linked runs in one respect: the native ABI
acknowledges a repeated, already completed transition (a second Start) instead
of refusing it, so a Host retry after a lost reply is safe. The kit checks that
the component still works afterwards. `apply` needs a linked factory.

Also cover the plugin's real external system (containers), its security
boundaries, and a crash or restart matrix wherever recovery is claimed.

## Documentation

Each source crate contains:

- `README.md`: a short overview linking to the two documents below.
- `DESIGN.md`: purpose and scope; architecture; capture or admission mechanism;
  data mapping (labels, element IDs, type mapping); ordering, checkpoint and
  recovery model; owned versus borrowed state; per-hook lifecycle behaviour;
  management classification; security model; failure model; observability;
  known limitations; differences from the legacy source.
- `CONFIGURATION.md`: every field (type, default, range, required, secret,
  update boundary); examples; request or wire formats; external
  prerequisites (privileges, server settings, versions); credentials; update
  rules; removal and erase behaviour; an operations runbook and
  troubleshooting.
- Crate-level `//!` documentation and doc comments on public items and
  configuration fields.
