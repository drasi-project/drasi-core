# Native transformer standard

This guide states what a production native transformer (`NativeComponent` +
`NativeTransformer`, or a `TransactionalComponent`) must do beyond compiling
against the native SDK (`drasi_computation_plugin_abi::ABI_VERSION`). It
applies to every crate in this directory. A transformer that maintains state
is built on the SDK's [stateful transformer runtime](#stateful-transformers),
which implements durable output, durable bootstrap, recovery, continuations and
scheduling for it; the `query`, `stateful-middleware`, `view` and `geometry`
transformers run on it. The stateless `middleware` transformer is the reference for projected
current state, and `arithmetic` for a transaction participant. The general rules in the
[native source standard](../sources/NATIVE-SOURCE-GUIDE.md) for
configuration, errors and documentation apply unchanged, and the
[native bootstrap provider standard](../bootstrappers/NATIVE-BOOTSTRAP-PROVIDER-GUIDE.md)
describes the providers a transformer's bootstrap input binds to. This guide
records what is different for transformers. For the SDK contracts themselves,
see the [native SDK guide](../../computation-plugin-sdk/README.md).

A transformer is driven by the Host: it has no independent worker unless it
declares one (wakeups, continuations). The graph calls `transform` with one
input envelope at a time and forwards the returned envelopes through the
output pipes.

## Stateful transformers

A stateful transformer reads changes on `in`, maintains state, and publishes
the changes to that state on `out`. A Continuous Query is one such
transformer. `drasi_computation_plugin_sdk::transformer::StatefulTransformer`
is the native component for all of them. It owns:

- lifecycle and the atomic, non-atomic and volatile storage profiles;
- configuration identity, the producer epoch and committed input progress;
- bootstrap from a bound provider or from the connected input's current state,
  including a first input connected after the transformer started;
- restart replay of unconfirmed output, rebuild after a gap in the input's
  retained history, crash-safe installation, and refusal when retained state
  cannot be trusted;
- the materialized output: current rows, retained history, and the snapshot,
  snapshot-and-follow and replay readers;
- delivery confirmation and scheduled work.

An implementation supplies a `TransformerLogic`: how a batch of input changes
updates its private state and which output changes it produces.

| Hook | Supplies |
|---|---|
| `State`, `bind`, `open`, `reset` | Private state over the component's host stores, for example a query evaluator and its indexes; `()` when the output is the only state |
| `prepare`, `evaluate`, `project` | Decode a batch, apply it to the state, and project the output changes numbered by the runtime |
| `next_due`, `due`, `take_schedule_change` | Optional scheduled work |
| `decorate`, `verify`, `verify_owner` | Optional annotations on outputs and checks of stored outputs |
| `row_mutations`, `restore_row`, `difference` | Optional storage keys and rebuild difference; the defaults key graph-element rows by identity and merge patches |
| `readers`, `plan_update`, `apply_update` | Optional extra output readers and in-place updates |

Construct it from the factory with `StatefulTransformer::new(request,
descriptor, processing, logic, Settings { stream, capacity, dataset, identity,
source_transactions })`. `identity` is the logic configuration retained state
was built from; retained state is reused only with the same value. The factory
declares `transformer::capabilities(manages_updates)` (adding only its own
extras, such as a semantic kind), describes components with
`transformer::describe`, and publishes `transformer::configuration_options()`
for the shared options `bootstrap`, `volatile`, `publication`, `recovery`,
`resetConfiguration` and `outboxCapacity`.

The [query transformer](query) supplies query evaluation, its result
projection and its query read API. The
[stateful middleware](middleware/README.md#stateful-middleware) supplies only
its pipeline. The [view](view) supplies its complete-image pipeline and keeps
each current input and the outputs it produced in its host stores.
[Geometry](geometry) keeps its inputs, memberships and records there, and
rebuilds its in-memory members and pair index from them in `open`; it also
plans updates against its open state, through `plan_update`.

`transformer::HostState` gives any implementation the same durable stores the
query uses: `apply_inputs` applies a batch of graph changes to the current full
inputs, kept in the element index (a patch needs a retained full image), and
`apply_inputs_where` retains only the inputs a filter accepts. `get_value` /
`set_value` keep JSON values in named private tables, and `get_values` reads a
whole table, for example to rebuild in-memory structures in `open`. `bind` it
in `TransformerLogic::bind` so each runtime operation reads and writes the
operation's stores. With the `test-support` feature, `MemoryState` provides the
same stores in memory, with transactions, for unit tests.

`transformer::update_plan` applies the update rules every stateful transformer
shares: the shared options other than `outboxCapacity` cannot change, output
retention may only grow in place, and a reconstruction initializes from its
input again when `bootstrap` is configured. An implementation decides only
whether its own settings can change in place. A factory that accepts
reconstruction directives checks them with `transformer::validate_reconstruction`.

## Ports

- Declare every data port in `FactoryMetadata::ports` with its schema and
  `PipeRequirements`. Use `GraphChangeCodec::schema()` for graph-change data;
  a non-default format goes in `Capabilities::port_formats` and is selected
  through the optional `ports` configuration field.
- Never declare a bootstrap port. `FactoryMetadata::descriptor()` derives the
  `bootstrap` input from `quiescent_bootstrap`/`durable_bootstrap`, and the
  Host owns it.
- When a transformer has several inputs or outputs, name one primary of each
  direction in `Capabilities::primary_ports` if authoring shorthand should be
  able to name the component without a port.
- `transform` must refuse an envelope on an unknown port and a control
  envelope it does not implement (for example `FuturesDue` without
  scheduling), with an error rather than a silent drop.

## Declared descriptions and keyed inputs

`Factory::describe` is required and pure. It returns, from the configuration,
the descriptor and the declared capabilities of bound resources, what each
port of this instance guarantees. The graph compares descriptions on every
edge when it binds; it never infers them later.

| Transformer | Description |
|---|---|
| Volatile (new producer identity every construction) | `request.volatile_transformer()` (`middleware`, `arithmetic`). |
| Output durability follows a bound resource | `ComponentDescription::new(progress).with_outputs(descriptor, OutputDescription::following(durability))`, from `request.bound_durability("processing")` (`query`). |
| Output relays its input's progress | `OutputDescription::RELAY` with `Capabilities::output_progress = Relay` (the transformer default). |

Inputs are unkeyed unless declared. A transformer that deduplicates input by
producer identity (exactly-once over a durable producer, or at-least-once
with its own idempotency) declares that input's `InputDelivery` with
`with_inputs`, and the graph then refuses an edge from a producer whose
identity cannot satisfy it (`Incompatibility::ProducerIdentity`). Never key
processing on producer identity without declaring it: an undeclared keyed
input is refused. A transformer that commits its own input progress declares
`committed_input_progress` on that port.

Configuration that selects behaviour is declared in the configuration schema
the factory publishes, so the graph rejects an unsupported definition when it
binds the component. Use `ConfigOption::constraint` for nested shapes and
closed value sets. For example, the middleware, view and query transformers
publish their supported middleware kinds as an enum on each `middleware`
entry's `kind` (`ConfigurationConstraint::middleware_declarations`), rather
than discovering an unknown kind when the plugin constructs the component.
Construction still validates against the same declaration.

## Lifecycle hooks

| Hook | Required behaviour |
|---|---|
| `initialize` | Compile everything configuration-derived (pipelines, expressions, schemas). Start no workers and open no external connections. |
| `start` | Prepare a fresh processing epoch. A volatile transformer creates its new producer identity here (or at construction) and resets its sequence; a durable one reopens its committed state through `NativeProcessing`. Observe `ctx.cancellation()`/`ctx.deadline()` across every await. A transformer that latched a processing failure refuses to start until it is reconstructed, unless it rebuilds all of its volatile state here. |
| `pause` | A Host-driven transformer with no workers may return `nothing_to_do()`: the Host calls no `transform` while paused. A transformer with wakeups, continuations or its own workers must gate them or return `lifecycle::unsupported(...)`. |
| `resume` | Reopen exactly what Pause closed. Output reads and their follow leases stay open across a pause and resume. |
| `stop` | Stop admitting work, settle in-flight state and keep committed state. `Drain` finishes the in-flight operation within the deadline; `Abort` cancels promptly. Never report an earlier transform failure as a Stop failure. |
| `dispose` | Implement both intents. Under `Erase`, remove exactly the state the transformer owns (its processing and output-retention records) idempotently, and declare `erases_owned_state`. A transformer that owns nothing declares it with a documented no-op. |

A `transform` that fails part-way must not leave half-applied state visible.
Either stage the whole envelope and roll back on failure (the
[stateful transformer runtime](#stateful-transformers) rolls back the host
processing operation), or latch
the failure so that every later operation fails until the component is
reconstructed (`middleware`'s `ProcessingGuard`). A transformer holds no state
outside its future that a dropped (cancelled) `transform` could leave
inconsistent.

## Durable output

The stateful transformer runtime implements this section. A transformer
whose outputs must survive a restart without recomputation
declares `durable_output`, `output_replays` for those ports, and uses the
Host's output-retention service through `NativeProcessing::run_output`, which
commits business state, the retained output and its sequence together.

- **`delivery_completed(outputs)`** is the graph's all-branches-accepted
  callback for the batch `transform` (or a continuation) returned. Advance the
  durable delivery position there, validating that the confirmed sequence is
  the pending one; a repeated confirmation of an already delivered sequence is
  success. It does not mean downstream sinks finished their business work.
- **`resume_delivery`** is called when an unchanged binding resumes after
  quiescence. Mark any retained, unconfirmed output for replay; do not
  recompute it.
- **`has_pending_emissions` / `continue_transform`**: with `continuations`,
  the graph keeps calling `continue_transform` while `has_pending_emissions`
  is true, before admitting new input. Declare `startup_continuation` to get
  one call after startup, so recovered output replays without waiting for an
  input. Return an empty batch when there is nothing to replay.
- Refuse new input while output awaits confirmation. A failed delivery can
  replay the same logical output identity; one committed completion is not
  exactly-once external delivery.

A volatile transformer leaves these hooks at their defaults, except that
`delivery_completed` should check that the component is still running and has
not latched a failure.

## Bootstrap relay and status

A transformer can initialize itself from a provider bound to its `bootstrap`
input, and can serve its own current output as a snapshot to downstream
consumers.

- **Quiescent initialization:** declare `quiescent_bootstrap = Some(port)` and
  return `BootstrapInput::Quiescent`, a `NativeBootstrapInput` handler, from
  `NativeTransformer::bootstrap_input`; the input port comes from the
  declaration. Records arrive through `bootstrap(InputEnvelope)`;
  `bootstrap_completed` publishes the initialized state. The provider must be
  quiescent; the graph refuses any other provider when the edge binds. The Host
  blocks data operations and output reads until completion.
- **Durable initialization:** declare `durable_bootstrap = Some(port)` (it
  requires indexed processing and committed input progress on that port),
  return `BootstrapInput::Durable`, a `NativeDurableBootstrapInput` handler,
  from `bootstrap_input`, and stage watermarks and completion state atomically
  in `bootstrap_completed`.
- **Status:** `bootstrap_status` reports `configured` (the configuration asks
  for a provider), `complete` and `reset_allowed`. Declare
  `bootstrap_optional` when the configuration decides whether a provider is
  used; the Host checks the status against the bound edges before activation.
  A durable handler also implements the required `reset_bootstrap`, which the
  Host calls only while initializing and only when `reset_allowed` is true.
- **Relay to consumers:** list the output ports whose current state can be
  read as a snapshot in `output_snapshots` and register exactly those
  providers in `NativeComponent::output_readers`. A downstream query then
  initializes from the transformer's output instead of bypassing it. The
  snapshot is hidden until initialization completes. A transformer sends no
  labels in its own bootstrap requests: its snapshot is not bounded by a
  downstream query's labels.

## Transactions

- A transformer that must join the Host's transaction declares
  `capabilities.transactional`, returns `Component::Transactional`, has exactly
  one input and one output, and implements `transform_in_transaction`. All
  mutable business state goes through the `NativeTransactionContext`; the
  participant has no commit, no workers, no wakeups, no continuations and no
  external effects. The Host's `TransactionTransformer` owns isolation,
  recovery, the outbox and the commit.
- Source transactions (multi-change source commits) are delivered as ordinary
  envelopes carrying a `drasi.source-transaction.v1` context entry. A
  transformer that changes the change set must keep the input's context on
  its emission (`GraphChangeCodec::derive_changes` preserves the input's
  annotations) and must not split one source transaction across emissions
  unless it declares how it completes them (the query transformer's
  `sourceTransactions` setting).

## Management

- A transformer whose configuration can change without reconstruction
  returns an `UpdatePlanner` from `Factory::update_planner` (its `assess` is
  pure: configuration only), implements `NativeManagement`, and declares
  `management: true` with every supported `update_modes` entry.
- Classify each field by the weakest safe boundary: per-envelope values can
  change in place (Pause); values bound at start need a restart (Stop);
  anything that changes output identity, streams, ports or retained state needs
  replacement, with an explicit data treatment (`Preserve`, `Rebootstrap` or
  `Unsupported`).
- `apply` refuses, leaving the object unchanged, unless the request's current
  configuration equals the object's, the plan still matches and the proposed
  configuration validates. `configuration()` returns exactly what the object
  runs with, including after an update.
- A transformer without management is replaced on every configuration change;
  say so in its README.

## Tests

There is no transformer conformance kit yet; each crate covers:

- `describe`/`metadata` agreement, and that the published configuration schema
  rejects every unsupported value it declares closed (for example an
  undeclared middleware kind) while construction accepts the same set;
- duplicate or undeclared references in configuration;
- multi-change and fan-out envelopes, output sequencing and producer
  identity;
- failure atomicity: a failed `transform` leaves no partial state, and either
  the component recovers or it latches until reconstruction;
- the bootstrap profile it declares (quiescent or durable), including
  completion gating of output reads;
- for durable output: replay after restart without recomputation,
  `delivery_completed` idempotency and `resume_delivery`;
- the loaded dynamic library through the Host SDK
  (`host-sdk/tests/native_computation_pipeline.rs`, `native_view.rs`,
  `native_query_plugin/*`).

## Documentation

Each transformer crate has a `README.md` covering its implementation names,
configuration, declared capabilities, bootstrap profile, recovery model and
known limitations, with crate-level `//!` documentation and doc comments on
public items.
