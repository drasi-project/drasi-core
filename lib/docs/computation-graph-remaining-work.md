# ComputationGraph remaining work

This is the list of open work for ComputationGraph in **drasi-core**, and for the
parts of **learning-drasi-server** and **test-infra** that exercise it.
Implementation and qualification entries describe open work, not completed
requirements. The separate [existing-code review](#code-review-findings) also
records retained compromises and questions that need a decision rather than
necessarily a code change.
Drasi Server work is tracked in the
[Drasi Server remaining work](../../../drasi-server/docs/remaining-work.md)
document (SW entries) and is linked from here only where a Core entry depends
on it.

**How to read it:**

- An **issue** is a confirmed gap or defect in implemented behavior.
- An **investigation** needs a reproduction before a fix can be chosen.
- **Qualification** means the behavior exists but lacks test evidence.
- An **improvement** adds capability, efficiency or maintainability.
- **USER-DEFERRED** entries are parked by an owner decision and should not
  be started without revisiting that decision.

Priority describes the impact on the affected deployment or release claim; it
does not mean every High item blocks every merge. Areas name the owning
repositories, not people. Statements marked **Unverified** have not been
confirmed against the code or by running tests.

Implementation work comes first. Test quality, coverage, CI selection,
reliability and measured qualification are grouped under
[Testing and qualification](#testing-and-qualification); implementation entries
link to their testing obligations there rather than repeating them. The
[testing guide](../../docs/contributing/contributing-code/contributing-code-tests/)
describes ordinary Cargo and CI execution.

The ABI 0.2 design is an unreleased breaking change. Converting every plugin to
the native SDK and retiring old adapters are not merge prerequisites, and no
compatibility shims or automatic migration of old state are planned.

| Work area | Entries |
|---|---|
| Managed updates and lifecycle | [Rebootstrap coordination](#cw-92), [named endpoints](#cw-93), [graph changes and live rebinding](#cw-94) (deferred), [plugin upgrade and configuration migration](#cw-95) (deferred), [late progress binding startup hold](#cw-104), [cleanup deadline for large graphs](#cw-107), [declared component descriptions](#cw-106), [durable query-edge resume](#cw-109), [retrying sinks at start and shutdown](#cw-115), [constructed bootstrap providers in batches](#cw-116), [input label selection validation](#cw-114) |
| Connectors and runtime boundaries | [Data connections](#cw-44), [old plugin support](#cw-63), [ordinary sources without replay](#cw-117), [ordinary PostgreSQL source](#cw-112), [query names in reaction results](#cw-113), [linked query entry points](#cw-80), [oversized query records](#cw-76), [element JSON in results](#cw-108), [bootstrap through transformers](#cw-110) |
| Native plugins: shared work | [Pause and stop](#cw-96), [erasing disposal](#cw-98), [conformance kit](#source-x-02), [configuration and secrets](#source-x-03), [update planning](#source-x-04), [retry policy](#source-x-05), [delivery contract](#sink-x-03), [observability](#source-x-07), [starter project](#dx-1), [packaging](#dx-10), [provider and transformer review](#cw-103) |
| Native sources | [Network runtime](#plugin-source-network), [PostgreSQL](#plugin-source-postgres), [MySQL](#plugin-source-mysql), [SQL Server](#plugin-source-mssql), [SQLite](#plugin-source-sqlite), [HTTP](#plugin-source-http), [gRPC](#plugin-source-grpc), [bridge](#plugin-source-bridge), [counter and mock](#plugin-source-counter-mock) |
| Native sinks | [Network runtime](#plugin-sink-network-runtime), [HTTP](#plugin-sink-http), [gRPC](#plugin-sink-grpc), [SSE](#plugin-sink-sse), [log](#plugin-sink-log), [capture](#plugin-sink-capture), [profiler](#plugin-sink-profiler), [stored procedures](#plugin-sink-storedproc-shared), [PostgreSQL](#plugin-sink-storedproc-postgres), [MySQL](#plugin-sink-storedproc-mysql), [SQL Server](#plugin-sink-storedproc-mssql), [bridge](#plugin-sink-bridge), [legacy reactions](#plugin-legacy-reactions) |
| Data, errors and diagnostics | [Saved-data formats](#cw-18), [deep data](#cw-19), [error messages](#cw-22), [status and log updates](#cw-23), [background work](#cw-17), [logging control](#cw-24), [request tracing](#cw-25), [measurements](#cw-26) |
| Scaling and maintainability | [Removed-component bookkeeping](#cw-33), [memory limits](#cw-29), [disk work](#cw-28), [processing repetition](#cw-32), [management code](#cw-34) |
| Public API, dependencies and release | [API curation](#cw-69), [legacy dependencies](#cw-66), [capabilities and delegation](#cw-72), [release documentation](#cw-73) |
| Optional capabilities | [Store housekeeping](#cw-71), [pipe policies](#cw-74), [isolated hosts](#cw-75), [mixed scheduling proof](#cw-82) |
| Test quality and CI | [Credential-provider tests](#cw-83), [CI gates](#cw-85), [repeated tests](#cw-86), [asynchronous tests](#cw-37), [build and fixture cost](#cw-87) |
| Behavior coverage | [Backend conformance](#cw-89), [loaded query plugin](#cw-77), [status and results](#cw-10), [query matching](#cw-13), [grouped results](#cw-12) |
| Recovery and failure | [Native input recovery](#cw-14), [chained bootstrap](#cw-111), [completion proof](#cw-36), [interrupted reset](#cw-20), [safe stop](#cw-05), [overlapping changes](#cw-07), [cleanup](#cw-08), [combined failures](#cw-39), [extension failures](#cw-03), [retry questions](#cw-21), [ownership and limits](#cw-35) |
| Native boundaries, platforms and performance | [Real plugin libraries](#cw-41), [Protocol Buffers compiler](#cw-67), [isolated host profiles](#cw-91), [complete suites](#cw-78), [large applications](#cw-40), [native query overhead](#cw-79) |
| Existing-code review | [Core and value types](#code-review-core), [Lib contracts and telemetry](#code-review-lib), [plugins and shared crates](#code-review-plugins), [tradeoffs and workarounds](#code-review-tradeoffs), [tests and comments](#code-review-tests), [unimplemented paths](#code-review-paths) |

Native plugin work is grouped by plugin under [Native sources](#native-sources)
and [Native sinks](#native-sinks), after the shared plugin work. Plugin entries
use per-plugin IDs (for example PG-13 for the PostgreSQL source or SPG-01 for the
PostgreSQL stored-procedure sink) because plugin design documents and code cite
them. Shared and decision IDs are qualified as Source or Sink (for example
Source X-03, Sink D-3) because the two families number them separately.

Keep IDs and anchors stable. Add new work here, as a new CW entry or a new
plugin ID in the plugin's section, rather than in a separate backlog or
checklist. Review-derived items keep their report IDs with a `CR-` prefix in
the [existing-code review](#code-review-findings). Related-work links connect
them to existing entries without replacing those entries or their IDs.

## Implementation and integration

<a id="cw-92"></a>

### Coordinate safe rebootstrap during managed updates (CW-92)

**Related review finding:** [CR-A02](#cr-a02), the PostgreSQL TRUNCATE reset
gap; explicit source rejection is not downstream resynchronization.

**Kind:** Unsupported capability. **Area:** Core, native SDK/Host, query plugin, Server.
**Priority:** Needed for general native graph recovery; ordinary-query
replacement already works without it. **Effort:** Large.

Some configuration changes make a component's retained state wrong, so the
component must discard it and rebuild from a snapshot (`Rebootstrap`) or restart
from the live stream (`Reset`). Today only an ordinary query can do this on its
own; there is no graph-wide protocol that makes a reset safe when other
components depend on the reset component's state or input.

**What works today:** The ordinary native-query factory plans a semantic
change with unchanged inputs as `Recreate` + `Rebootstrap` (if the query has
bootstrap inputs) or `Recreate` + `Reset` (streaming-only), and an
output-retention increase as `InPlace` + `Preserve`. It rejects changes to
inputs, output stream, storage, bootstrap or recovery policy. The managed planner
also rejects a data reset that changes the query's live inputs. Every other native
factory rejects an update directive unless it declares `Recreate` update support,
and none except the query plans `Rebootstrap`/`Reset`.

**Example of the gap:** A query `orders-by-region` changes its `WHERE` clause but
keeps the same output columns. A second query and an SSE sink consume its output.
The query can rebuild itself, but nothing ensures the downstream query
resynchronizes, that changes arriving during the snapshot are neither lost nor
applied twice, or that a crash midway resumes the same reset rather than running
it again.

**What remains:** A graph-owned reset protocol, built on the existing
configuration owner, lifecycle operations, bootstrap/read services and
recovery-domain ownership, that before discarding state:

1. Identifies the actual snapshot frontier and keeps a path for changes arriving
   after it. An available provider or an empty bootstrap result is not proof.
2. Decides which queued work the snapshot supersedes, retains or replays the
   rest, never silently drops unconfirmed output, and persists the operation
   identity and boundaries so retries and restarts neither reset twice nor
   resume an unconfirmed operation.
3. Coordinates affected downstream queries and stateful consumers, confirms they
   resynchronized, and leaves shared history and other consumers' obligations
   intact.

Do not treat a factory's plan, the presence of a factory, or
`reset_configuration: true` as these guarantees, and do not report a semantic
change as `Preserve`.

**Evidence:** `drasi-core/components/computation-plugins/transformers/query/src/update.rs:39-118`
(query plan), `drasi-core/lib/src/management/update_planning.rs:9-15`
(data reset requires `Recreate`) and `:515-527` (query reset cannot change
live inputs), `drasi-core/components/computation-plugin-sdk/src/export.rs:979-989`
(directive rejected without declared `Recreate` support),
`drasi-core/components/host-sdk/src/computation/query.rs:310-365` (memory and
persistent reset handling).

**Done when:** Real native query pipelines change output shape, and change
semantics with unchanged columns. Independent result oracles verify the
snapshot/live boundary, retained post-frontier changes, downstream query and
stateful-consumer resynchronization, and that unaffected consumers are
untouched. Cancellation, cleanup failure and process exit before and after reset,
bootstrap and completion either resume the same operation safely or leave it
visibly blocked without losing ownership or data. Unsupported
provider/consumer combinations are rejected before any destructive work.

<a id="cw-93"></a>

### Persist and resolve native named endpoints once (CW-93)

**Kind:** Unsupported authoring capability. **Area:** Core, Server, test-infra.
**Priority:** Medium. **Effort:** Medium.

Native graph edges name their endpoints by component UUID, and a UUID exists
only after its component is declared. A native consumer therefore cannot be
declared in one request with an edge to a producer, identified by name, that
will be declared in a later request.

**Example:** Request 1 declares sink `alerts-sse` with an input edge from
`orders-query`. Request 2 later declares `orders-query`. Today request 1 must
either omit the edge (declare the sink disconnected and wire it after request 2
returns the query's UUID) or be submitted after request 2.

**What works today:** Native additions can declare UUID-addressed
`relationships` to previously accepted components; `boundary_relationships`
stores edges to ordinary queries. Required links bind once both endpoints exist.
Ordinary declarations' pending name references do not apply to native endpoints;
an unresolved name is not a native endpoint.

**What remains:** A native named-endpoint declaration, managed by the
configuration owner, that waits for a later declaration with that name and binds
exactly once to its assigned UUID. The unresolved intent and the eventual
binding survive acceptance, request retry and restart. Renaming the producer or
reusing its old name never retargets a resolved edge. Role, port and schema are
validated before the connection is published. Do not derive UUIDs from names,
create placeholder components, use external IDs as a workaround, or add a second
mutable graph registry. Until then, native helpers that need named producers
must require the producer first or offer explicit two-step wiring.

**Evidence:** `drasi-core/lib/src/computation/v1/graph/topology.rs:90-118`
(`DesiredRelationship` keyed by `EdgeDefinition`; `boundary_relationships`),
[configuration guide](computation-graph-configuration.md#configuration-ownership-and-definition-batches).

**Done when:** Public-API tests cover consumer-before-producer in separate
requests, missing and wrong ports, duplicate names, rename and name reuse,
request retry and restart, and prove that a resolved binding never follows a
different component.

<a id="cw-94"></a>

### Review managed graph changes and live input rebinding (CW-94)

**Kind:** Design review / disabled functional coverage.
**Area:** Core, native SDK/Host, SSE sink.
**Status:** USER-DEFERRED until the other build and test failures are resolved.
No implementation plan is adopted.

A managed update cannot change which upstream components feed a running
component that binds its own inputs. Re-pointing a live SSE sink to a different
query requires removing and re-adding it, and the tests that would cover live
rebinding are disabled.

**Example:** SSE sink `alerts-sse` reads from `orders-query`. A managed update
that changes its input to `orders-query-v2` is rejected at acceptance with
"live input-binding changes require current/proposed graph bindings; the
specification-only update planner cannot establish them", even though the SSE
factory itself would assess the change as `Replace` + `Preserve`.

**Current state:** DrasiLib's configuration owner handles all managed changes;
raw graph reconciliation cannot bypass it. The planner rejects any input change
for a created component whose factory binds inputs. The SSE factory supports
update planning and assesses an input change as `Replace` + `Preserve`, but that
assessment is never reached through a managed update. Construction-time input
binding and a preview's replacement decision do not count as permission to
update a managed component.

**Disabled tests:** In `drasi-core/components/host-sdk/tests/native_network/sse.rs:915-931`
(test target `native_network_loopback`), both tests carry
`#[ignore = "CW-94: deferred pending graph-change design review"]`:

- `native_sse_declared_inputs_follow_a_live_rebind_on_current_thread`
- `native_sse_declared_inputs_follow_a_live_rebind_on_multi_thread`

Their bodies and assertions are kept intact as a reference, covering both
statically linked and dynamically loaded plugins. They provide no passing
coverage of live rebinding.

**What remains:** Once the other build and test failures are resolved, agree the
desired managed graph-change design with the user, covering managed acceptance,
actual binding and factory assessment, affected generations, recovery and
cleanup together. Do not make the tests pass with a temporary graph, an owner
bypass, a forced replacement or an "unsupported" error oracle.

**Evidence:** `drasi-core/lib/src/management/update_planning.rs:551-565`
(rejection), `drasi-core/components/computation-plugins/sinks/sse/src/config.rs:428-444`
(SSE assessment), `drasi-core/components/computation-plugin-sdk/src/update_planning.rs:61-66`
(`inputs_changed`).

**Done when:** The design is agreed, and re-enabled tests cover affected and
unaffected components and live streams, on both Tokio flavors and both plugin
loading paths. Until then, report this coverage as deferred.

<a id="cw-95"></a>

### Upgrade native plugins in place and migrate their configuration (CW-95)

**Kind:** Unsupported capability / design decision.
**Area:** Core, native SDK/Host, Server. **Priority:** Medium. **Effort:** Large.
**Status:** USER-DEFERRED. Exact version matching stays until this is picked up.

A component is bound to the exact plugin build that created it, so a new plugin
release cannot take over existing components. Moving to the new build means
removing and re-adding each component, which discards its state and disrupts
downstream components.

**Example:** A graph contains an SSE sink created by plugin
`drasi-computation-sink-sse@0.4.0`. After installing `@0.4.1`, the stored
component still names `0.4.0`; restoring it fails unless `0.4.0` is also loaded.
If `0.4.1` also raises the configuration version from 1 to 2, stored v1
configuration is rejected because there is no migration.

**Current state:** `ImplementationIdentity` (name, version, plugin ID and package
version) is matched exactly everywhere: factory lookup, plugin registration
(`id@version`), provenance checks and managed updates. A factory declares one
`configuration_version`, and stored recipes and Server configuration files must
match it exactly. Native libraries are never unloaded, so two builds of one
plugin can be loaded side by side. ABI safety does not depend on the package
version; `abi_version` and `wire_version` are checked separately.

**Decision required first:**

| Option | On a new package release | Trade-off |
|---|---|---|
| A: exact matching (current) | Both builds can be loaded; components move by removal and addition. | Side-by-side and canary moves, but every release loses state. |
| B: ignore package version in matching | One build per plugin ID; replace and restart, components upgrade in place. | Keeps state; no side-by-side builds. |
| C: A plus explicit per-component "rebind to version" | Both builds loaded; upgrade on request. | Both benefits; most complex; needs a rule to retire old builds. |

**What remains once B or C is chosen:**

1. Allow one package version per plugin ID (Option B), and make provenance and
   accepted-update identity checks ignore the package version on the upgrade
   path only. Resource recipes (native bootstrap, bootstrap coordination,
   time merge) and the transactional-transformer registry also key on the full
   identity and must be upgraded too.
2. Replace `configuration_version` with a supported range
   (`minimum..=current`) and add a pure `migrate_configuration(from, config)`
   to the factory planner: no I/O, clock, randomness or secret resolution; it
   never changes descriptor, ports, role or bindings. Out-of-range or newer
   versions fail without touching stored state.
3. Provide an SDK migration-chain helper (one step per version, contiguity
   checked at registration) with golden-fixture and idempotence test support.
4. On load, migrate a stored component's configuration, have the plugin assess
   the change and choose `Preserve`, `Rebootstrap` or `Reset`, check feasibility
   with existing update planning (see [CW-92](#cw-92)), and run it as an ordinary
   accepted update with a deterministic operation ID. Initialization receives
   the previous package and configuration versions so the plugin can migrate its
   own persisted state. Failure leaves a visible error state; there is no
   automatic fallback.
5. Make Server's file-based configuration use the same migration.

Open questions: a `Reset`/`Rebootstrap` upgrade causes a processing gap and may
start an expensive rebuild on restart without confirmation; migration is
forward-only, so rollback needs the old binary plus a pre-upgrade configuration
snapshot; unverified whether the previous versions are enough for plugins to
judge state compatibility or whether some need their own state version.

**Evidence:** `drasi-core/components/computation-contracts/src/identity.rs:7-18`,
`drasi-core/components/host-sdk/src/computation/loader.rs:758,1220`
(`mem::forget`), `drasi-core/components/host-sdk/src/plugin_registry.rs:144-147,696-703`,
`drasi-core/lib/src/computation/v1/graph/topology.rs:135-138,242-249`,
`drasi-core/components/computation-plugin-sdk/src/metadata.rs:289-292,556-560`,
`drasi-core/lib/src/computation/v1/graph/resources.rs:463-487`,
`drasi-core/lib/src/computation/runtime/component.rs:401-415`,
`drasi-core/components/computation-plugins/transformers/query/src/update.rs:44-58`,
`drasi-core/components/host-sdk/src/management/{native,bootstrap_coordination,time_merge}.rs`,
`drasi-core/components/host-sdk/src/computation/bootstrap.rs:40-43`,
`drasi-core/lib/src/computation/v1/transaction_transformer.rs:82`,
`drasi-server/src/computation.rs:688-705`.

**Done when:** A reference factory at configuration v3 with a v1→v3 chain
restores stored v1 and v2 components through static and dynamic loading;
`Preserve`, `Rebootstrap` and `Reset` upgrades each run with the plugin choosing;
unsafe choices, unsupported assessments and failed migrations leave a visible
error; out-of-range versions leave stored state unchanged; secret references stay
unresolved; an interrupted upgrade retries with the same operation ID; a broken
chain fails registration; and the chosen option's loading rule is enforced with
a clear error.

<a id="cw-106"></a>

### Finish declared component descriptions (CW-106)

**Kind:** Improvement. **Area:** Core, Host SDK, native SDK. **Priority:** Medium.
**Effort:** Medium.

Components describe themselves through one `ComponentDescription`
(component facts, per-port facts, per-dependency requirements). Factories
declare it from their configuration and the declared `ResourceCapabilities`
of bound resources, constructed components from the instance, and the graph
compares descriptions when it binds edges and resources, recording a
mismatch on the nodes. The model carries only producer identity and replay,
input delivery, component progress persistence and resource durability so
far.

**What remains:**

- QoS replay pipes that need producer identity are not yet checked against
  the producer's description.
- A native instance's description is its factory's describer answer; the
  plugin's constructed object does not report its own description across the
  ABI, so the instance-versus-factory check is meaningful only for in-process
  components.
- Native constructions outside the graph (for example the Host's ordinary
  query adapter) derive the describer's resource capabilities from the bound
  objects; resource handles created without `with_capabilities` stay
  undeclared and satisfy no requirement.
- Bootstrap facts are declared: providers declare completion (quiescent or
  durable), pairing protocol, element namespace and identity; sources declare
  their coordination offer and element identity; the Host declares bootstrap
  port facts; the graph checks bootstrap edges (`check_bootstrap_edge`), and a
  relay's completion is resolved through its own providers.
- Most network sources declare their own progress persistence as
  `Undeclared`; declare it where a requirement needs it. The PostgreSQL,
  SQLite and MySQL sources declare theirs and require a durable progress
  binding (`source_progress`/`consumer_progress`) for the modes whose cursors
  must survive a restart (their untracked modes declare `Volatile`); the SQL
  Server source declares the durability of its bound consumer progress.
- Component progress persistence is declared but not yet compared along an
  edge: nothing refuses a `Volatile` source feeding a query with durable
  progress (see [MY-36](#my-36)).
- `CheckpointedSink` and `QueryReplayTransformer` return
  `ComponentDescription::undeclared()`. The graph therefore applies Stall to a
  wrapped sink whose policy is Skip and treats its inputs as unkeyed, and the
  replay transformer's outputs satisfy no keyed input
  (`drasi-core/lib/src/computation/v1/consumer_recovery.rs:887-890,966-969`).
- The in-process graph does not refuse a consumer that keys delivery on
  producer identity without declaring a keyed input; only the Host SDK refuses
  such native consumers
  (`drasi-core/components/host-sdk/src/computation/factory.rs:582-584`). An
  incompatible producer is then found only when the consumer's delivery
  runner rejects a delivery. **Unverified:** whether that rejection reaches
  the graph unclassified, which the graph retries as transient.
- Bootstrap edges do not compare output volatility with a persistent
  consumer. A persistent query given a volatile upstream output snapshot is
  accepted at binding (a `Durable` acceptance takes any completion) and fails
  only when the bootstrap runs ("persistent query cannot bootstrap from a
  volatile output", `drasi-core/lib/src/computation/v1/query.rs:174-180`,
  `output_bootstrap.rs:167`). Declare snapshot durability on bootstrap outputs
  and refuse this in `check_bootstrap_edge`.
- The further facts identified by the compatibility audit (recovery and
  control messages, ordering, bootstrap capabilities, archive and atomic
  publication support, credential kinds) belong in this model rather than in
  new mechanisms.

**Evidence:** `drasi-core/components/computation-contracts/src/description.rs`;
`drasi-core/lib/src/computation/v1/graph/controller.rs`
(`declared_description`, `check_edge_compatibility`,
`record_incompatible_edge`);
`drasi-core/components/host-sdk/src/computation/factory.rs`
(`describe_request`).

**Done when:** every compatibility fact the graph relies on is declared in
the description and checked when the edge or resource is bound.

<a id="cw-104"></a>

### Remove the fixed startup hold for late consumer progress bindings (CW-104)

**Kind:** Issue (improvement). **Area:** Core. **Priority:** Medium.
**Effort:** Medium.

When a consumer's progress binding arrives after its producer's start group is
already reserved, the reconciliation that binds the late edges waits on a
fixed hold of up to the graph cleanup timeout (5 s by default) for that start
group's report before it proceeds and starts the waiting members. The start
deadlock itself is fixed (`Operations::conflicts` no longer keeps settled
members reserved), but the hold remains, so such graphs start about 5 s late.

Removing the hold outright broke `managed_native_wiring_multi_thread`, because
the same wait also keeps management and stop requests queued behind the
in-flight start. The hold is the start-report wait: the binding reconciliation
drives the start for up to `cleanup_timeout` while awaiting its report.

**What remains:** replace the fixed hold with a wait on the events that end it
(the producer's start report, or its members settling), while still keeping
management and stop requests ordered behind the start, as that test requires.

**Example:** `staggered_consumer_progress_bindings_multi_thread` takes about
5 s although every component is ready well before; the `current_thread`
variant shares the same hold.

**Evidence:** `drasi-core/lib/src/computation/v1/graph/reconcile.rs:3343-3353`
(`drive_reconciliation(..., graph.cleanup_timeout, ...)` awaiting the start
report); `drasi-core/lib/src/computation/v1/lifecycle.rs:29`
(`DEFAULT_CLEANUP_TIMEOUT` = 5 s);
`drasi-core/lib/src/computation/v1/graph/controller.rs:3284-3305`
(`Operations::conflicts`); `drasi-core/lib/tests/computation_queries.rs:861-994`
(staggered-binding tests and the comment at `:945-946`).

**Done when:** both staggered-binding tests finish well under the cleanup
timeout on both Tokio flavours, and `managed_native_wiring_multi_thread` and
the other management and stop ordering tests still pass.

<a id="cw-107"></a>

### Scale the cleanup deadline for large graphs (CW-107)

**Kind:** Issue. **Area:** Core lifecycle.
**Priority:** Medium. **Effort:** Small to medium.

Each component's stop has its own cleanup deadline, 5 seconds by default, but
a graph stops all its components at once. In a large graph the stops compete
for the same threads, so a component that stops in milliseconds on its own
can miss its deadline. The test `publication_tests::control_latency` stops
graphs of 16, 1,024 and 4,096 nodes; the 4,096-node case timed out in debug
builds until the test raised the deadline to 60 seconds.

**What remains:** decide whether the deadline should cover the whole stop
rather than each component, scale with graph size, or be documented as a
setting large deployments must raise (`DrasiLib::builder().with_cleanup_timeout_secs`,
`drasi-core/lib/src/builder.rs:586`). **Unverified:** whether release builds
of realistic graphs hit the default.

**Example:** an application with 4,000 components calls stop; some components
report a cleanup timeout even though each would have stopped promptly.

**Evidence:** `drasi-core/lib/src/computation/v1/lifecycle.rs:29`
(`DEFAULT_CLEANUP_TIMEOUT`); `drasi-core/lib/src/computation/runtime/events.rs:310,330`
(the test's 60-second override).

**Done when:** stopping a 4,096-node graph with default settings succeeds in
debug builds on both Tokio flavours, or the limit is documented with
guidance, and the test runs with the default deadline.

<a id="cw-115"></a>

### Treat a retrying sink as running at instance start and shutdown (CW-115)

**Kind:** Issue. **Area:** Core lifecycle (`DrasiLib`). **Priority:** Medium.
**Effort:** Small to medium.

A sink that is retrying a transient delivery failure is running: its lifecycle
is `Running`, its health `Degraded`, and the graph keeps retrying the same input
until it is delivered. Two instance-wide operations report it as failed:

- `DrasiLib::start()` returns a `PartialStartError` ("managed components did not
  all become ready: starting managed component '<name>' (<id>) failed during
  Processing") when a sink's first delivery fails transiently while start waits
  for readiness. The instance stays running and the sink recovers on its own,
  but a second `start()` fails with "Server is already running".
- `DrasiLib::shutdown()` while a sink is retrying returns an error carrying the
  sink's last transient failure ("instance shutdown failed: shutting down
  computation graph: … failed during handle: transient delivery failure …"),
  although cleanup completes (`ShutdownOutcome::completion` is `Complete`).
  Stopping only that sink with `GraphControl::stop_components` during its retry
  wait reports `StopOutcome::Stopped`. **Unverified:** which step of the
  shutdown stop turns the recorded retryable failure into a stop failure.

**What remains:** define how start and shutdown treat a retrying sink,
consistently with `stop_components`: start should not report a running,
retrying sink as a startup failure, and shutdown should report an undelivered
held input as a distinct outcome rather than as a component failure.

**Example:** in the developer guide's webhook lesson, stopping the receiver,
saving a low quantity and pressing Ctrl-C makes Stockroom exit with the
webhook's connection error.

**Evidence:** `drasi-core/lib/src/lib_core.rs:600-616` (managed readiness during
start); `drasi-core/lib/src/computation/instance.rs:248-290` (`finish_driver`
collects stop-report failures); `drasi-core/lib/src/computation/v1/graph/sink_batching.rs:450-466`
(an interrupted retry); `drasi-core/lib/tests/computation_sink_delivery_failures.rs:317-352`
(`stop_during_retry_backoff_exits_without_waiting_for_it`). Both behaviors were
reproduced with a test sink that always fails transiently, and the shutdown
case also with the native HTTP sink.

**Done when:** start does not report a retrying sink as a startup failure,
shutdown during a retry reports a documented outcome consistent with
`stop_components`, and tests cover both.

<a id="cw-116"></a>

### Add constructed bootstrap providers to DrasiLib batches (CW-116)

**Kind:** Improvement. **Area:** Core public API. **Priority:** Low.
**Effort:** Small.

`ComponentBatchBuilder` adds constructed sources, transformers, sinks and
services, but it has no method for a constructed bootstrap provider, the fifth
component role. Within `DrasiLib` a provider node must come from a factory
specification (`component`), although the standalone `ComputationGraph`
builder accepts constructed providers (`bootstrap_provider`).

**Evidence:** `drasi-core/lib/src/computation/components.rs:45-73`;
`drasi-core/lib/src/computation/v1/graph.rs:1015`.

**Done when:** `ComponentBatchBuilder` accepts a constructed bootstrap provider,
a test connects one over a bootstrap edge, and the developer guide's bootstrap
chapter shows it.

<a id="cw-114"></a>

### Reject label selections on native query inputs when the query is accepted (CW-114)

**Kind:** Issue. **Area:** Core query assembly and management validation.
**Priority:** Low. **Effort:** Small.

A query input can carry `nodes` and `relations` label selections, which are
subscription settings for ordinary sources. For an input from a component in
the graph, such as another query or a native source, the declaration is
accepted and the query then fails at construction with "native input label
selection belongs in the graph, not Source-plugin subscription settings". Drasi
Server's API therefore answers 200 for a query that can never start.

**Example:** learning-drasi-server's Curbside Pickup tutorial: adding `nodes` to
a query-to-query input is accepted, then the query fails to construct.

**Evidence:** `drasi-core/lib/src/computation/v1/pipeline.rs:556-573`.

**Done when:** such a declaration is rejected when it is accepted, with the same
message, and a test covers it.

<a id="cw-44"></a>
<a id="hosting-connectors-server-and-examples"></a>
<a id="current-required-family-boundary"></a>

### Finish the remaining data connections (CW-44)

**Related review findings:** [CR-P06](#cr-p06) and [CR-P07](#cr-p07) identify
Kubernetes capability/configuration decisions for the native port, not already
implemented behavior that parity alone requires.

**Kind:** Issue/integration. **Area:** Core, Server.
**Priority:** High for required families. **Effort:** Very large.

Some connectors still exist only as old-interface (ordinary) plugins, and some
native connectors cannot yet handle realistic workloads. Until each required
family has a working native source, initial-data provider and sink, users must
keep the old stack for them.

**What remains:**

| Area | Remaining work |
|---|---|
| Kafka and Kubernetes | Write native sources and initial-data providers, packaged independently. Only ordinary versions exist (`drasi-core/components/sources/{kafka,kubernetes}`, `drasi-core/components/bootstrappers/{kafka,kubernetes}`); there is nothing under `drasi-core/components/computation-plugins/sources/`. |
| Other legacy sources and initial-data providers (decision Source D-14) | These ordinary plugins have no native counterpart and no recorded decision: sources `cloudflare-radar`, `dataverse`, `gtfs-rt`, `here-traffic`, `hyperliquid`, `neo4j`, `open511`, `oracle`, `platform`, `ris-live`, `sui-deepbook` and `websocket` (`drasi-core/components/sources/*`), and initial-data providers `cloudflare-radar`, `dataverse`, `gtfs-rt`, `here-traffic`, `neo4j`, `open511`, `oracle`, `platform`, `scriptfile` and `sui-deepbook` (`drasi-core/components/bootstrappers/*`). `here-traffic` and `open511` are unpublished. For each, decide (as [Sink D-7](#sink-d-7) does for reactions): port to the native SDK, keep ordinary, or retire. `otel` is ported as [`drasi.network/otel-source`](../../components/computation-plugins/sources/otel/README.md); `mock` is [MK-01](#mk-01); `noop` needs no port; check whether the `application` initial-data provider is covered by the application source's native module. |
| Schema discovery | The native PostgreSQL, MySQL, SQL Server, SQLite and HTTP sources implement `source_schema_reader`. Kafka and Kubernetes need one when they are ported. Factory configuration schemas are not source data schemas. |

**Constraints:** keep what already works natively: HTTP bootstrap, adaptive
batching, multi-item gRPC delivery, output templates, SSE snapshot readers,
gRPC TLS/mTLS, HTTP/gRPC ingress authentication and durable admission (graceful
stop finishes in-flight commits and answers them; admission waits on a full
mailbox or journal), and the PostgreSQL/MySQL/SQLite slices. HTTP REST bootstrap does not establish an
atomic snapshot/live cut, and event-only sinks must not advertise replacement
or completion guarantees they do not implement. Port each family as a separate,
reviewable change. This work adds no public DrasiLib SQL API.

Per-plugin native work is listed under [Native sources](#native-sources) and
[Native sinks](#native-sinks); which legacy reactions need a native sink is
[Sink D-7](#sink-d-7), and moving application consumers off the ordinary
`ApplicationReaction` is [LR-01](#lr-01).

Real-service, discovery, security-boundary and static/loaded conformance tests
are in [CW-41](#cw-41), restart coverage in [CW-14](#cw-14), and the SQL Server
environment gate in [CW-78](#cw-78).

**Example:** a Server configuration with a `kafka` source can only be loaded
through the ordinary plugin interface; there is no native factory to select.

**Done when:** every required source, initial-data provider and sink exists as
a native plugin, a capability-by-capability comparison with its ordinary
counterpart finds nothing working that was dropped, and its
[component qualification](#cw-41) and
[Server acceptance](../../../drasi-server/docs/remaining-work.md#sw-21) pass.
Skipped cases and matching test counts are not proof.

<a id="cw-18"></a>

### Keep saved data and retries unambiguous (CW-18)

**Related review findings:** [CR-C10](#cr-c10), named-timezone representation,
and [CR-P18](#cr-p18), redb WAL format refusal. These inform explicit format
contracts; they do not require automatic migration of old state.

**Kind:** Format/release improvement. **Area:** Core contracts, storage, SDK/Host.
**Priority:** High. **Effort:** Medium to large.

Saved and transmitted records must say clearly which fields identify a request,
result or configuration, and an old or damaged record must fail with a clear
reason. Otherwise a harmless formatting change can make a retry look like new
work, or an old record can look like corruption.

**What remains:**

- Audit current saved and wire formats and list, for each, which fields are
  identity and which are diagnostics or formatting.
- Read the version or header before decoding the rest of the record, so an
  unsupported version is reported as such. Today the envelope codecs decode the
  whole current frame layout first and only then compare the version
  (`drasi-core/components/computation-contracts/src/codec.rs:153-162`,
  `.../binary_codec.rs:78-82`). Records with their own embedded versions, such
  as the time-merge snapshot (`Saved { version: 2, .. }` in
  `drasi-core/lib/src/computation/v1/time_merge/recovery.rs:51-58`), need the
  same check.
- State, per format, whether an unsupported version is upgraded, refused or
  reset. Golden bytes, identity and malformed-record tests are in
  [CW-41](#cw-41).

**Example:** an envelope written by an older layout whose fields differ fails
with a JSON or MessagePack decode error, not "unsupported computation envelope
format version", so an operator cannot tell an old record from a corrupt one.

**Done when:** restarts and retries in the current format keep their identity,
and incompatible records are rejected with a version error without silently
resetting valid state.

<a id="cw-19"></a>

### Reject excessively deep data safely (CW-19)

**Kind:** Hardening. **Area:** Core contracts, codecs, providers, network
sources. **Priority:** High where input could terminate the process.
**Effort:** Large.

Very deeply nested input can exhaust the call stack while being parsed,
converted or dropped, which aborts the whole process rather than failing one
request. Envelope lineage is already bounded (1,024 entries,
`MAX_LINEAGE_DEPTH` in `drasi-core/components/computation-contracts/src/envelope.rs:225`),
and lineage is destroyed iteratively. Nested data values are not.

**What remains:**

- Audit every decoder and value conversion that accepts external data and set
  an explicit, documented nesting limit. Current state:
  - XML webhook bodies and other network-source input are tracked in
    [NET-10](#net-10).
  - Unverified: JSON (`serde_json`) and YAML (`serde_yaml`) parsing stop at
    their library default of 128 levels, protobuf (`prost`) at 100 and
    MessagePack (`rmp-serde`) at 1,024. Drasi neither sets nor documents these
    limits, and whether 1,024 levels of recursive conversion and drop fit in a
    plugin thread's stack has not been tested.
- Make dropping a deeply nested value safe after an error or cancellation.
- Add malformed-input tests for each source format (JSON, XML, YAML, protobuf
  `Struct`, MessagePack). Decoder, drop and allocation qualification is in
  [CW-41](#cw-41).

**Example:** a 1 MiB XML webhook body made of about 150,000 nested `<a>`
elements fits under a `maxMessageBytes` of 2 MiB, parses without error, and
then recurses once per level while being converted and dropped.

**Done when:** each supported input format has a documented depth limit, and
over-deep input fails that request with an error instead of overflowing the
stack.

<a id="cw-66"></a>

### Retire mitigated legacy dependency paths (CW-66)

**Kind:** Dependency remediation. **Area:** Core and affected consumers.
**Priority:** Medium for upstream replacements; reassess before expanding usage.
**Effort:** Depends on compatible upstream releases.

A few dependencies with published security advisories (RustSec) remain because
no compatible patched release exists. Each is used in a way the advisory does
not affect, or is wrapped by a safeguard, but those boundaries break if usage
changes.

| Retained dependency | Where | Why it is acceptable now |
|---|---|---|
| Azure Core 0.21 (Core) and 0.20 (test-infra) | Azure Storage reaction; test-infra e2e framework and `arn` tool | Per-call transport safeguards keep SDK signing/TLS and redact authorization before logging. SharedKey/Table remain available. |
| quick-xml 0.31 (via Azure) | Azure Storage | A quick-xml 0.41 preflight rejects duplicate or excessive attributes in fixed-schema service metadata; the affected namespace resolver is not used, and blob content is not parsed as XML. |
| RSA 0.9 | MySQL | Only public-key OAEP encryption is used, not the affected private-key operations. |
| zstd 0.13 / zstd-safe 7 | MySQL | Only read-side decoding is used, not the affected context reuse, write, dictionary or multithreaded operations. |
| ZIP 0.6 | Server and test-infra build tooling | Only extracts the bundled Swagger UI archive, never user archives. |
| Old `rand` | Transitive | The advisory's logging feature is disabled. |

**What remains:**

- Adopt patched Azure Core/Table and MySQL compression releases once they are
  compatible, and remove the corresponding safeguards.
- Re-audit before using any of these libraries in a new way; the safeguards
  cover only the current uses.
- Do not add advisory exceptions or weaken allocator/license rules just to make
  a scan pass. Raw audit scans in Core, Server and test-infra still report
  these findings.

**Example:** using `azure_data_tables` 0.21 to parse user-supplied XML would
fall outside the quick-xml preflight and reopen the advisory.

**Done when:** each retained path is replaced by a patched release, or a scan
shows no advisories other than ones with a documented, still-valid boundary.

<a id="cw-22"></a>

### Make errors explain what happened and what to do (CW-22)

**Related review findings:** [CR-L03](#cr-l03) retains the component-context
question despite preserved causes; [CR-D02](#cr-d02) is misleading propagation
documentation; [CR-A01](#cr-a01) needs placeholder reachability established.

**Kind:** Issue/audit and API improvement. **Area:** Core, Server, UI.
**Priority:** High. **Effort:** Large.

When something fails, a user should be able to tell which component failed,
what stopped, what was saved or left unfinished, whether cleanup failed, and
what is safe to retry. The same failure should read the same way in the
library, Server, UI, logs and monitoring.

**What remains:**

- Audit remaining public-path `expect`/`panic!` calls and places where a typed
  error is converted to a string, and separate proven internal invariants from
  recoverable failures.
- Check ordinary-source initialization and retry state, and middleware
  recovery poisoning and retry classification.
- Replace fabricated edge identifiers and ambiguous, duplicated or debug-only
  cause messages with the actual operation context.
- For queries running as native plugins, check that a plugin-owned Rust cause
  can still be found through `DrasiError::downcast_ref` and is explained
  clearly in Server and UI output. A correct reset refusal does not prove this.
- Keep using `DrasiError::classification()` and typed causes
  (`drasi-core/lib/src/error.rs:212-240`); do not replace them with string
  matching. Cross-surface error and typed-cause tests are in [CW-10](#cw-10).

**Example:** a query plugin fails with a typed storage error. The Server API
should return a response that names the query, says the storage is intact and
the operation can be retried, instead of a generic "internal error" string.

**Done when:** a user can diagnose and recover from the selected failures
without guessing from a generic error, and a rejection is never reported as
successful work.

<a id="cw-23"></a>

### Make live status and log updates reliable (CW-23)

**Related review finding:** [CR-L02](#cr-l02) distinguishes existing log-loss
counts from their unverified exported-metric integration.

**Kind:** API improvement. **Area:** Core, Server, SDK/Host.
**Priority:** High. **Effort:** Medium to large.

Clients that follow component status, events or logs can fall behind or
reconnect. They need to know when they missed updates and how to get the
current state, and an old subscription must never report a replacement
component as the one it was following.

**What remains:**

- Define slow-reader and reconnect behaviour for status, event and log
  subscriptions: missed updates are detected and a current snapshot can be
  fetched.
- Keep instance identity in every update, and release observer state when the
  component that owns it is removed.
- Document the snapshot/changed acknowledgement contract for progress
  observers so clients do not busy-loop without consuming a snapshot.
- State in the API docs that coalesced status is not a full event log.
  Observer handover, acknowledgement, callback and reconnect tests are in
  [CW-10](#cw-10).

**Example:** the documented log loop for `subscribe_source_logs_by_id`
(`drasi-core/lib/src/inspection.rs:600-630`) is
`while let Ok(log) = receiver.recv().await { ... }`. When the reader falls
behind, the broadcast receiver returns `RecvError::Lagged`, the loop ends, and
the client silently stops receiving logs.

**Done when:** readers either receive the required updates or are explicitly
told to refresh, and an old reader cannot report a replacement as its old
component.

<a id="cw-17"></a>

### Make components stop their own background work (CW-17)

**Related review findings:** [CR-K04](#cr-k04), ordinary foreign-call retention
after abandoned cleanup, and [CR-K05](#cr-k05), serialized foreign drops.
Do not trade away the documented safety boundary to claim cleanup.

**Kind:** Ownership audit/hardening. **Area:** Core, components and providers.
**Priority:** High. **Effort:** Medium to large.

If a component starts a task, thread or connection that nothing owns, that work
can keep running after the component stops or is removed. It can then hold
resources a replacement needs or act on stale state.

**What remains:**

- Audit components and providers that create their own tasks, threads or
  connections. Take ownership before starting them and keep it until the work
  has really ended, including during failed startup and cancellation.
- Cover blocking storage work that was already submitted and construction
  results that were rejected.
- The cleanup/leak distinction is verified in [CW-08](#cw-08).

**Example:** the ordinary PostgreSQL initial-data provider detaches its
connection driver with `tokio::spawn` and never joins it
(`drasi-core/components/bootstrappers/postgres/src/postgres.rs:389-394`). The
SQLite provider runs its table scan in `spawn_blocking`
(`drasi-core/components/bootstrappers/sqlite/src/lib.rs:118-128`); cancelling
the bootstrap drops the await but the scan keeps running.

**Done when:** a stopped or removed component leaves no unowned background
work that can affect a replacement or keep its resources busy.

<a id="cw-33"></a>

### Reclaim bookkeeping left by removed components (CW-33)

**Kind:** Long-running resource improvement. **Area:** Core graph bookkeeping.
**Priority:** Medium. **Effort:** Medium to large.

The computation graph keeps a slot for every component ever added, so a
long-running graph that keeps adding and removing components grows without
bound even if the number of live components stays constant.

**What remains:**

- Measure repeated add/remove cycles in one long-running graph.
- Reclaim or reuse node and component slots that no pending work or cleanup
  still references. `ComputationGraph` stores `nodes` and `components` in
  `Vec`s indexed by stable runtime indices that survive removal
  (`drasi-core/lib/src/computation/v1/graph.rs:1722-1735`); new components are
  always pushed (`.../graph/addition.rs:1101-1102`).
- Any reuse must use generation-checked handles, so a delayed request for an
  old component cannot reach its replacement, and must account for retained
  cleanup and job references.

**Example:** a Server that adds and removes one short-lived query per minute
holds about 525,000 dead slots after a year, although only a handful of
components are live.

**Done when:** a stable number of live components does not accumulate entries
indefinitely, and delayed old requests cannot affect new components.

<a id="cw-29"></a>

### Finish memory limits for saved queues and results (CW-29)

**Related review findings:** [CR-K12](#cr-k12), fixed backend buffer sizes, and
[CR-K17](#cr-k17), ordinary in-memory WAL replay. Their limits are distinct from
the native queue budgets below.

**Kind:** Capacity improvement. **Area:** Core queues, retained state and providers.
**Priority:** Medium. **Effort:** Large.

Several places that hold data in memory have no byte limit, so a slow consumer
or a large result can grow memory until the process is killed. Each kind of
buffer needs its own limit; no single existing limit bounds total memory.

**What remains:**

- Add byte limits for shared transactional queues, saved output and current
  result rows. Shared transactional QoS journals refuse a byte budget today
  (`drasi-core/lib/src/computation/v1/qos_pipe.rs:252,622-625`).
- Reserve capacity before processing commits, and define what happens to an
  oversized input or when a buffer is full.
- Keep these limits distinct: encoded-envelope admission quotas, payload-cache
  bounds, receipt/progress metadata, backend buffers and current-result state.
- Add an optional total in-flight byte budget for the HTTP and gRPC sources
  ([NET-11](#net-11)).

Existing optional byte budgets for FIFO, retained and non-shared QoS stores
(`drasi-core/lib/src/computation/v1/retained_store.rs:116-130`,
`.../qos_pipe.rs:409-416,452-461`) and paged payload caches are already
implemented. Query snapshot/history paging removes whole-response transport
caps, not result-memory limits. Splitting one oversized native item is
[CW-76](#cw-76).

**Example:** a query with a slow subscriber on a shared transactional journal
keeps accepting input; the journal's retained entries grow with no byte limit
until the host runs out of memory.

**Done when:** the selected limits hold through slow consumers, cancellation
and restart, and work that cannot be retained safely is refused instead of
accepted.

<a id="cw-28"></a>

### Reduce unnecessary disk work and growth (CW-28)

**Related review finding:** [CR-P19](#cr-p19), ordinary FFI batches implemented
as per-item calls. This is not an absence of native batch APIs.

**Kind:** Storage/concurrency improvement. **Area:** Core retained, QoS,
delivery and merge stores. **Priority:** Medium. **Effort:** Medium to large.

Some stores rewrite their whole state, or many small records, for every change.
This costs disk I/O proportional to total state rather than to the change, and
can leave obsolete records behind.

**What remains:**

- Measure full-state rewrites, repeated reads, small writes and obsolete stored
  records, specifically:
  - time-merge persistence, which re-encodes every buffered envelope on each
    save (`drasi-core/lib/src/computation/v1/time_merge/recovery.rs:51-80`);
  - whole-metadata rewrites of QoS and delivery ledgers;
  - receipt, cursor and head updates;
  - per-item acknowledgement and decode work.
- Where measurements justify it, use per-entry or per-stream records, safe
  write coalescing and shorter journal locks.
- Keep transaction ownership, crash recovery and subscriber membership atomic;
  do not release a lock early if that breaks the commit protocol. `Busy` is
  bounded admission, not loss; keep explicit caller retry and backpressure.
- Reproduce older storage reports before treating them as bugs. Retained and
  QoS batch receive already exist; the open question is persistence and
  acknowledgement cost.

**Example:** a time merge holding 10,000 buffered events re-encodes and writes
all 10,000 each time one event is admitted or released.

**Done when:** selected workloads show lower storage cost or bounded growth
without changing results, recovery or independent query retention choices.

<a id="cw-32"></a>

### Remove measured repetition from data processing (CW-32)

**Related review findings:** [CR-P10](#cr-p10), Garnet's intermediate
serialization model, and [CR-K10](#cr-k10), whole-set next-member scans.

**Kind:** Measured performance improvement. **Area:** Core processing, codecs and pipes.
**Priority:** Medium. **Effort:** Medium.

The processing path repeats some work for each event: encoding just to
measure or validate, re-hashing, rebuilding metadata and rescanning history.
Removing measured repetition lowers CPU cost without changing results.

**What remains:**

- Profile and remove significant repetition in: codec round-trips used only
  for validation or size measurement (for example
  `BinaryEnvelopeCodec::encoded_size` serializes the whole frame to count bytes,
  `drasi-core/components/computation-contracts/src/binary_codec.rs:70-76`);
  progress-map cloning; payload copies; per-event port parsing and hex
  formatting; repeated lock acquisition; boxed futures.
- Replace idle polling with owner notifications where practical, and measure
  peer-handler wake fan-out and shared-queue head-of-line delay.
- Use existing batched and polling paths before adding a fast-path API.
- Cache only data whose identity is truly stable: encoded size and validation
  can depend on branch-specific annotations, not just the immutable payload.
- Profile actual outbox reads and commit topology rather than assuming every
  read clones all history or every output needs its own transaction. Keep
  ordering, contiguous completion and independent subscriber obligations.
- Query-boundary optimizations should follow [CW-79](#cw-79)'s measurements.

**Example:** an envelope that is size-checked at admission and again at a
retained store is fully serialized twice before it is ever written.

**Done when:** measured processing cost falls while output, identity, replay
and failure behaviour stay the same.

<a id="cw-63"></a>

### Remove the remaining old plugin support (CW-63)

**Related review findings:** [CR-L01](#cr-l01) and [CR-P01](#cr-p01) cover
ordinary bootstrap boundaries; [CR-P13](#cr-p13), [CR-P14](#cr-p14) and
[CR-P15](#cr-p15) cover ordinary packaging; [CR-P19](#cr-p19) covers ordinary
state batching; [CR-K01](#cr-k01), [CR-K02](#cr-k02) and [CR-K03](#cr-k03)
cover process-pinned libraries, runtime and callbacks. Retirement can remove
these paths only after their required consumers move; a native counterpart
alone is not evidence that the ordinary finding is resolved.

**Kind:** Breaking-release cleanup. **Area:** Core, Server and remaining consumers.
**Priority:** Medium. **Effort:** Medium to large.
**Depends on:** The remaining consumer moves in [CW-44](#cw-44).

Ordinary (pre-native) Sources, Reactions and bootstrap providers still run
through a second plugin ABI, Host loader and set of graph adapters. That stack
must stay until every required plugin has a native replacement, but it carries
dead version branches and lenient decoding that hide malformed input, and it
doubles the surface that has to be maintained and tested.

**What remains:**

- Remove dead version gates in the ordinary loader. The loader only accepts
  ordinary ABI 0.19.x, yet still reads trailing registration fields behind
  per-version checks written for older, smaller layouts.
- Remove lenient fallbacks that turn malformed current input into apparent
  success, such as decoding an unenveloped snapshot row as a "bare row" with
  signature 0.
- Inventory the ordinary hosts, proxies, factories, graph adapters
  (`legacy_source`, `legacy_reaction`, `legacy_index`, `legacy_resources`) and
  static registrations; record which required consumer still uses each one.
- After a component's last required user moves to a native plugin, port its
  useful behavior assertions to the native interfaces, then delete the adapter,
  its compatibility-only fixtures, dependency edges and build paths.

Retiring the linked query constructors is a separate item, [CW-80](#cw-80).
Using the native query plugin does not depend on this work.

**Example:** `drasi-core/components/plugin-sdk/src/ffi/vtable_gen.rs:478-489`
accepts any JSON value that is not a `KeyedSnapshotRow` envelope and yields it as
row signature 0, so a current plugin that sends a wrongly shaped row produces a
result instead of an error. Its test,
`make_snapshot_stream_falls_back_to_bare_row` (`:4323-4335`), describes this
as compatibility with pre-0.10 plugins, which the loader already rejects.

**Evidence:**

- `drasi-core/components/host-sdk/src/loader.rs:629-633,668-676`: only ordinary
  ABI 0.19.x is accepted.
- `drasi-core/components/host-sdk/src/loader.rs:470-499,516-528`: identity-provider
  and `set_log_level` fields are still gated on older SDK versions.
- `drasi-core/components/plugin-sdk/src/ffi/vtables.rs:871-890`: layout comments
  for "older plugin binaries".
- `drasi-core/lib/src/computation/v1/legacy_{source,reaction,index,resources}.rs`:
  graph adapters for ordinary plugins.
- Unverified: whether "guessed-schema" fallbacks exist beyond the bare-row
  decode; none were found by searching the Host and SDK.

**Done when:** Each removed path has no required user; unsupported plugin
libraries and malformed current data fail with a clear error; native interfaces
cover the behavior the removed code provided; behavior assertions are kept,
not deleted.

<a id="cw-117"></a>

### Make ordinary sources declare whether they can replay (CW-117)

**Related review finding:** [CR-K14](#cr-k14), HERE traffic's Now-only start
configuration; do not infer historical replay from an inherited default.

**Kind:** Issue. **Area:** Core ordinary source API and source plugins.
**Priority:** Medium. **Effort:** Small to medium.

The ordinary `Source` trait defaults `supports_replay()` to `true`, and the
check that refuses a source without replay as a persistent query's input
relies on it. Only the application, gRPC, HTTP, Kafka, OpenTelemetry and
WebSocket sources override it. A source that cannot replay and keeps the
default, such as the Kubernetes source, is accepted as a persistent query's
input: the first start bootstraps normally, and the next start fails with
"source subscription requires reset/rebootstrap before this query can resume …
bootstrap provider requires an explicit reset under Strict recovery". Drasi
Server then exits because startup was partial.

**What remains:** make replay support an explicit declaration in every ordinary
source (no default, or a default of `false`). Decide each source's answer;
besides Kubernetes, the polling and push sources (for example GTFS-RT, HERE
traffic, Hyperliquid, Open511, RIS Live and Sui DeepBook) are unlikely to
replay. **Unverified:** each source other than Kubernetes.

**Example:** learning-drasi-server's High Risk Containers tutorial with
`persistIndex: true`: change a container image while Server is stopped, then
start Server again.

**Evidence:** `drasi-core/lib/src/sources/traits.rs:234-244` (the default);
`drasi-core/components/sources/kubernetes/src` (no override);
`drasi-core/components/computation-contracts/src/query_recovery.rs:109` (the
error).

**Done when:** every ordinary source states whether it can replay, a source
that cannot is refused as a persistent query's input when the query is
accepted, and a test covers the Kubernetes case.

<a id="cw-112"></a>

### Make the ordinary PostgreSQL source verify its database and survive outages (CW-112)

**Related review findings:** [CR-P02](#cr-p02), [CR-P03](#cr-p03),
[CR-P04](#cr-p04) and [CR-P05](#cr-p05) cover shared decoder/authentication
limitations beyond the three ordinary-source gaps below.
[CR-A02](#cr-a02) distinguishes ordinary TRUNCATE handling from native typed
rejection. Moving consumers to the native source does not by itself remove
limitations in shared helpers.

**Kind:** Issue. **Area:** Ordinary PostgreSQL source and initial-data provider.
**Priority:** Medium. **Effort:** Medium.

Drasi Server's tutorials and examples use the ordinary PostgreSQL source, which
has three gaps that the native PostgreSQL source does not:

- **A replaced database is not detected.** A source with saved progress resumes
  from its saved log position (LSN) without comparing the database's system
  identifier. After the database is recreated, replication starts at the old
  position, changes before it are skipped, and the persistent queries keep
  their old results. The native source binds its cursors to the system
  identifier and database name.
- **An outage fails the source.** After a lost connection the source waits 5
  seconds and tries one reconnect; if that fails, the source fails ("Replication
  failed: Connection refused"). Queries that read it directly fail with "lost
  its explicit lifecycle dependency", and Drasi Server cannot restart them
  without a reset ([SW-38](../../../drasi-server/docs/remaining-work.md#sw-38)).
  The native source reconnects with a bounded backoff policy.
- **Initial-data warnings.** The ordinary PostgreSQL initial-data provider
  receives every label the query reads, including labels served by the query's
  other inputs, and logs "Table '<label>' does not exist, skipping" for each one
  at every start.

**What remains:** fix these in the ordinary source and provider, or move Drasi
Server's tutorials and examples to the native source
([SW-25](../../../drasi-server/docs/remaining-work.md#sw-25)) and retire the
ordinary ones with [CW-63](#cw-63).

**Example:** in learning-drasi-server's Building Comfort tutorial with durable
indexes, removing the database volume and starting again resumes from the old
LSN: a room's temperature is 40 in the database, but its comfort result still
reflects 46.

**Evidence:** `drasi-core/components/sources/postgres/src/stream.rs:200-218`
(`read_lsn = start_lsn.unwrap_or(slot_lsn)`) and `:783-798`
(`recover_connection`); `drasi-core/components/bootstrappers/postgres/src/postgres.rs:436-441`;
for comparison, `drasi-core/components/postgres-common/src/replication.rs:339-357`
(`source_binding`) and
`drasi-core/components/computation-plugins/sources/postgres/src/worker.rs:282`
(`BackoffPolicy`).

**Done when:** the tutorials either use the native source, or the ordinary
source detects a replaced database, reconnects with bounded backoff, and its
provider warns only about labels the source serves, with tests for each.

<a id="cw-113"></a>

### Fill query names in ordinary reaction results (CW-113)

**Kind:** Issue. **Area:** Core ordinary reaction host and reaction plugins.
**Priority:** Low. **Effort:** Small.

`QueryResult.query_name` carries the query's current name "when attached by a
host with name authority", and the log reaction documents a `query_name`
template variable. The ordinary reaction host never sets it: the shared adapter
creates every result with `query_name: None`, and the host fills only
`input_alias`, although it holds a `ComponentDirectory` that can resolve the
name. Templates that use `{{query_name}}` therefore render an empty string, as
in Drasi Server's tutorials. The log reaction also logs its queries at start
with Rust's debug formatting ("receiving results from queries: [Id { id:
ComponentId { … } }]").

**Evidence:** `drasi-core/components/computation-contracts/src/exchange.rs:244-245`;
`drasi-core/components/computation-contracts/src/internal/typed_change/adapters.rs:692-694`;
`drasi-core/lib/src/computation/v1/plugin_reaction.rs:310-315`;
`drasi-core/components/reactions/log/README.md:276`;
`drasi-core/components/reactions/log/src/log.rs:407`.

**Done when:** ordinary reactions receive the query's current name, every
documented log-reaction variable renders, and the log reaction's start message
lists its queries in plain form.

<a id="cw-34"></a>

### Simplify the remaining management code (CW-34)

**Kind:** Maintainability improvement. **Area:** Core graph/query/pipe management.
**Priority:** Medium. **Effort:** Medium to large.

A small number of very large modules own graph construction, live changes,
reconciliation, recovery and shutdown together. That makes ownership and state
transitions hard to follow and lets similar logic drift apart. This item
reduces that risk in small, protected steps. It is not a rewrite and is not a
substitute for any defect fix.

**What remains:**

- Share one event/completion dispatch path between the main controller loop and
  the reconciliation waits, keeping the current resource, peer and control
  servicing.
- Move operation groups, reservations, connection/control state and shutdown into
  explicit state types with one owner each. Make the relationship between a
  component's slot and its reported status explicit, and share
  relationship-policy predicates instead of adding new registries.
- Factor duplicated commit/recovery steps out of the controller, reconciler,
  management runtime and Host processing modules. Share receiver,
  acknowledgement and retention-window logic only where the contracts are the
  same.
- Keep structural validation in one place. `DesiredTopology::validate_structure`
  and graph building both go through `reconcile::validate_definition`; new
  checks belong there, not in a caller. Absent dependencies remain valid waiting
  declarations. Generated/property checks that all entry points agree are
  [CW-10](#cw-10).

Query definition, evaluator setup, result state, publication and recovery
helpers are already shared in `drasi-query-common`; do not move the evaluator
into the Host or extract those again.

**Example:** A change to how a completed operation is reported currently has to
be made in both the controller's event loop and the reconciler's waits
(`tokio::select!` loops in `controller.rs` and `reconcile.rs`) and kept
consistent by hand.

**Evidence (current sizes):**

- `drasi-core/lib/src/computation/v1/graph/controller.rs` (about 8,600 lines).
- `drasi-core/lib/src/computation/v1/graph/reconcile.rs` (about 4,400 lines);
  `validate_definition` at `:1580`.
- `drasi-core/lib/src/computation/v1/graph.rs` (about 3,500 lines).
- `drasi-core/lib/src/management/runtime.rs` (about 2,300 lines).
- `drasi-core/components/host-sdk/src/computation/processing.rs` (about 2,000 lines).
- `drasi-core/lib/src/computation/v1/graph/topology.rs:569-573`:
  `validate_structure`.

**Done when:** Each changed area has a clear single owner and there is one
authoritative graph state. [API/structural conformance](#cw-10),
[combined failures](#cw-39) and [matched measurements](#cw-40) pass before and
after each refactor. Fewer files or lines alone is not completion.

<a id="cw-24"></a>

### Finish logging control for plugins and profiling (CW-24)

**Related review finding:** [CR-K06](#cr-k06) is the narrower ordinary custom-host
fallback ambiguity, not a general inability to honor configured Drasi `Off`.

**Kind:** Operational capability. **Area:** Core observability, native SDK/Host.
**Priority:** Medium. **Effort:** Small to medium.

Applications can compose Drasi's component logging into their own subscriber
(`ComponentLogRuntime`, `logging.layer()`,
`DrasiLib::builder().with_component_logging(...)`; `drasi-core/lib/README.md:803-823`),
and every loaded native plugin forwards its `log` records to the host through
the native logging bridge (`install_host_logger`,
`drasi-core/components/computation-plugin-sdk/src/diagnostics.rs:94`, installed
at `.../computation-plugin-sdk/src/lib.rs:300`). Records made inside a
component's telemetry scope are tagged with that component
(`drasi-core/lib/src/observability/plugin_telemetry.rs:225-250`).

**What remains:**

- Records a plugin makes outside a component scope reach the host logger
  without a component tag. **Unverified:** whether background tasks that
  plugins spawn keep the scope. Check the shipped plugins and either propagate
  the scope or document how plugin authors should.
- Settle the profiling interfaces (`drasi-core/lib/src/profiling/`) and their
  dependencies.
- Correct the `diag!` doc comment
  (`drasi-core/components/computation-plugins/common/network/src/lib.rs:45-48`):
  it says a dynamically loaded library has no logger, but the bridge now sets
  one and its `max_level`.

**Example:** a native source spawns a reconnect task with `tokio::spawn` and
no scope; its "connection refused" warning reaches the host log but not
`/sources/<id>/logs`.

**Done when:** every plugin record that belongs to a component is tagged with
it, the profiling interfaces are settled, and the docs describe the bridge.
Server's component-log test is
[SW-32](../../../drasi-server/docs/remaining-work.md#sw-32).

<a id="cw-25"></a>

### Finish tracing through sinks and destinations (CW-25)

**Kind:** Operational capability. **Area:** Core, native sinks, destinations.
**Priority:** Medium. **Effort:** Medium.

W3C trace context is carried from source ingress through inputs, outputs,
pipes, batches and admission; batches and joins use span links with limits;
and the native HTTP and gRPC sources accept an incoming `traceparent`
(`drasi-core/lib/README.md:1095-1107,1195-1200`). Replay after a restart starts
fresh context instead of inventing durations
(`drasi-core/lib/README.md:1268-1278`).

**What remains:**

- The native HTTP and gRPC sinks do not send `traceparent` to destinations, so
  a trace stops at the sink; no `traceparent` appears in
  `drasi-core/components/computation-plugins/sinks/*/src/`.
- Record one span per delivery attempt, so a retry is visible.
  **Unverified:** whether existing sink spans already distinguish attempts.
- Keep trace context separate from the identity used to deduplicate or
  recover business work, such as the `x-drasi-delivery-id` idempotency key.

**Example:** a POST to the native HTTP source updates a row, a query joins it,
and an HTTP sink delivers the result after one retry. The trace should show
both delivery attempts and continue in the destination service.

**Done when:** a selected request can be followed through each delivery
attempt into the destination, with no invented duration across a restart.
Persistent reconstruction has no historical context by design.

<a id="cw-26"></a>

### Fill the remaining measurement gaps (CW-26)

**Related review findings:** [CR-L02](#cr-l02), log-loss metric integration;
[CR-P16](#cr-p16), ordinary platform timing placeholders; [CR-K08](#cr-k08),
adaptive backlog estimates. These do not invalidate existing native spans or
queue counters.

**Kind:** Operational capability. **Area:** Core, plugins, Server.
**Priority:** Medium. **Effort:** Medium.

Core records metrics for every component kind: `drasi.component.*`
(availability, calls, inputs, outputs, operation duration), queue pressure
(`drasi.queue.depth`, `depth_max`, `blocked_enqueues`, `drops`,
`edge_info`), `drasi.source.events_dispatched`, `drasi.errors`,
`drasi.pipeline.end_to_end.duration`, `drasi.query.*` and the reaction
checkpoint, dedup, gap and recovery counters. Server exports them over OTLP.

**What remains:**

- Earliest-subscriber or per-consumer QoS lag. Queue depth and blocked
  enqueues show pressure but not which subscriber holds it back.
- Connector-specific plugin metrics. The SDK facility exists
  (`ConstructionContext::plugin_metrics`,
  `drasi-core/lib/src/computation/v1/graph/specification.rs:867`), but no
  connector plugin uses it yet; see [Source X-07](#source-x-07).
- Conformance assertions that each metric reports exactly the expected counts
  and timing boundaries across normal work, retries, batching and failure.
  These live with [public observation conformance](#cw-10) and
  [completion evidence](#cw-36).
- **Unverified:** why `drasi.reaction.results_processed` stays at zero in
  Server's walkthrough
  ([SW-36](../../../drasi-server/docs/remaining-work.md#sw-36)).

**Example:** a QoS channel whose slowest required subscriber is a slow HTTP
sink blocks admission at the source. `drasi.queue.blocked_enqueues` rises, but
no metric names the lagging subscriber.

**Done when:** subscriber lag and connector metrics exist, and the
conformance assertions pass.

<a id="cw-69"></a>

### Curate the public API before a stable release (CW-69)

**Related review findings:** [CR-C01](#cr-c01), aggregation interface narrowing;
[CR-C08](#cr-c08) and [CR-C09](#cr-c09), shared value representation questions;
[CR-K18](#cr-k18), ordinary broadcast filtering limits. Keep that broadcast
contract distinct from the native pipe naming question below.

**Kind:** API improvement. **Area:** Core, shared contracts and SDK consumers.
**Priority:** High before advertising a stable API. **Effort:** Large.

Much of `drasi-lib`'s public surface is exposed by accident through wildcard
re-exports, and several inspection and status values have unclear or
inconsistent meanings. Once a stable release is advertised, every exported item
becomes a compatibility promise, so the surface has to be chosen deliberately
first.

**What remains:**

- Replace wildcard re-exports with intentional exports and document what the
  `computation::v1` namespace promises.
- Review public constructors, builders, `#[non_exhaustive]` use and exposure of
  third-party collection types; choose extensibility per type rather than
  hiding every field.
- Group the `DrasiLib` facade by responsibility while keeping one runtime owner.
- Return structured values instead of `Debug` strings from inspection, use
  explicit component kinds instead of catch-all role fallbacks, and document the
  casing/translation between public and factory settings.
- Give `ManagementStatus.converged` (field) and `ManagementStatus::converged()`
  (method) distinct, documented meanings or names.
- Define builder setting precedence, including repeated `with_management`
  calls.
- Clarify `BroadcastPipe`'s single-receiver, drop-oldest contract, or rename it,
  so it is not confused with QoS multicast.
- Preserve typed error classification and causes.

**Example:**

- `status.converged` can be `true` while `status.converged()` returns `false`,
  because the method also requires no error, no reconciliation in progress and
  no resource errors.
- `builder.with_management_resources(r).with_management(opts)` silently discards
  `r`, because `with_management` replaces the whole options value.
- Graph inspection reports `"kind": format!("{:?}", component.kind)`, so
  renaming a Rust enum variant changes the public JSON.

**Evidence:**

- `drasi-core/lib/src/computation/v1/mod.rs:196-200`,
  `drasi-core/lib/src/computation/v1/graph.rs:52-53`,
  `drasi-core/lib/src/queries/mod.rs:43-50`,
  `drasi-core/lib/src/observability/mod.rs:25-28`: wildcard re-exports.
- `drasi-core/lib/src/management/mod.rs:230-249`: `ManagementStatus`.
- `drasi-core/lib/src/builder.rs:200-231`: `with_management` and
  `with_management_resources`.
- `drasi-core/lib/src/computation/v1/inspection.rs:277-319`: `Debug`-formatted
  fields.
- `drasi-core/lib/src/computation/v1/broadcast_pipe.rs:202-205`.

**Done when:** Representative downstream clients build against the curated
surface; the compatibility policy is written down and distinguishes Rust API,
plugin ABI, wire and persisted-format promises; numeric revision/fencing values
are not presented as authorization capabilities. Downstream compilation,
API/semver and publication checks are [CW-85](#cw-85); behavioral API
conformance is [CW-10](#cw-10).

<a id="cw-71"></a>

### Add optional configuration-store housekeeping integrations (CW-71)

**Kind:** Optional operational improvement. **Area:** Core management, providers, Server.
**Priority:** Medium for long-lived managed deployments. **Effort:** Large.

The persistent (redb) managed-configuration store keeps named configuration
snapshots until they are deleted by hand, and its encryption key must be
passed in as raw bytes. Long-lived deployments need age-based snapshot cleanup
and integration with a real key provider.

**What remains:**

- Add opt-in snapshot retention by age that uses the existing revision-checked
  delete, so it cannot remove a snapshot that changed concurrently.
- Integrate an application key provider (for example a secret store or KMS)
  with the existing atomic key rotation, so a missing or rotated key leaves the
  store recoverable across cancellation and restart.
- Document backup handling and key recovery. State plainly that rotation does
  not securely erase or re-encrypt existing backups, and that restoring a
  configuration snapshot does not rewind query data or receipt batches.

Already present, and not to be replaced by a second store: opt-in receipt
expiry (`receipt_batch_capacity`), revision-checked snapshot deletion and
`rotate_key`.

**Example:** An operator wants "delete configuration snapshots older than 30
days" and wants the store key fetched from Azure Key Vault at startup and
rotated there. Today both need custom application code around
`delete_configuration_snapshot` and `rotate_key(key: [u8; 32])`.

**Evidence:**

- `drasi-core/components/state_stores/redb/src/configuration/housekeeping.rs:10-16`
  (`ConfigurationStoreOptions`), `:374` (`rotate_key`).
- `drasi-core/components/state_stores/redb/src/configuration.rs:132`: the store
  takes a raw 32-byte key.
- `drasi-core/lib/src/management/mod.rs:398-450`: snapshot list/delete/restore API.

**Done when:** Retention cannot delete a concurrently changed snapshot; expired
requests cannot be reapplied as new work; unavailable or rotated keys preserve
recoverable ownership across cancellation and restart. Persistent namespace,
operator-recovery and race tests are in [CW-07](#cw-07) and [CW-39](#cw-39).

<a id="cw-72"></a>

### Make component capabilities and wrapper delegation explicit (CW-72)

**Related review finding:** [CR-C02](#cr-c02) retains the middleware factory
dependency question. Existing element-index injection and native negotiated
services do not establish that the broader factory proposal is needed or done.

**Kind:** API/design improvement. **Area:** Core component contracts, wrappers, SDK/Host.
**Priority:** Medium. **Effort:** Large.

`ComputationComponent` and its sub-traits have many optional hooks with
default implementations. A wrapper component (such as a checkpointing or proxy
wrapper) that forgets to forward one silently uses the default, so the wrapped
component appears to lack a capability, or a no-op default looks like success.

**What remains:**

- Document which hooks are required, optional or unsupported for each role,
  and the `Send`/`Sync`, serialization and cancellation obligations of the
  asynchronous hooks.
- Replace hand-written forwarding with a capability or delegation design that
  makes a missing forward a compile error or an explicit "unsupported" result.
- Any ABI extension must be explicitly versioned and negotiated, never inferred
  from a trait name or a successful no-op default.
- Add further native services only for a concrete need. Native factories can
  receive only the Host's fixed resource set (bootstrap, QoS channel,
  checkpoint, identity, processing); reuse scoped services rather than pass Rust
  objects or storage pointers across FFI. Native in-place updates
  (`assess`/`apply`), query processing/bootstrap/scheduling/output services and
  non-query scheduling are already negotiated; do not reopen them.
- For `CheckpointedSink`, add scoped progress-store deletion and combined
  inner/wrapper retirement proof. Until then keep its explicit rejections; never
  forward permanent deletion or a batch past the progress owner.

**Example:** `CheckpointedSink` hand-forwards about 30 methods to its inner sink.
A new defaulted hook added to `EnvelopeSink` would not be forwarded unless
someone remembers to add it. Separately, deleting a checkpointed sink fails with
"checkpointed sink cannot delete its inner state without scoped progress-store
deletion".

**Evidence:**

- `drasi-core/lib/src/computation/v1/component.rs:52-311`: traits with defaulted
  hooks.
- `drasi-core/lib/src/computation/v1/consumer_recovery.rs:927` (`CheckpointedSink`),
  `:943`, `:1016`, `:1058` (explicit rejections).
- `drasi-core/components/host-sdk/src/computation/factory.rs:120-170,260-340`:
  the fixed native resource set.
- `drasi-core/components/computation-plugin-abi/src/lib.rs:245-250,324`:
  native `assess`/`apply`.

**Done when:** Wrapped and unwrapped components provide the same advertised
behavior, shown by the capability/delegation matrix in [CW-10](#cw-10), and
unsupported combinations reject before promising successful cleanup.

<a id="cw-73"></a>

### Finish release and migration documentation (CW-73)

**Kind:** Release preparation/documentation. **Area:** Core, Server and consumers.
**Priority:** High before publishing the replacement. **Effort:** Medium to large.

ComputationGraph is a breaking replacement for the current published Drasi
libraries and Server. Users need one guide that tells them what changed and how
to move an existing deployment, and the published install instructions must
match what is actually released.

**What remains:**

- Write one release migration guide covering removed/renamed APIs,
  instance-scoped Server configuration and routes, query inputs and results,
  supported Server/plugin pairings, and explicit backup, reset and rebootstrap
  procedures for old state. Do not restore deprecated aliases or promise
  automatic conversion of incompatible stored data.
- Carry the required query-factory registration, supported storage profiles,
  per-item transport limits ([CW-76](#cw-76)), retained recovery rules and
  reset-baseline behavior into the published package and Server instructions.
- Verify distribution metadata and that matching native libraries are
  available, instead of assuming a local build is an installable plugin.
- Once matching packages are published, update navigation and install snippets
  to that release. Until then keep the truthful source-build instructions
  (component READMEs currently use `drasi-lib = { path = "../drasi-core/lib" }`).
- Fix `drasi-core/docs/README.md:9`, which links to a nonexistent
  `../CONTRIBUTING.md`.
- Give each contract topic in the usage, configuration and reference documents
  one authoritative home and link to it, instead of repeating explanations.
- Generate versions and changelogs with release-plz (`drasi-core/release-plz.toml`),
  never by hand.

**Example:** A user running published Drasi Server with a SQLite-backed
continuous query must be able to follow the guide to back up, install the new
Server and native query plugin, register the query factory, and rebootstrap,
without reading source code.

**Evidence:** `drasi-core/docs/README.md:9`;
`drasi-core/components/computation-plugins/transformers/query/README.md:60-100`
(current registration instructions); `drasi-core/lib/Cargo.toml:18`
(`version = "0.9.3"`).

**Done when:** A reader can build or install the advertised version, choose the
right configuration and safely replace an old deployment using only the
published guidance. Link, docs.rs/package, documented-command (including the
`build-native-network-plugins` Make target) and developer-guide example checks
are [CW-85](#cw-85).

<a id="cw-74"></a>

### Evaluate optional pipe throughput policies (CW-74)

**Kind:** Optional performance capability. **Area:** Core pipes and delivery.
**Priority:** Measurement-led future work. **Effort:** Medium to large.

Pipes currently limit how much data is queued (capacity) but not how fast it
flows, and a consumer handles deliveries in order. Some workloads may benefit
from rate limits or from letting several deliveries complete independently.
Only adopt either if measurements show a need.

**What remains:**

- Evaluate asynchronous rate/burst limits on admission or delivery, kept
  separate from capacity quotas.
- If measured workloads need independently completing in-flight deliveries,
  design a bounded acknowledgement window that advances durable progress only
  through the contiguous completed prefix.
- Leave the existing `receive_batch` contract unchanged.

**Example:** An HTTP sink with a destination limit of 100 requests per second
could use a rate/burst policy instead of failing and retrying. A sink with
independent 200 ms calls could keep 8 in flight; if deliveries 1, 2 and 4
complete, progress advances only to 2.

**Evidence:** `drasi-core/lib/src/computation/v1/pipe.rs:195`,
`qos_pipe.rs:1701`, `retained_pipe.rs:415` (`receive_batch`); no rate or
in-flight window in the pipe implementations.

**Done when:** Each adopted policy enforces its rate/burst or window bound
without changing ordering, shared QoS accounting or required delivery
guarantees. Controlled-clock/failure tests are [CW-39](#cw-39) and measurements
are [CW-40](#cw-40). Reject complexity that measurements do not justify.

<a id="cw-75"></a>

### Extend isolated-host platform and containment capabilities (CW-75)

**Kind:** Optional platform capability. **Area:** instance-host, bridge, Server.
**Priority:** Required before advertising an additional profile. **Effort:** Large.

The instance host, which runs DrasiLib instances as supervised child
processes, works only on Unix and provides no OS-level resource containment.
Its runtime budgets are scheduling and size limits, not a sandbox.

**What remains:**

- If Windows hosting is required, define and implement a Windows
  supervision profile. On non-Unix platforms the supervisor factory is
  registered but rejects use.
- Where a deployment needs it, add explicit kernel CPU/memory/process
  containment (for example cgroups or job objects) and kernel-enforced
  parent-death fencing. In-process unwind handling cannot contain aborts or
  unsafe memory faults.

**Example:** A host configuration with
`require_hard_resource_isolation: true` fails validation with "hard
CPU/memory/security isolation is unsupported; runtime budgets are not an OS
sandbox".

**Evidence:**

- `drasi-core/components/instance-host/src/lib.rs:7-34`: modules are
  `#[cfg(unix)]`; `unsupported.rs` is used elsewhere.
- `drasi-core/components/instance-host/src/config.rs:44-59,131-138`:
  `RuntimeBudget` and the rejected isolation options.

**Done when:** Each adopted hosting/containment profile implements its
ownership and isolation contract and passes [CW-91](#cw-91)'s
platform/network qualification. This adds no remote component-method executor,
distributed transaction promise or automatic remote provisioning.

<a id="cw-76"></a>

### Transfer oversized native query records and envelopes (CW-76)

**Kind:** Transport capability gap. **Area:** Native contracts, ABI, SDK/Host.
**Priority:** High before claiming parity for large individual payloads.
**Effort:** Medium.

A native query can process an item up to 64 MiB, but cannot return any single
result record or envelope larger than 16 MiB when results are read back.
Whole result sets are already paged without a total size limit; the problem
is only one oversized item.

**What remains:**

- Add negotiated, bounded fragmentation and reassembly for individual items,
  reusing the existing codecs and owned transfers.
- Apply it consistently to snapshot, replay, recovery-view and follow reads.
- Preserve schema/identity checks, coherent snapshot/history heads, page order,
  cancellation and activation retirement.
- Do not just raise all limits or keep an unbounded second copy.

**Example:** A query emits a row containing a 20 MiB JSON document. Processing
succeeds, but a snapshot read or replay of that row fails because the encoded
read reply exceeds `MAX_READ_BYTES`.

**Evidence:**

- `drasi-core/components/computation-plugin-abi/src/output_reads.rs:19`:
  `MAX_READ_BYTES = 16 MiB`.
- `drasi-core/components/computation-plugin-abi/src/lib.rs:34`:
  `MAX_MESSAGE_BYTES = 64 MiB`.
- `drasi-core/components/host-sdk/src/computation/output_reads.rs:230`,
  `output_reads/transfer.rs:295`, `output_follow.rs:216`: oversize rejection.

**Done when:** An item valid for processing but larger than 16 MiB can be read
and replayed intact, with a stated end-to-end maximum and allocation bound.
Boundary-size, malformed-fragment and cleanup tests are [CW-41](#cw-41).
Aggregate queue/result memory quotas are [CW-29](#cw-29).

<a id="cw-80"></a>

### Retire the remaining linked query runtime entry points (CW-80)

**Kind:** Breaking-release cleanup. **Area:** Core library, tests, examples and Server.
**Priority:** Medium; after parity evidence. **Effort:** Medium.
**Depends on:** [CW-77](#cw-77) and [CW-79](#cw-79).

Queries normally run through the native query plugin, and there is no automatic
fallback to the in-process (linked) query runtime. The linked runtime is still
reachable through explicit constructors that many tests and examples use. Keeping
two ways to run a query means two paths to maintain and test.

**What remains:**

- Inventory callers of the linked entry points: `ContinuousQueryFactory::default()`
  (a factory with no constructor, which builds `ContinuousQueryTransformer`
  in-process) and `ContinuousQueryTransformer::new`/`new_configured`/`construct`.
  About 90 Rust files in drasi-core and 14 in drasi-server reference them
  (mostly tests and examples, for example `drasi-core/lib/examples/stockroom.rs:119,380`).
- Move each supported caller to the registered native query factory, then
  remove the linked constructors and any dependency/build paths they alone use.
- Keep `ContinuousQueryFactory::with_constructor`, or a renamed equivalent: the
  Host uses it to wrap the native query plugin
  (`drasi-core/components/host-sdk/src/computation/query.rs:154`).
- Replace Server's use of `ContinuousQueryFactory::default()` to look up the
  query implementation identity (`drasi-server/src/computation.rs:335`).
- Keep the Core evaluator, shared query code and behavior assertions. Keep a
  reproducible, pinned comparison baseline before removing executable reference
  fixtures; do not delete tests just to shrink the change.

**Evidence:** `drasi-core/lib/src/computation/v1/query.rs:186`
(`ContinuousQueryTransformer`), `:1756-1942` (`ContinuousQueryFactory`,
`with_constructor`), `:2232-2250` (linked construction when no constructor is
set).

**Done when:** Supported application and test paths no longer need the linked
query runtime, a missing native registration still fails explicitly, and native
behavior coverage is intact. This does not require removing the ordinary
Source/Reaction stack ([CW-63](#cw-63)).

<a id="cw-108"></a>

### Return query result elements as element JSON (CW-108)

**Kind:** Issue. **Area:** Core (computation-contracts), Server.
**Priority:** High for any query that returns a whole node or relation.
**Effort:** Small; the fix is committed, verification remains.

When a native query result is projected to JSON (result envelopes, the
`QueryResult` handed to reactions and Server's results endpoint, graph views),
values without an explicit arm fall through to a catch-all that writes Rust
`Debug` text. A query such as `MATCH (n:Person) RETURN n` therefore returns
`"n": "Element(Node { ... })"` as a string instead of the node's properties.
`ElementMetadata`, `ElementReference`, `ListRange`, `Expression` and `Awaiting`
take the same path.

**Fix (committed in `361d34c3`, not yet run through the workspace gates):**
`query_value_to_json` now has explicit arms: `Element` uses the existing element
JSON conversion (properties plus `$metadata`), `ElementMetadata`,
`ElementReference` and `ListRange` use their `Display` text, and `Expression`
and `Awaiting` stay as explicit text, so no variant reaches a catch-all. The
test `returned_elements_project_to_element_json_not_debug_text` covers node,
metadata and reference projection.

**Evidence:** `drasi-core/components/computation-contracts/src/internal/typed_change/adapters.rs:704-758`
(`query_variables_to_json`, `query_value_to_json` with the `_ =>
format!("{value:?}")` catch-all before the fix); callers in `query_codec.rs:670,715` and
`graph_view.rs:698`.

**Done when:** the workspace gates pass with the fix and its test, and a Server
query returning a node shows the node's properties in its results.

<a id="cw-109"></a>

### Resume a persistent query bootstrapped over a query edge without gaps (CW-109)

**Runtime contract:** A persistent downstream query commits its upstream output
identity with the snapshot watermark and completion marker. On restart its
data connection requests `replay_after` using that identity and its **last
committed logical input sequence**, not the snapshot cut or the last enqueue.
Retained envelopes go through normal processing before buffered live input;
receiving the replay head does not advance the checkpoint.

A retention gap or a different producer epoch reaches startup as a typed `Gap`
or `EpochChanged`. Strict fails without rescanning; `AutoReset` uses the
existing guarded reset policy. A paired source instead restores its connector
handover from opaque session state. That state is not an output cursor.

**Regression coverage:** [Session implementation](../src/computation/v1/graph_bootstrap.rs)
and [loaded query-chain tests](../../components/host-sdk/tests/native_query_plugin/durable_chain.rs).
The `indexed_edge_restart_*` cases use the ordinary data connection without
an application-supplied downstream provider. These consumer-restart cases
are distinct from both the output-resource tests and the process-kill matrix.

**Done when:** a process-kill test of a durable query-to-query chain over edges,
for both the linked and the native query, shows no loss or duplicate after
restart, and an uncoverable gap produces a typed startup reset (or
`IncompleteBootstrap` under strict recovery) that `AutoReset` can repair,
instead of silent continuation or a processing failure. Covered by the chain
matrix in [CW-111](#cw-111).

<a id="cw-110"></a>

### Bootstrap chains through transformers and into sinks (CW-110)

**Kind:** Unsupported capability. **Area:** Core, Host SDK, transformer plugins.
**Priority:** Medium. **Effort:** Medium to large.

Initial data reaches a component only when it has a compatible bootstrap input
connected to a compatible bootstrap output. Several realistic chains have no
such path today (survey of 2026-11-20 at `4385703a`, rechecked against the
current tree for the middleware facts):

- `source → stateless transformer → query`: the query cannot bootstrap from the
  source edge because it does not read the source's data directly, and a
  stateless transformer may expose no bootstrap output.
- `query → middleware`: refused at binding, because the middleware accepts only
  a quiescent bootstrap (`quiescent_bootstrap`, optional) while a query's
  output snapshot completes durably.
- `query → view`: supported. The view initializes from an upstream query
  through the edge or a bound output bootstrap provider, and its output is
  persistent with a persistent processing provider
  (`components/host-sdk/tests/native_query_plugin/durable_chain.rs`,
  `stateful_in_the_middle`).
- Arithmetic has no bootstrap ports; native sinks have none, so a sink never
  receives initial rows as changes, only through snapshot reads.
- `query → geometry`: supported in the same way as the view, through the
  same chain test; geometry also bootstraps from a SQLite provider
  (`components/host-sdk/tests/native_query_plugin/middleware_bootstrap.rs`).
- The optimization and policy transformers were not part of the survey; their
  bootstrap ports need the same review.

**What remains:** decide per combination whether it should be supported
(for example a quiescent re-serve of a durable snapshot for middleware, or a
transformer relay that forwards a bootstrap through itself) or is inherently
unsupported, declare that in the component descriptions, and document the
refusals in the transformer guide. Do not infer support from configuration.

**Evidence:** `drasi-core/components/computation-plugins/transformers/middleware/src/lib.rs:81-82`
(quiescent, optional bootstrap input), `.../middleware/src/output.rs:134`;
`drasi-core/components/host-sdk/src/computation/factory.rs` (Host-added
bootstrap ports for native plugins).

**Done when:** each listed chain either bootstraps end to end in a test or is
refused at binding with a documented reason.

<a id="native-plugins-shared-work"></a>

## Native plugins: shared work

These entries cover work that every native source, initial-data provider and
sink depends on: the SDK lifecycle helpers, the conformance kit, configuration
and secrets, update planning, retry and delivery rules, observability,
documentation and distribution. Plugin-specific follow-ups live under each
plugin's own entry; these entries hold only the shared part.

<a id="cw-96"></a>
<a id="source-x-01"></a>
<a id="sink-x-01"></a>

### Make pause and stop behave the same in every plugin (CW-96, Source X-01, Sink X-01)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

The shared lifecycle helpers are in place. `PauseControl` and `PauseGate`
(`drasi-computation-contracts::pause`, re-exported by the SDK) give an
acknowledged pause: Pause returns once every worker has reached a quiet point,
and an expired or cancelled Pause withdraws the request. `ActivationScope`
owns an activation's workers; its `stop` honours the deadline and keeps
unjoined workers so a retried Stop joins them. The PostgreSQL, MSSQL, MySQL
and SQLite sources and the profiler sink use them, the bridge source refuses
new data with `Busy` while paused ([BR-01](#br-01)), and a Host test proves a
paused host-driven sink receives no `handle` and its in-flight input settles
before Pause returns. Native lifecycle calls refuse an already cancelled scope
before reaching the plugin.

**What remains:**

- Move the SSE sink's local pause flag onto the shared gate (NI-03 in
  [CW-101](#cw-101)). Host-driven sinks (HTTP, gRPC, log) and sources whose
  only work is network admission need no worker gate.
- The conformance kit checks that a paused source refuses new input; it does
  not yet observe that a paused worker-driven source emits nothing.

**Example:** the SSE sink pauses through its own flag; a fix to the shared gate
does not reach it.

**Evidence:** `drasi-core/components/computation-contracts/src/pause.rs`;
`drasi-core/components/computation-plugin-sdk/src/lifecycle.rs` (`ActivationScope`);
`drasi-core/lib/tests/computation_task_execution.rs` (paused sink test);
`drasi-core/components/computation-plugins/sinks/sse/src/component.rs`.

**Done when:** the SSE sink uses the shared gate and the kit fails a
worker-driven source that emits while paused.

<a id="cw-98"></a>
<a id="source-d-1"></a>
<a id="sink-d-1"></a>

### Make Erase work for plugins that own nothing or own shared state (CW-98, Source D-1, Sink D-1)

**Kind:** Issue + Decision. **Priority:** High. **Effort:** Medium.

The Host fails Erase for any factory that does not declare
`erases_owned_state`, and there is no way for a factory to say it owns no
external state. Removing a component with `StateIntent::Erase` therefore fails
for plugins that have nothing to erase.

**What remains:**

- **Decide** the mechanism: (a) a new factory capability "owns no external
  state" that lets the Host complete Erase without calling the plugin, or
  (b) plugins declare `erases_owned_state` and implement a no-op erase.
  **Recommendation:** (a); it is explicit and stops plugins from claiming an
  erase they do not perform.
- Define SDK idempotent-retry semantics for erase: a second Erase after a
  partial first one must finish the job and must not fail on missing objects.
- Add erase-twice and erase-after-failed-start cases to the conformance kit;
  add an erase-twice test for the query transformer.
- For `drasi.network/http-delivery` sinks, erase must also remove the Host
  `DeliveryRunner` progress record.

Per-plugin erase gaps (factories without erase, the bridge sink) belong to
those plugins. The three stored-procedure sinks erase only their own progress
rows, idempotently.

**Example:** removing a log sink with `StateIntent::Erase` fails at the Host
check, although the log sink stores nothing.

**Evidence:** `drasi-core/components/host-sdk/src/computation/proxy.rs:1539-1542`;
`drasi-core/components/computation-plugin-sdk/src/metadata.rs:222`.

**Done when:** the mechanism is chosen and implemented; Erase succeeds for
plugins that own nothing and is idempotent for plugins that own state; and the
conformance kit checks erase twice for every plugin that runs it.

<a id="source-x-02"></a>
<a id="sink-x-02"></a>

### Run the conformance kit through loaded libraries and publish it (Source X-02, Sink X-02)

**Kind:** Qualification. **Priority:** High (P0). **Effort:** Medium.

The kit's suites are generic over a `ConformanceTarget`. `LinkedTarget` wraps a
linked factory; the Host SDK's `LoadedTarget` (feature `test-support`) loads a
built `cdylib` and drives it through the C ABI. `tests/native_conformance.rs`
runs the HTTP source and capture sink through it on both Tokio flavours. The
kit also covers the full `DisposeReason` matrix and dispose after a failed
initialization. Loaded runs accept the ABI's acknowledgment of a repeated,
already completed transition, and verify the component still works.

**What remains:**

- Missing scenarios: stop timeout followed by a successful retry; refusal of a
  stale or forged update plan (the recheck exists only in the ABI export, and
  loaded `apply` is not driven by the kit); a paused worker-driven source
  emitting nothing ([CW-96](#cw-96)); erase twice for plugins that own nothing
  ([CW-98](#cw-98)); a crashed consumer for durable sinks.
- Every native plugin runs the kit, linked and loaded.
- Publish the kit and driver as a supported vendor surface. The Host pipeline
  suite hard-codes Core package names in `plugin_package`, so it cannot run a
  vendor plugin.

The loaded-library run overlaps [CW-41](#cw-41) item 2; that entry owns the
wider ABI contract matrix.

**Example:** the PostgreSQL source passes the kit linked but has never run it
through its built library.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/testing.rs`
(`ConformanceTarget`, `LinkedTarget`);
`drasi-core/components/host-sdk/src/computation/conformance.rs`;
`drasi-core/components/host-sdk/tests/native_conformance.rs`;
`drasi-core/components/host-sdk/tests/native_computation_pipeline.rs:67-76`.

**Done when:** every native plugin passes the kit both linked and loaded on both
Tokio flavours, including the missing scenarios; and a vendor can run the kit
against their own library from published crates with documented steps.

<a id="source-x-03"></a>
<a id="sink-x-09"></a>

### Finish shared configuration, secret and credential support (Source X-03, Sink X-09)

**Kind:** Improvement. **Priority:** High (P1). **Effort:** Medium.

`ConfigOption::secret()` exists and secrets are flattened to top-level fields,
but nested settings are still invisible to configuration forms and some
credential paths are missing. Database factories still mark a whole `settings`
object secret by hand.

**What remains:**

- Describe nested configuration (fields, defaults, conditional requirements)
  in published schemas and `configuration_details`, so forms can render it.
- A redacting `Debug` helper on configuration types. (The rule is in place:
  native plugin configuration keys are lowerCamelCase at every level and every
  nested configuration type has `deny_unknown_fields`; the SDK conformance
  kits' `configuration_round_trip` checks the published keys with
  `testing::ensure_camel_case_configuration`. The stored-procedure sinks'
  `routes`/`defaultTemplate` still reject unknown keys with a hand-written
  shape check, because the shared `reaction-common` `TemplateSpec` flattens
  its extension.)
- Nested secret references (for example HTTP webhook secrets, HT-04 in
  [CW-100](#cw-100)) and proof that object-valued secrets resolve (NI-01 in
  [CW-101](#cw-101)).
- Consumer-wrapper factories reject the `identity` resource, so a durable
  consumer sink cannot use activation credentials (rotation on each start).
  Allow it, or document the alternative.
- Document default, persistence, redaction and rotation semantics, and warn
  against removing credentials from `configuration()`, which breaks
  reconstruction.

**Example:** a plugin that declares one secret object for its whole
configuration shows a form with a single opaque field, with no host, port or
TLS mode, and a pasted literal object is rejected because secret fields
require references. (The MySQL source used to do this; it now declares only
`password` and the TLS client key as secrets.)

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/configuration.rs:221-226`;
`drasi-core/components/host-sdk/src/computation/factory.rs:1373`;
`drasi-core/lib/src/computation/v1/graph/specification.rs:140-160`.

**Done when:** nested settings appear in published schemas; no factory sets
`.secret = true` by hand; consumer sinks can use activation credentials or the
limit is documented; and the semantics are documented in the SDK guide.

<a id="source-x-04"></a>
<a id="sink-x-10"></a>

### Make update planning reachable for real plugin changes (Source X-04, Sink X-10)

**Kind:** Issue. **Priority:** Medium (P1). **Effort:** Medium.

Plugins implement `plan_update`, but the Host adapter refuses to ask most of
them. It rejects every factory that binds its own inputs and every
configuration that contains an unresolved reference, so "supports lifecycle"
does not mean a plugin can be reconfigured.

**What remains:**

- Assess configurations that hold secret references, by comparing references
  rather than resolved values.
- A shared rule for what each change kind returns (`Replace`/`Recreate`,
  `Preserve`/`Reset`/`Rebootstrap`), applied by every plugin's `plan_update`,
  with tests. `Rebootstrap` acceptance is [CW-92](#cw-92); live input
  rebinding is [CW-94](#cw-94).
- Say in the SDK guide which changes a vendor can expect to apply live.

**Example:** changing the PostgreSQL source's `slotName` while its password is a
secret reference fails with "native update assessment requires literal
configuration; unresolved references cannot be assessed", before the plugin
sees the change.

**Evidence:** `drasi-core/components/host-sdk/src/computation/factory.rs:1316-1345`
(`binds_inputs` rejection at 1322-1325), `:1800-1815` (literal configuration);
`drasi-core/lib/src/management/update_planning.rs:516,551-565,601`.

Unverified: whether every managed update reaches this adapter check; DrasiLib
also calls `factory.plan_update` directly.

**Done when:** a plugin with secret references gets its `plan_update` called;
each plugin's plan follows the shared rule and is tested; and the guide lists
the supported live changes.

<a id="source-x-05"></a>
<a id="source-x-06"></a>
<a id="sink-x-05"></a>

### Share one retry, backoff and error-classification policy (Source X-05, Source X-06, Sink X-05)

**Kind:** Improvement. **Priority:** Medium (P1). **Effort:** Medium.

Sink delivery already has one model: a sink classifies a failed delivery with
`DeliveryFailure` (transient, permanent or skipped), and the graph retries a
transient failure in place with bounded backoff. The stored-procedure sinks
make one attempt per call and rely on it. The retries that plugins make
themselves still use separate helpers: network sinks share
`common/network/src/retry.rs` for their own attempts within one delivery, the
PostgreSQL, MySQL and SQL Server sources share `BackoffPolicy` from
`computation-contracts/src/reconnect.rs` for reconnects, the bridge sink retries
with a fixed delay ([BS-01](#bs-01)), and the legacy gRPC code classifies errors
by matching message text (`categorize_error`).

**What remains:**

- Move the retry and backoff policy to a family-neutral crate usable by
  sources, initial-data providers and sinks, and adopt it in each plugin that
  retries.
- A typed error taxonomy (transient, permanent, configuration, authentication)
  that maps onto `DeliveryFailure` for deliveries and onto `LifecycleError`
  kinds for activation failures, replacing string matching such as the legacy
  `categorize_error`.

**Example:** the network sinks, the database sources and the bridge sink each
implement retry delays separately, with different ceilings and jitter.

**Evidence:** `drasi-core/components/computation-plugins/common/network/src/retry.rs`;
`drasi-core/components/computation-contracts/src/reconnect.rs`;
`drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:305-320`;
`drasi-core/components/grpc-common/src/output/send.rs:309`;
`drasi-core/components/computation-plugins/common/storedproc/src/driver.rs:12-16`;
`drasi-core/lib/src/computation/v1/graph/sink_delivery.rs` (the graph's
delivery-failure model).

**Done when:** one policy crate is used by every native plugin that retries,
and errors are classified by type, not message text.

<a id="sink-x-03"></a>
<a id="sink-x-04"></a>
<a id="sink-x-07"></a>

### Apply one delivery contract to all sinks (Sink X-03, Sink X-04, Sink X-07)

**Kind:** Improvement. **Priority:** Medium (P1). **Effort:** Medium.

Sinks use three delivery styles (host-driven `DeliveryRunner`, plugin-owned
durable consumer, and best-effort) without one documented rule for choosing
between them or for idempotency. Vendors writing a database sink have to work
out the delivery contract themselves.

**What remains:**

- Document the choice rule and the duplicate/loss outcome of each style in the
  sink guide, with a database-sink lesson (row mapping, transaction per batch,
  acknowledgement after commit).
- An idempotency-key rule for remote sinks that cannot deduplicate, used by
  future ports.
- Move remaining sinks that fit the host-driven profile onto
  `DeliveryRunner` (plugin parts sit with each plugin).

**Example:** a vendor's sink acknowledges before its database commit; on crash
the change is lost, and nothing in the guide warns against it.

**Evidence:** `drasi-core/components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md`;
`drasi-core/components/computation-plugin-sdk/src/consumer.rs:59`.

**Done when:** the guide states the rule and outcomes; new sinks follow it; and
the kit's crash case checks the declared outcome.

<a id="sink-x-06"></a>

### Set shared limits for batches and large inputs (Sink X-06)

**Kind:** Improvement. **Priority:** Medium (P1). **Effort:** Small.

Sinks handle oversized inputs differently, and some reject a batch by size
alone. The shared rule is: split rather than reject on size, and apply a
timeout to each external command.

**What remains:** write the rule into the sink guide and add a kit scenario with
an oversized batch. Per-plugin adoption is in [CW-44](#cw-44) and each plugin
entry.

**Example:** a 50,000-row result change arrives at a storedproc sink; it should
be split into bounded calls, each with a timeout.

**Evidence:** `drasi-core/components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md`.

**Done when:** the rule is documented and the kit checks it.

<a id="source-x-07"></a>
<a id="sink-x-11"></a>

### Report readiness and metrics from plugins (Source X-07, Sink X-11)

**Kind:** Improvement. **Priority:** Medium (P1). **Effort:** Medium.

Several plugins discard their `ControlSender`, so they never report
not-ready while a connection is down. The SDK now has a metric facility
(`ConstructionContext::plugin_metrics`,
`drasi-core/lib/src/computation/v1/graph/specification.rs:867`, and
`ScopedMetricEmitter`,
`drasi-core/components/computation-contracts/src/telemetry/scoped_metrics.rs:26`;
documented in `drasi-core/components/computation-plugin-sdk/README.md:111-113`),
but no connector plugin uses it yet. Plugin logs already reach the Host
([CW-24](#cw-24)).

**What remains:**

- Use `ControlSender::ready`/`not_ready` in every plugin with an external
  connection.
- Agree shared metric names (events, failures, retries, reconnects, lag) and
  emit them from the connector plugins through `plugin_metrics`, feeding
  [CW-26](#cw-26).

**Example:** the gRPC sink's endpoint goes down; the component still reports
ready because the sink dropped its `ControlSender`.

**Evidence:** `ControlSender` discarded in `Factory::create` at
`drasi-core/components/computation-plugins/sinks/grpc/src/lib.rs:101`,
`.../sources/postgres/src/lib.rs:220`, `.../sources/mysql/src/lib.rs:278`,
`.../sources/mssql/src/lib.rs:164`, `.../sources/http/src/lib.rs:109`,
`.../sources/grpc/src/lib.rs:112`, `.../sources/otel/src/lib.rs:113`,
`.../sources/counter/src/lib.rs:76`, and the stored-procedure sinks
`.../sinks/storedproc-mysql/src/lib.rs:99`, `storedproc-postgres/src/lib.rs:139`
and `storedproc-mssql/src/lib.rs:102`. The stored-procedure sinks report Ready
while their database is down, failing deliveries transiently instead. No
connector under `drasi-core/components/computation-plugins/` calls
`plugin_metrics`.

**Done when:** connected plugins report readiness and emit the agreed metrics,
and a test observes both through the Host.

<a id="source-x-08"></a>
<a id="sink-x-13"></a>

### Remove panics from plugin entry paths and report export errors (Source X-08, Sink X-13)

**Kind:** Issue. **Priority:** Medium (P1). **Effort:** Small.

Plugin `lib.rs` files use `.expect(` while building metadata, and the export
macro turns a failed or panicking definition into a null metadata pointer,
discarding the reason. The loader then reports only that metadata is missing.

**What remains:** make the export macro carry the error text to the Host and
log it; adopt a rule (with a lint or test) of no `expect`/`unreachable!` on
paths called across the ABI. Per-plugin counts are plugin work.

**Example:** a typo in a field name makes `ConfigDefinition` construction fail;
the Host reports a null metadata pointer with no cause.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/lib.rs:75-100`.

**Done when:** a failing definition produces an actionable Host error naming the
plugin and cause, and the rule is enforced.

<a id="source-x-09"></a>
<a id="sink-x-14"></a>

### Correct shared plugin documentation (Source X-09, Sink X-14)

**Kind:** Issue. **Priority:** Medium (P1). **Effort:** Small.

The shared documents now state the current versions: native ABI 0.2.0, which
before 1.0 loads only a library with the same major and minor; wire version 1;
and ordinary ABI 0.19. They also say that native libraries stay loaded for the
life of the process. `computation-plugin-sdk/tests/documented_abi_version.rs`
passed while several documents still quoted old versions, because it:

- reads only five named documents and the files under
  `components/computation-plugins`;
- finds a version only directly after `ABI `, `ABI is ` or `wire version `, so
  it missed "wire version is **2**", "ordinary ABI (0.18" and
  "wire-version-2";
- accepts any `0.x` after `ABI ` as the native version, because the native
  major is 0, so an old ordinary version such as "ABI 0.15" passes.

A version scan also cannot catch a stale rule, such as "binaries built for
another ABI major are rejected". Only some native plugins have `DESIGN.md` and
`CONFIGURATION.md`.

**What remains:**

- Widen the check: compare the native major and minor exactly, check ordinary
  versions separately, accept the phrasings above, and scan every document
  that quotes a version, including the data-design doc. The ordinary
  plugins' own READMEs are excluded; they are removed with the ordinary
  plugins ([CW-63](#cw-63)).
- Require both documents for every native plugin (per-plugin READMEs are plugin
  work).

**Example:** "Native wire version is **2**" passed the check until it was
corrected by hand.

**Evidence:** `drasi-core/components/computation-plugin-sdk/tests/documented_abi_version.rs:36-98`;
`drasi-core/components/computation-plugin-abi/src/lib.rs:28-33,77-83`.

**Done when:** the check fails on each stale form above, and every native
plugin has both documents.

<a id="source-x-10"></a>
<a id="sink-x-15"></a>

### Run every native plugin through named CI jobs (Source X-10, Sink X-15)

**Kind:** Qualification. **Priority:** Medium (P1). **Effort:** Medium.

Most test infrastructure work is already tracked: CI gates in
[CW-85](#cw-85), loaded-library checks in [CW-41](#cw-41), recovery in
[CW-14](#cw-14) and build churn in [CW-87](#cw-87). The shared remainder is
that the Host pipeline test maps only five Core plugins.

**What remains:** replace the hard-coded `plugin_package` map with a manifest
covering all native plugins, and give each database plugin a named CI job that
reports "not run" when its container is unavailable.

**Example:** a new native source cannot be loaded by the pipeline suite without
editing a `match` arm.

**Evidence:** `drasi-core/components/host-sdk/tests/native_computation_pipeline.rs:67-76`.

**Done when:** all native plugins load in the pipeline suite and each has a named
job.

<a id="dx-1"></a>
<a id="dx-5"></a>

### Provide a standalone native plugin starter (DX-1, DX-5)

**Related review findings:** [CR-P13](#cr-p13), [CR-P14](#cr-p14) and
[CR-P15](#cr-p15) describe ordinary packaging counterparts. Their resolution is
separate from the native starter's published-dependency release gate.

**Kind:** Improvement. **Priority:** High (P0). **Effort:** Large.

No native plugin project can be built outside the Core checkout. Counter's
manifest inherits workspace settings, and the developer book's project uses
path dependencies into `drasi-core`.

**What remains:**

- A standalone project with a native source and sink, registry-versioned
  manifests, the export feature, a fake database adapter, and Host SDK
  dependencies only in the test harness.
- One evolving lesson: emit a change, see query output, write a destination
  row, then swap in a real driver.
- Lessons for snapshot plus CDC (handover, checkpoint and resume) and for a
  database sink (mapping and delivery contract, see
  [the delivery entry](#sink-x-03)).
- A release gate that builds the starter from an empty directory using only
  published crates.

**Example:** a vendor copies `sources/counter`; it fails to build because
`version.workspace = true` has no workspace.

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/Cargo.toml`;
`drasi-core/lib/docs/developer-guide/stockroom/Cargo.toml:24-28`;
`drasi-core/components/sources/README.md:8-12`.

**Done when:** the starter builds and passes the conformance kit from an empty
directory with published crates, as a maintained release gate.

<a id="dx-10"></a>

### Support third-party packaging and discovery (DX-10)

**Kind:** Improvement. **Priority:** Medium (P1). **Effort:** Medium.

The packaging tool is Core's `xtask`, which is not published, and the Host
loader only discovers files named `libdrasi_*` or `drasi_*`.

**What remains:** a versioned packaging tool or build image that generates
native OCI metadata and signs artifacts; discovery that does not depend on the
`drasi` filename prefix (or a documented rule); a supported platform matrix.
Server deployment recipes are Server work.

**Example:** a valid `libacme_db.so` in the plugin directory is skipped with a
warning.

**Evidence:** `drasi-core/xtask/Cargo.toml:7`;
`drasi-core/components/host-sdk/src/loader.rs:784`.

**Done when:** a vendor packages, signs and loads a non-`drasi`-named library
with published tools.

<a id="native-sources"></a>

<a id="cw-103"></a>

### Review the native initial-data providers and transformers (CW-103)

**Kind:** Investigation. **Area:** Native plugins. **Priority:** Medium.
**Effort:** Large.

The production reviews covered the native sources and sinks. The native
initial-data providers and transformers have not been reviewed, so their
lifecycle, erase, update planning, configuration, failure handling and
documentation gaps are unknown and untracked.

- Initial-data providers: `http`, `mssql`, `mysql`, `postgres` and `sqlite`
  (`drasi-core/components/computation-plugins/bootstrappers/*`). The
  PostgreSQL provider was only changed to share its source's flat
  configuration and managed slot identity.
- Transformers: `arithmetic`, `middleware`, `query` and `view`
  (`drasi-core/components/computation-plugins/transformers/*`). `query`
  implements update planning; none of the four has `DESIGN.md` or
  `CONFIGURATION.md`.

**Progress:** the providers' declarations and pairing were reworked
(`computation-plugins/bootstrappers/NATIVE-BOOTSTRAP-PROVIDER-GUIDE.md`):
every bootstrap factory has a pure `describe` the Host forwards unchanged;
paired providers (PostgreSQL, MySQL, SQL Server, SQLite coordinated) are
opened by their source's coordinator through a declared protocol checked when
the edge binds; unpaired providers take their element namespace and stream
from the graph relation instead of `source`/`stream` keys; the SQLite file
snapshot declares its element identities and is refused on a source with
other tables or keys; quiescent-versus-durable completion is checked at
binding; queries pass their labels and providers select tables and endpoints
by them; `NativeBootstrapProvider` is split into required
`QuiescentBootstrapProvider` and `DurableBootstrapProvider` hooks.

The transformers now have a standard
(`computation-plugins/transformers/NATIVE-TRANSFORMER-GUIDE.md`: ports, declared
descriptions and keyed inputs, lifecycle, durable output with
`delivery_completed`/`resume_delivery`, bootstrap relay and status,
transactions, management). The middleware and view transformers share one
implementation (`computation-plugins/common/middleware`: pipeline,
configuration options and a constant kind list) instead of two copies, and
declare their kinds (`decoder`, `map`, `parse_json`, `promote`, `relabel`; the
first, third and fourth were previously missing from the native path) as a
constraint on `middleware[].kind` in the published configuration schema,
checked when the graph binds the component. The view adds its insert-only map
restriction to the same schema. The query transformer publishes its middleware
registry's kinds the same way (`ConfigurationConstraint::middleware_declarations`).
The middleware transformer has unit tests for declared kinds, duplicate and
undeclared pipeline names, fan-out and failure latching.

**What remains:** review each plugin against the native sink and source
standards (lifecycle hooks, erase, update planning, configuration and secrets,
retries, tests, documentation) and record the findings as per-plugin entries
here, as the source and sink sections do. Port the container suites that
still construct coordinated providers outside the graph
(`components/computation-plugins/sources/postgres/tests/native_transactions`,
`host-sdk/tests/native_mysql_components.rs`,
`host-sdk/tests/fixtures/native_mysql`, `host-sdk/tests/fixtures/native_mssql`)
to the paired path: create the source's coordinator
(`NativeFactory::create_source_bootstrap`) and open providers with
`create_paired_query_provider`, or wire the provider node into the graph.

Done for configuration: the standalone PostgreSQL snapshot is flat camelCase
with published defaults (PGA-10); the SQL Server, MySQL and PostgreSQL
bootstrap schemas are built from the same option lists as their sources
(`drasi_{mssql,mysql,postgres}_common::…::definition::options`; MSA-6,
MYA-10); the HTTP provider's hand-written key allow-list is gone and the
`drasi_http_common` bootstrap types are camelCase with `deny_unknown_fields`
at every level, refused at `describe` (HTA-12). The query transformer's keys
are camelCase (`outboxCapacity`, `sourceTransactions`, `resetConfiguration`).

**Example:** whether a SQLite initial-data provider stopped mid-snapshot
releases its read transaction within the stop deadline is not known.

**Evidence:** no `DESIGN.md` or `CONFIGURATION.md` in any of these crates.

**Done when:** each provider and transformer has been reviewed and its open
work is listed here.

## Native sources

Each source has its own section. Work shared by every plugin is under
[Native plugins: shared work](#native-plugins-shared-work).

<a id="plugin-source-postgres"></a>
<a id="cw-102"></a>

### PostgreSQL source

`drasi-core/components/computation-plugins/sources/postgres` (with `drasi-core/components/postgres-common` and `drasi-core/components/computation-plugins/bootstrappers/postgres`). Production-ready: real Pause, Drain/Abort stop, managed updates, erase of managed slots, reconnection in every mode, typed failures, activation credentials and schema discovery are implemented; behaviour is in its [DESIGN.md](../../components/computation-plugins/sources/postgres/DESIGN.md) and [CONFIGURATION.md](../../components/computation-plugins/sources/postgres/CONFIGURATION.md). The items below are the remaining follow-ups (formerly CW-102).

<a id="pg-13"></a>

#### Pair the source and its initial-data providers through the coordinator (PG-13)

**Kind:** Improvement. **Priority:** High. **Effort:** Medium. **Status:** Resolved.

The PostgreSQL source now declares `HostServices::bootstrap_coordination` and implements `SourceBootstrapCoordination::create_bootstrap_coordinator` for every mode and declares its coordination protocol (`drasi_postgres_common::replication::coordination`): required by `drasi/postgres-transactions` and `drasi.postgres/changes` with `coordinatedSnapshot: true`, optional for the shared modes. The coordinator opens each query's provider (`PostgresSnapshot`/`PostgresSharedSnapshot`, now in the source crate) from the source's own identity, stream, settings and activation credentials, so managed slots are always named under that source's prefix. The coordinated and shared bootstrap factories take no configuration (`PairedBootstrapFactory`), and the graph refuses a provider on a source that does not declare the same protocol when the edge binds (`lib/tests/computation_bootstrap_binding.rs`). The PostgreSQL container suites under `components/computation-plugins/sources/postgres/tests` still construct coordinated providers outside the graph and need porting to the paired path (see [CW-103](#cw-103)).

<a id="pg-truncate-reset"></a>

#### Handle TRUNCATE with a typed reset (Source D-9 follow-up)

**Related review finding:** [CR-A02](#cr-a02) is this same remaining reset
capability, with the ordinary warning-only path distinguished from native
typed rejection.

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

A publication that publishes TRUNCATE is refused at start. A TRUNCATE of a selected table published later (after `ALTER PUBLICATION`) fails the source with a typed `SourceFailure::Unsupported` before anything of its transaction is published; a TRUNCATE of only unselected tables is ignored. The legacy source tolerated TRUNCATE (and silently diverged). Operators must recreate publications without TRUNCATE, which the default `CREATE PUBLICATION` does not do. The graph has no typed source reset that would let downstream queries rebuild from a fresh snapshot.

**What remains:** a typed source reset in the graph contracts that forces downstream queries to rebuild from a fresh snapshot; emit it on TRUNCATE, then allow publications that publish TRUNCATE.

**Example:** `CREATE PUBLICATION p FOR TABLE orders` (default `publish` includes truncate) is refused until it is recreated `WITH (publish = 'insert, update, delete')`.

**Evidence:** "Unsupported" section of `drasi-core/components/computation-plugins/sources/postgres/DESIGN.md`; `drasi-core/components/postgres-common/src/replication/transaction.rs` (`WalMessage::Truncate`); live test `postgres_runtime_truncate_fails_typed_without_partial_publication`.

**Done when:** a TRUNCATE on a published table produces a reset that rebuilds affected queries, covered by a live test.

<a id="pg-10"></a>

#### Offer opt-in whole-row identity for keyless tables (PG-10, Source D-10)

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

Tables without a usable key are rejected (documented). The legacy source assigned random IDs, which broke updates and deletes. Some users have keyless tables with `REPLICA IDENTITY FULL`.

**What remains:** an opt-in mode that derives a stable element ID from the full row for tables with `REPLICA IDENTITY FULL`, with its duplicate-row limitation documented.

**Example:** a log table with no primary key and `ALTER TABLE events REPLICA IDENTITY FULL` can be selected only after opting in.

**Evidence:** `drasi-core/components/computation-plugins/sources/postgres/DESIGN.md:167-176`.

**Done when:** the opt-in works in a live test, including update and delete, and stays off by default.

<a id="pg-30"></a>
<a id="pg-33"></a>
<a id="pg-34"></a>
<a id="pg-35"></a>

#### Complete PostgreSQL qualification (PG-30, PG-33, PG-34, PG-35)

**Kind:** Qualification. **Priority:** Medium. **Effort:** Medium.

The live suite runs against both the static plugin and the built library, but several claimed behaviours have no test.

**What remains:**

- Run the SDK conformance kit (`SourceConformance`, `drasi-core/components/computation-plugin-sdk/src/testing.rs`) for each factory on both Tokio flavours.
- Live tests: activation credentials through the built library; the refusal of PostgreSQL 12; a fault injected mid-transaction with reconnect in rows and shared modes; SQLSTATE 55006 classification.
- Add the rows and shared modes to the [CW-14](#cw-14) crash matrix.

**Example:** `cargo test -p drasi-computation-source-postgres --test native_transactions -- --ignored` should include a case that kills the walsender between `BEGIN` and `COMMIT` of a rows-mode source and checks that no change is lost or duplicated.

**Evidence:** conformance kit used only by `drasi-core/components/computation-plugins/sources/{http,grpc}/src/tests.rs`; live suite `drasi-core/components/computation-plugins/sources/postgres/tests/native_transactions/` has no activation-credential or version-refusal case.

**Done when:** the kit and the listed live tests pass in a named CI job ([CW-85](#cw-85)).

<a id="pg-observability"></a>

#### Report PostgreSQL readiness and metrics (Source X-07)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small to medium.

The source logs through `log` but reports no metrics, and its factories drop the `ControlSender`, so reconnect attempts and replication lag are invisible to the host.

**What remains:** report retrying/ready status through the `ControlSender`; emit the shared source metric set ([CW-26](#cw-26)): changes, transactions, bytes, LSN lag, reconnects.

**Example:** during a 30-second reconnect loop the instance status stays "running" with no indication it is retrying.

**Evidence:** `_control: ControlSender` unused at `drasi-core/components/computation-plugins/sources/postgres/src/lib.rs:220` (shared by every factory the source registers).

**Done when:** retry status and the metrics appear for a running source and are covered by a test.

<a id="pg-20"></a>

#### Tidy PostgreSQL mode model and metadata (PG-20, Source X-08)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

A `Mode` enum drives configuration and planning, but the component still holds `progress` and `shared` as two `Option`s and bails at run time on an impossible combination. Factory metadata uses `expect`, which is reachable from the FFI exporter.

**What remains:** carry the mode as an enum in the component; build metadata without `expect` (or validate once in a test-covered `LazyLock`).

**Example:** `anyhow::bail!("PostgreSQL source progress binding is invalid")` is reachable only through a construction bug.

**Evidence:** `drasi-core/components/computation-plugins/sources/postgres/src/component.rs:71-72,603`; `drasi-core/components/computation-plugins/sources/postgres/src/lib.rs:83,87`.

**Done when:** the invalid-binding branch and metadata `expect`s are gone.

<a id="plugin-source-mysql"></a>

### MySQL source

`drasi-core/components/computation-plugins/sources/mysql` (engine in `drasi-core/components/mysql-common/src/native`; live suite in `drasi-core/components/computation-plugins/sources/mysql/tests/native_live`). Production-ready: reconnection with a dead-peer watchdog, a DDL policy that ignores unrelated statements, real Pause, Drain/Abort stop within the host deadline, erase, managed updates, flat configuration with only `password` and the TLS client key secret, activation credentials, extended types and typed failures are implemented; behaviour is in its [DESIGN.md](../../components/computation-plugins/sources/mysql/DESIGN.md) and [CONFIGURATION.md](../../components/computation-plugins/sources/mysql/CONFIGURATION.md). The items below remain.

<a id="my-14"></a>

#### Report MySQL readiness to the host and emit metrics (MY-14, Source X-07)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Small to medium.

Readiness changes (streaming, reconnecting, failed), retries, skipped tables and adopted schema changes are only logged. The `ControlSender` is dropped because the factories declare no control interface, so the host never sees the source's readiness, and it emits no metrics through the SDK metric facility.

**What remains:** report readiness through the host (as the SQLite source does through its `ControlSender`); the shared metric set ([CW-26](#cw-26), [Source X-07](#source-x-07)) including binlog lag, through `plugin_metrics`.

**Evidence:** `drasi-core/components/computation-plugins/sources/mysql/src/lib.rs:118-121`; `drasi-core/components/computation-plugins/sources/mysql/DESIGN.md:258-265`.

**Done when:** readiness changes are visible to the host and metrics are emitted under test.

<a id="my-15"></a>

#### Carry GTIDs in MySQL checkpoints (MY-15)

**Related review finding:** [CR-K15](#cr-k15) records the ordinary comparator's
single-lineage assumption; this native failover capability is not an ordinary
ordering fix.

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

GTIDs are used only for the initial start position; checkpoints store file/offset bound to `@@server_uuid`, so a reconnect to a different server fails with `SourceFailure::Unsupported` and a failover needs a new source. This is a new capability, not parity: the legacy source stored one transaction's GTID (`uuid:gno`) as its "GTID set" and resumed with it, which the server treats as the executed set and would have resent almost every retained transaction. Refusing a failover is the safer current behaviour.

**What remains:** store the GTID set with each checkpoint and resume by GTID when the server UUID changes.

**Evidence:** `MySqlStartPosition::Gtid` `drasi-core/components/mysql-common/src/native/mod.rs:146`; server UUID check `drasi-core/components/mysql-common/src/native/source.rs:932`; `drasi-core/components/computation-plugins/sources/mysql/DESIGN.md:267-280` (Unsupported).

**Done when:** a failover test resumes on the new primary without loss.

<a id="my-16"></a>

#### Pair the MySQL source and bootstrapper through the coordinator (MY-16)

**Kind:** Issue. **Priority:** High. **Effort:** Medium. **Status:** Resolved: the transactions, shared-transactions and shared-changes sources with `startFrom: snapshot` declare and implement a coordination protocol (`drasi_mysql_common::native::coordination`); their coordinator opens `MySqlSnapshot` from the source's own settings, binding and activation credentials, and the paired factories take no configuration. A provider on the wrong source is refused when the edge binds. SQL Server has the same pairing (`drasi.mssql/coordinated-changes`, offered by a source that owns a `barrierTable`).

Original problem:

The source and `drasi-core/components/computation-plugins/bootstrappers/mysql` take the same flat settings and share committed progress, but are not paired through the SDK bootstrap coordinator. The provider names its source in a free `source` field and repeats the source's `stream`, `tables`, `tableKeys` and connection settings; nothing checks the pairing at wiring. A provider with `tables: [a, b]` beside a source with `[a]` loads both tables and only then the source refuses to start (binding differs); a wrong `source` records the watermark under another ID and the real source fails forever with "no complete consumer-owned boundary". Compatibility is thus discovered at start, after expensive work. Declare `HostServices::bootstrap_coordination` and implement `SourceBootstrapCoordination` as SQLite does, so the provider receives the source's settings and binding and mismatches are refused when the edge is bound. The SQL Server source and provider have the same gap (their `barrierTable`, `tables` and `source` are duplicated too).

**Evidence:** `drasi-core/components/computation-plugins/sources/mysql/DESIGN.md:277-279`.

**Done when:** coordinated MySQL providers are created through the source's coordinator and mismatches are refused.

<a id="my-38"></a>

#### Give the MySQL bootstrap providers activation credentials (MY-38)

**Kind:** Issue. **Priority:** Medium. **Effort:** Medium.

The MySQL source can take its login from activation credentials, but the bootstrap providers connect with their literal `user` and `password` only. A source that uses an identity provider therefore cannot be paired with a password-less provider. This is the MySQL counterpart of [MS-22](#ms-22) and needs the same SDK and Host change.

**What remains:** the coordinated providers now connect with the paired source's current activation credentials (the source's coordinator shares them, [MY-16](#pair-the-mysql-source-and-bootstrapper-through-the-coordinator-my-16)). The standalone `drasi.mysql/snapshot` still connects with its literal login; pass it activation credentials or document that standalone initial loading needs a literal login. (The MySQL source's erase connects to nothing, since it owns no server-side state, so unlike the PostgreSQL and SQL Server sources it needs no `DisposeContext::credentials`.)

**Evidence:** `drasi-core/components/computation-plugins/bootstrappers/mysql/src/config.rs` (`user`, `password`; no credential context).

**Done when:** a provider authenticates with provider-supplied credentials in a test, or the limit is documented in the source and bootstrapper docs.

<a id="my-17"></a>
<a id="my-18"></a>
<a id="my-19"></a>

#### Split the largest MySQL engine functions (MY-17, MY-18, MY-19, Source X-08)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small to medium.

The factories are table-driven, the native build has one TLS stack (`mysql_async`/rustls only with the `client` feature) and failures are typed `SourceFailure`s. Some engine functions are still long and hard to review.

**What remains:** split `Binlog::decode_inner` (about 215 lines) and `prepare_connection` (about 215 lines); replace `RwLock<Option<JoinHandle>>` worker fields with plain owned fields.

**Evidence:** `drasi-core/components/mysql-common/src/native/binlog.rs:495`; `drasi-core/components/mysql-common/src/native/source.rs:224,616-830`; `drasi-core/components/mysql-common/src/native/snapshot.rs:102`; `drasi-core/components/mysql-common/src/native/standalone_snapshot.rs:30`.

**Done when:** no engine function is longer than a reviewer can follow, and worker handles are plain fields.

<a id="my-36"></a>

#### Decide what an untracked MySQL source does under restored persistent queries (MY-36)

**Kind:** Decision. **Priority:** Medium. **Effort:** Small.

An untracked (volatile) MySQL factory keeps no position: after a restart it starts again at `startFrom` (for example the end of the binary log). When its queries use a persistent index provider, the restart restores the queries' rows instead of taking a new snapshot, so changes made while the graph was stopped are never seen and the restored rows stay stale until the next change to them. Nothing refuses or warns about this pairing.

The live test that covers it was changed to expect this: with the untracked factory the offline update is asserted to be missing after restart, while the coordinated (tracked) factory replays it.

The untracked factories now declare `ProgressPersistence::Volatile` (and the tracked ones durable progress requirements), so the fact is declared. The graph does not act on it: query inputs are unkeyed and no edge check compares a producer's progress persistence with the consuming query's, so the pairing is still accepted.

**What remains:** an owner decision: either document the behaviour as intended for untracked factories (the MySQL DESIGN.md describes it), or add the edge rule (CW-106): a query whose progress is durable declares that its inputs need a producer whose progress is not `Volatile`, and the graph refuses the edge at binding. The rule would apply to every volatile source (HTTP, gRPC, SQLite `changes`, PostgreSQL volatile rows), not only MySQL.

**Example:** stop a graph whose `drasi.mysql/changes` source feeds a RocksDB-backed query, run `UPDATE items SET value='offline' WHERE code=3`, start again: the query still shows the old value, and only a later update reaches it.

**Evidence:** `drasi-core/components/host-sdk/tests/fixtures/native_mysql/parity.rs:799-836` (`manual_keys`, assertion `rows(result)?.contains(&offline) == coordinated`).

**Done when:** the decision is recorded, and either the behaviour is documented with that test as its proof or the pairing is refused with a test.

<a id="my-30"></a>
<a id="my-31"></a>
<a id="my-32"></a>
<a id="my-33"></a>

#### Qualify the MySQL source (MY-30, MY-31, MY-32, MY-33)

**Kind:** Qualification. **Priority:** High. **Effort:** Medium.

Live tests through the in-process wrapper now cover erase twice, a pause longer than server timeouts, reconnection after the server kills replication, DDL on unselected and selected tables, TRUNCATE, large transactions and database-wide selection with unsupported tables and extended types; the Host family suite passes linked and loaded. The plugin crate itself runs only database-free metadata, round-trip, lifecycle and assess tests, without the SDK conformance kit.

**What remains:**

- Run the SDK conformance kit (`SourceConformance`) for every factory, linked and through the loaded library, on both Tokio flavours.
- Fault injection not yet covered: a proxy kill and a half-open connection detected by the dead-peer watchdog.
- Credential rotation through activation credentials across a restart; a duplicate `serverId`; one live test per decoded type (spatial, `VECTOR`, non-UTF-8 character sets beyond latin1).

**Evidence:** `drasi-core/components/computation-plugins/sources/mysql/src/tests.rs`; `drasi-core/components/computation-plugins/sources/mysql/tests/native_live/production.rs`.

**Done when:** these pass and run in a named CI job ([CW-85](#cw-85)).

<a id="plugin-source-mssql"></a>

### SQL Server source

`drasi-core/components/computation-plugins/sources/mssql` (engine in `drasi-core/components/mssql-common/src/native`). One pull-based CDC factory (`drasi.mssql/changes`) with real Pause, Drain/Abort stop, reconnection, managed updates, erase of owned snapshot markers, flat configuration, activation credentials (including Entra ID tokens) and typed failures; behaviour is in its [DESIGN.md](../../components/computation-plugins/sources/mssql/DESIGN.md) and [CONFIGURATION.md](../../components/computation-plugins/sources/mssql/CONFIGURATION.md). It is verified with unit tests against an in-memory database seam and database-free Host checks only: live qualification against a real server is deferred by an owner decision ([MS-30](#ms-30), [CW-78](#cw-78)), so it is not yet qualified for production.

<a id="ms-06"></a>
<a id="ms-20"></a>

#### Add a SQL Server column-exclusion list (MS-06, MS-20)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

No column type rejects a table any more, and decimals keep their precision, but every captured column is always read, including bulky `xml`, `varbinary(max)` or `nvarchar(max)` columns a query never uses.

**What remains:** an optional per-table column-exclusion list that leaves excluded columns out of change and bootstrap reads, refusing exclusion of key columns.

**Evidence:** `drasi-core/components/computation-plugins/sources/mssql/DESIGN.md:285-294` (Known limitations).

**Done when:** an excluded column is neither read nor emitted, in a test, and excluding a key column is refused.

<a id="ms-15"></a>

#### Verify split-update and command-ID grouping on a real server (MS-15)

**Kind:** Investigation. **Priority:** Medium. **Effort:** Small.

The pager fails a group with more than two images or an unpaired before/after image with a typed `Unsupported` error. Whether SQL Server emits split updates (delete + insert for key changes) or several `__$command_id` rows for one LSN/seqval in practice has not been checked, and the SQL text and commit-time conversion have never run against a real server.

**What remains:** reproduce on a real server (part of [MS-30](#ms-30)), then decode those cases instead of failing.

**Evidence:** `drasi-core/components/computation-plugins/sources/mssql/DESIGN.md:114-121`; `drasi-core/components/mssql-common/src/native/paging.rs`.

**Done when:** primary-key updates and multi-statement transactions stream correctly in a live test.

<a id="ms-16"></a>
<a id="ms-17"></a>
<a id="ms-18"></a>
<a id="ms-19"></a>

#### Report SQL Server readiness to the host and emit metrics (MS-16, MS-17, MS-18, MS-19, Source X-07)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small to medium.

The engine sits behind a database seam with typed failures, but readiness, reconnect attempts and held consumers are only logged: the factory drops its `ControlSender`, and it emits no metrics.

**What remains:** readiness and reconnect status through the host; metrics ([CW-26](#cw-26), [Source X-07](#source-x-07)) such as poll lag and page sizes, through `plugin_metrics`.

**Evidence:** `drasi-core/components/computation-plugins/sources/mssql/src/lib.rs:132-138`; `drasi-core/components/computation-plugins/sources/mssql/DESIGN.md:278-283`.

**Done when:** status changes are visible to the host and metrics are emitted under test.

<a id="ms-22"></a>

#### Give the SQL Server bootstrap providers activation credentials (MS-22)

**Kind:** Issue. **Priority:** Medium. **Effort:** Medium.

The source can take its login from activation credentials, including a Microsoft Entra ID token, but bootstrap factories receive no activation credentials. The paired providers therefore always need a literal `user` and `password` (a secret reference), and a provider configured without them is refused.

**What remains:** the coordinated `drasi.mssql/bootstrap` provider is now opened by the source's coordinator and authenticates with the source's current activation credentials, including an Entra ID token. The standalone `drasi.mssql/snapshot` still needs a literal SQL login. (The source's own erase already connects with the freshly resolved credentials of the erasing dispose, `DisposeContext::credentials`, so a source rebuilt after a restart erases its markers without starting.)

**Example:** an Azure SQL database with SQL authentication disabled can stream changes through the source with an Entra ID token, but its initial data cannot be loaded.

**Evidence:** `drasi-core/components/computation-plugins/bootstrappers/mssql/src/lib.rs:198-199`; `drasi-core/components/computation-plugins/bootstrappers/mssql/README.md:20-22`.

**Done when:** a provider authenticates with provider-supplied credentials in a test, or the limit is documented in the source and bootstrapper docs.

<a id="ms-30"></a>
<a id="ms-31"></a>
<a id="ms-32"></a>

#### Qualify the SQL Server source (MS-30, MS-31, MS-32)

**Kind:** Qualification. **Priority:** High. **Effort:** Large. **USER-DEFERRED.**

No case has run against a real SQL Server ([CW-78](#cw-78)); an owner deferred live qualification. The live fixture is kept compiling and current: it runs against an explicitly approved isolated database (`DRASI_MSSQL_TEST_MODE=external`) or an owned container (`DRASI_MSSQL_TEST_MODE=podman`, which runs under amd64 emulation on arm64 Macs).

**What remains:** run the Host and Server real cases; port the legacy integration suite ([CW-41](#cw-41)); the SDK conformance kit through the loaded library; live tests for column types, split updates ([MS-15](#ms-15)), commit times across a DST change, least privilege, orphan markers, reconnection, backlog performance, retention-gap and binding-change errors; check element labels match the legacy `normalize_table_label` output.

**Evidence:** `drasi-core/components/host-sdk/tests/fixtures/native_mssql/README.md`; `drasi-core/components/host-sdk/tests/native_mssql_components.rs:487-502` (ignored); `drasi-core/components/computation-plugins/sources/mssql/DESIGN.md:15-22`.

**Done when:** these pass in a named CI job against a real server.

<a id="plugin-source-sqlite"></a>

### SQLite source

`drasi-core/components/computation-plugins/sources/sqlite` (engine in `drasi-core/components/sqlite-common/src/native`; in-process wrapper `drasi-core/components/sources/sqlite/src/native.rs`). Production-ready: four factories that own the database connection (REST or Rust handle writes, hook-based capture, optional journal) with source/bootstrap coordination, real Pause, a two-phase stop, erase of journal tables, managed updates, published defaults, `initSql` and an authenticated `/api/sql`, and a hardened REST API; behaviour is in its [DESIGN.md](../../components/computation-plugins/sources/sqlite/DESIGN.md) and [CONFIGURATION.md](../../components/computation-plugins/sources/sqlite/CONFIGURATION.md). The items below remain.

<a id="sq-05"></a>

#### Report `path: null` with the in-memory message (SQ-05)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

The SQL and DDL access decision is implemented (`initSql`, `/api/sql`, and refusal of an in-memory loaded source without either). A literal `path: null` is still refused by the Host's field type check before the plugin sees it, so the user gets a type error instead of the plugin's in-memory guidance.

**What remains:** accept `null` for `path` in the published schema so the plugin's message applies, or document the type error.

**Evidence:** `drasi-core/components/computation-plugins/sources/sqlite/DESIGN.md:160-169`; `drasi-core/components/host-sdk/tests/fixtures/native_sqlite/host_components.rs:1113`.

**Done when:** `path: null` produces the same refusal as an absent `path`, or the difference is documented.

<a id="sq-08"></a>

#### Rate-limit the SQLite REST API (SQ-08)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small to medium.

The REST listener has bearer authentication, TLS, body and time limits and paging, but no request rate or connection limit.

**What remains:** rate and connection limits, preferably from the shared network runtime ([plugin-source-network](#plugin-source-network)).

**Evidence:** `drasi-core/components/computation-plugins/sources/sqlite/DESIGN.md:362-370` (Known limitations).

**Done when:** a test shows excess requests refused with `429` and connections bounded.

<a id="sq-09"></a>
<a id="sq-12"></a>

#### Emit SQLite metrics (SQ-09, SQ-12, Source X-07)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

Readiness is reported through the `ControlSender`, but counters (commits, rollbacks, journal depth, REST requests) are not exported; it does not use the SDK metric facility yet.

**What remains:** emit those counters once [Source X-07](#source-x-07) and [CW-26](#cw-26) provide the facility.

**Evidence:** `drasi-core/components/computation-plugins/sources/sqlite/DESIGN.md:344-351`.

**Done when:** counters are emitted under test.

<a id="sq-10"></a>
<a id="sq-11"></a>

#### Type SQLite source errors (SQ-10, SQ-11, Source X-08)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

Progress preparation is shared with the in-process wrapper and the coordinator's checks are collapsed, but apart from the journal, failures are prose `anyhow` errors rather than typed `SourceFailure`s.

**What remains:** typed failures for schema, position and configuration errors, as the PostgreSQL and MySQL sources have.

**Evidence:** `SourceFailure` is used only in `drasi-core/components/sqlite-common/src/native/journal.rs`.

**Done when:** callers can tell a permanent failure from a transient one by type, with tests.

<a id="sq-30"></a>
<a id="sq-31"></a>

#### Qualify the SQLite source (SQ-30, SQ-31)

**Kind:** Qualification. **Priority:** Medium. **Effort:** Medium.

The linked plugin runs the SDK conformance kit on both Tokio flavours, and tests cover the stop scenario, Drain and Abort, Pause, erase twice, `initSql`, `/api/sql` authentication, REST limits and paging, and token rotation.

**What remains:** the conformance kit through the loaded library; cold reconstruction with undrained persistent input-pipe frames (not a qualified guarantee today); the journal in the [CW-14](#cw-14) crash matrix; the [CW-82](#cw-82) mixed pipeline.

**Evidence:** `drasi-core/components/computation-plugins/sources/sqlite/src/tests.rs`; `drasi-core/components/computation-plugins/sources/sqlite/DESIGN.md:362-370`.

**Done when:** these run in a named CI job ([CW-85](#cw-85)).

<a id="plugin-source-network"></a>
<a id="cw-100"></a>

### Shared network source runtime

`drasi-core/components/computation-plugins/common/network` (`SourceRuntime`/`Ingress`, shared by the HTTP and gRPC sources). Pause gating of data admission (producer metadata stays available, also from the host while the source is parked), StopMode-aware stop (Drain lets in-flight durable commits finish and answers them; Abort interrupts them as uncertain), back-pressured durable admission (`NativeAdmission::admit_wait`, with the committed prefix of a non-atomic batch always reported), authentication, TLS, rate, connection and handshake limits, managed updates and server-sequenced durable admission are implemented; the items below remain.

<a id="net-08"></a>
<a id="source-d-12"></a>

#### Erase host-owned durable admission state on removal (NET-08, Source D-12)

**Kind:** Investigation. **Priority:** Medium. **Effort:** Small to medium.

A durable HTTP or gRPC source writes through a host-owned QoS channel: a producer registry entry named by the source's component UUID, plus journal records. Both sources declare `erases_owned_state` with a no-op erase because they own nothing themselves, so this state disappears on removal only if the Host deletes it. No Host code that deletes a removed source's producer registration or journal was found.

**What remains:**

- Confirm whether removing a durable HTTP or gRPC source with `StateIntent::Erase` deletes its producer registration and journal records.
- If not, add Host-side erasure owned by the Host (the plugin cannot reach it), make it idempotent, and test it through the loaded library.
- Apply the same answer to the bridge source's durable journal ([BR-03](#br-03)).

**Example:** add a durable HTTP source, post events, remove it with erase, then add a new source with the same UUID. If the old producer entry survives, `producer_status` resumes from the old head instead of zero.

**Evidence:** `drasi-core/components/computation-plugins/common/network/src/durable.rs:221` (implicit producer named by the source UUID); `drasi-core/lib/src/computation/v1/qos_pipe/admission.rs:346` (`retire_producer` exists, but no dispose path calls it for a removed source); [CW-98](#cw-98) for the `erases_owned_state` rule.

**Done when:** a loaded-library test removes a durable HTTP and a durable gRPC source with erase and shows no producer entry or journal record remains, and a second erase succeeds.

<a id="net-10"></a>

#### Bound the nesting depth of network input (NET-10)

**Kind:** Issue. **Priority:** High (an over-deep body can overflow a plugin thread's stack and abort the process). **Effort:** Medium.

Request size is capped (`maxMessageBytes`, Content-Length pre-check, 413), but nesting depth is not. A small body with deep nesting therefore passes the size check and is then converted and dropped recursively.

**What remains:**

- XML webhook bodies and HTTP bootstrap responses: add a depth limit to the shared explicit-stack parser (`http-common/src/content.rs`); it currently pushes one frame per element with no bound.
- JSON and YAML webhook bodies and standard HTTP routes: set and document an explicit depth limit instead of relying on library defaults.
- gRPC: nested protobuf `Struct`/`ListValue` properties are converted to JSON text by a recursive function with no depth check; bound it explicitly instead of relying on `prost`'s decode limit.
- Reject over-deep input with a 400/`INVALID_ARGUMENT` for that request only, and test each format.

**Example:** a 1 MiB XML webhook body of about 150,000 nested `<a>` elements is under a `maxMessageBytes` of 2 MiB, parses without error, then recurses once per level while being converted and dropped.

**Evidence:** `drasi-core/components/http-common/src/content.rs` (`xml_to_json`); `drasi-core/components/computation-plugins/common/network/src/wire.rs:196-222`. Unverified: the effective JSON/YAML (`serde_json`/`serde_yaml`, 128) and protobuf (`prost`, 100) defaults. The general decoder audit (contracts, codecs, MessagePack, safe drop) stays in [CW-19](#cw-19).

**Done when:** each HTTP and gRPC input format has a documented depth limit, and a test per format shows over-deep input fails that request without affecting the source.

<a id="net-11"></a>

#### Add an optional total in-flight byte budget (NET-11)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small to medium.

Memory held by queued input is bounded only by `ingressCapacity × maxMessageBytes` per source. With large messages that product can be far larger than an operator expects, and there is no single cap across concurrent requests.

**What remains:** add an optional per-source byte budget for admitted but not yet delivered input, reject with 503/`RESOURCE_EXHAUSTED` when it is full, and document it in both sources' `CONFIGURATION.md`. The cross-component memory work is [CW-29](#cw-29).

**Example:** `ingressCapacity: 1024` with `maxMessageBytes: 16777216` allows up to 16 GiB queued; `maxInFlightBytes: 268435456` would cap it at 256 MiB.

**Evidence:** `drasi-core/components/computation-plugins/common/network/src/config.rs` (no total-bytes field); `drasi-core/components/computation-plugins/sources/http/CONFIGURATION.md`.

**Done when:** the budget is configurable, enforced across concurrent requests, tested, and documented.

<a id="net-06"></a>
<a id="net-12"></a>

#### Measure and, if needed, parallelize admission throughput (NET-06, NET-12)

**Kind:** Investigation. **Priority:** Medium. **Effort:** Medium.

Both admission paths serialize requests per source. Volatile admission holds the sequence lock while it reserves queue space for every event in a batch, so one slow batch blocks all other requests. Implicit durable admission commits one request at a time per source, so durable throughput is limited to one journal commit round trip at a time.

**What remains:** benchmark both paths with concurrent clients; if the lock is the limit, reserve capacity before taking the sequence lock (volatile) and pipeline consecutive durable sequences while keeping their order and at-least-once semantics.

**Example:** 32 clients posting single events to a durable HTTP source complete at the rate of one commit at a time, regardless of journal or CPU headroom.

**Evidence:** `drasi-core/components/computation-plugins/common/network/src/ingress.rs:358-362`; `drasi-core/components/computation-plugins/common/network/src/durable.rs:220-221`.

**Done when:** measured throughput for both paths is recorded, and either the serialization is removed with order-preservation tests or the measurement shows it is not the bottleneck.

<a id="net-07"></a>
<a id="net-14"></a>

#### Report readiness and recovery and publish ingress metrics (NET-07, NET-14)

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

Listener death is already reported: the listener task catches a panic or error, cancels the activation, reports the failure to the graph through `report_failure` in durable mode, and makes `next()` return the error in volatile mode. What is missing is readiness and recovery reporting, because both factories drop the `ControlSender`, and metrics, so operators cannot see request rates, rejections or queue pressure.

**What remains:**

- Keep the `ControlSender` and report readiness (listener bound) and recovery through it.
- Publish metrics using the shared names from [CW-26](#cw-26): requests, admitted events, rejections by kind (auth, rate limit, size, paused), rate-limited clients, queue occupancy and durable commit latency.
- Check that `diag!` records from the listener tasks carry the component scope, so they reach the component's log endpoint ([CW-24](#cw-24)).

**Example:** a host cannot tell when a volatile HTTP source's listener is bound and accepting, other than by probing the port.

**Evidence:** `drasi-core/components/computation-plugins/sources/http/src/lib.rs:109` and `drasi-core/components/computation-plugins/sources/grpc/src/lib.rs:112` (`_control`); `drasi-core/components/computation-plugins/common/network/src/ingress.rs` (`SourceRuntime::start`, failure reporting); `drasi-core/components/computation-plugins/common/network/src/lib.rs` (`diag!`).

**Done when:** readiness and recovery appear in component status, and the metric set is emitted and documented for both sources.

<a id="plugin-source-http"></a>

### HTTP source

`drasi-core/components/computation-plugins/sources/http` (`drasi.network/http-source`, dynamic and Rust hosting). Lifecycle, management, authentication, TLS, schema publication and transparent durability are implemented, with [DESIGN.md](../../components/computation-plugins/sources/http/DESIGN.md) and [CONFIGURATION.md](../../components/computation-plugins/sources/http/CONFIGURATION.md); webhook secret handling, schema publication details and test ports remain.

<a id="ht-04"></a>

#### Let webhook secrets use secret references and harden signatures (HT-04)

**Kind:** Issue. **Priority:** Medium. **Effort:** Medium.

Webhook HMAC and bearer secrets can only name environment variables. The Host resolves secret references only in top-level configuration fields, and webhook auth is nested under `webhooks.routes[].auth`, so a webhook secret cannot come from a secret store the way `authToken` can. Signatures also have no replay protection, and HMAC-SHA1 cannot be disabled.

**What remains:**

- Let webhook secrets be secret references, either through a flattened top-level secret map (for example `webhookSecrets: { github: <secret ref> }` referenced by name from each route) or nested-path references in the Host resolver (Source D-2 chose flattening for the other fields).
- Add an optional timestamp-tolerance check for signed requests, so a captured request cannot be replayed later.
- Add an option to refuse `hmac-sha1` signatures.

**Example:** `"auth": { "bearer": { "tokenEnv": "GITHUB_HOOK_TOKEN" } }` works only if the Server process has `GITHUB_HOOK_TOKEN` set; it cannot point at a secret store entry.

Secrets named by environment variable are already resolved once per accepted configuration (construction, assess and apply), so a missing variable refuses the configuration or update; this item is only about secret-store references and signature hardening.

**Evidence:** `drasi-core/components/http-common/src/webhook/config.rs` (`secret_env`, `token_env`); `drasi-core/components/http-common/src/webhook/auth.rs` (`ResolvedAuth::resolve`; SHA1 always accepted); `drasi-core/components/computation-plugins/sources/http/src/webhook.rs` (`Webhooks::new`).

**Done when:** a webhook secret can be a secret reference resolved by the Host, replay tolerance and SHA1 refusal are configurable and tested, and `CONFIGURATION.md` documents them.

<a id="ht-08"></a>

#### Publish the webhook schema and the bound listener address (HT-08)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

Tools that read the factory schema see `webhooks` only as an opaque `Object`, so they cannot validate or render routes. A listener configured with `port: 0` reports its chosen port only in an `Info` log line, which a loaded plugin drops because `diag!` forwards only warnings and errors to stderr.

**What remains:** publish the nested `webhooks` schema (routes, auth, mappings, CORS, error behaviour); expose the bound address through health or status so a caller of a loaded library can discover it.

**Example:** a test harness starts the loaded HTTP source with `"port": 0` and has no way to learn which port to post to.

**Evidence:** `drasi-core/components/computation-plugins/sources/http/src/config.rs:118`; `drasi-core/components/computation-plugins/common/network/src/ingress.rs:572-574,645-651`; `drasi-core/components/computation-plugins/common/network/src/lib.rs:44-53`.

**Done when:** the factory schema describes every webhook field, and the bound address is available outside the process log.

<a id="ht-09"></a>
<a id="ht-30"></a>

#### Port the webhook test suites and cover the remaining paths (HT-09, HT-30)

**Kind:** Qualification. **Priority:** Low. **Effort:** Medium.

The ordinary HTTP source's webhook end-to-end and integration suites test only the ordinary plugin. The native webhook handler and the durable HTTP adapter have no unit tests of their own. Loaded-library tests now cover durable webhooks across a restart with explicit-session retry deduplication, durable pause (metadata answered, data refused without commit, sequence resumed), graceful stop with in-flight commits, and back-pressure; the volatile refusal of session headers has a crate test.

**What remains:**

- Port `webhook_e2e.rs` and `webhook_integration.rs` to native construction.
- Add unit tests for each `errorBehavior` in `webhook.rs` and for the `durable_http.rs` error-to-status mapping.
- Test `effectiveFrom` being required for explicit-session webhook requests.
- Add a CORS preflight test on the fallback (unmatched) route; the existing preflight test covers a configured webhook route.
- Loaded-library conformance and rejection-has-no-graph-effect coverage are in [CW-41](#cw-41).

**Example:** a webhook configured with `errorBehavior: "accept_and_log"` that receives an unmappable body should return 200 and emit nothing; no native test asserts this.

**Evidence:** `drasi-core/components/sources/http/tests/webhook_e2e.rs`, `webhook_integration.rs`; `drasi-core/components/computation-plugins/sources/http/src/webhook.rs`, `durable_http.rs` (no tests); `drasi-core/components/host-sdk/tests/native_network/http_family.rs:168-175`.

**Done when:** the ported suites and new unit tests pass against the native source in CI.

<a id="plugin-source-grpc"></a>

### gRPC source

`drasi-core/components/computation-plugins/sources/grpc` (`drasi.network/grpc-source`, dynamic and Rust hosting). Lifecycle, management, authentication, TLS, stream limits and transparent durability are implemented, with [DESIGN.md](../../components/computation-plugins/sources/grpc/DESIGN.md) and [CONFIGURATION.md](../../components/computation-plugins/sources/grpc/CONFIGURATION.md); health probing and test coverage remain.

<a id="gr-06"></a>
<a id="gr-08"></a>

#### Decide how health probes authenticate (GR-06, GR-08)

**Kind:** Decision. **Priority:** Low. **Effort:** Small.

The source's own `HealthCheck` RPC sits behind the same bearer-token interceptor as data RPCs, unlike HTTP `/health`, so an unauthenticated liveness probe fails when a token is set. There is also no standard `grpc.health.v1` service, which Kubernetes and most gRPC tooling expect.

**What remains:** decide whether `HealthCheck` bypasses the token; decide whether to add `grpc.health.v1` (requires approving the `tonic-health` dependency); implement and document the result.

**Example:** a Kubernetes `grpc` liveness probe against a source with `authToken` set fails, because it calls `grpc.health.v1.Health/Check`, which does not exist, and cannot send a bearer token.

**Evidence:** `drasi-core/components/computation-plugins/sources/grpc/src/component.rs:439-466,520-521`; `drasi-core/components/computation-plugins/sources/grpc/src/tests.rs:277-281`.

**Done when:** the decision is documented in `CONFIGURATION.md` and a test probes health without credentials if that is the chosen behaviour.

<a id="gr-07"></a>
<a id="gr-30"></a>

#### Complete gRPC input and durable-mapping tests (GR-30, GR-07)

**Kind:** Qualification. **Priority:** Low. **Effort:** Medium.

Malformed, oversize and deeply nested `Struct` input is not tested, and the mapping from durable admission errors to gRPC status codes has no tests, so a change to either could silently alter what clients see.

**What remains:**

- Tests for malformed, oversize and deeply nested `Struct` input (depth limit in [NET-10](#net-10)).
- A table test for the `durable_grpc` admission-error to status-code mapping.
- Native gRPC in the [CW-36](#cw-36) completion profile; loaded-library conformance is in [CW-41](#cw-41).
- Cleanup: deduplicate the unary admission handlers; optionally add throttled stream progress for legacy clients.

**Example:** a durable `Busy` must reach the client as `UNAVAILABLE` (retryable), not `INTERNAL`; nothing asserts this.

**Evidence:** `drasi-core/components/computation-plugins/sources/grpc/src/durable_grpc.rs:91-260` (no tests); `drasi-core/components/computation-plugins/common/network/src/wire.rs:196-222`.

**Done when:** the input and error-mapping tests pass and native gRPC is in the CW-36 profile.

<a id="plugin-source-bridge"></a>

### Bridge source

`drasi-core/components/computation-plugins/sources/bridge` (`drasi.bridge/source`) with `common/bridge`. A plugin-SDK source (`NativeSource`, `source_admission`) exported by its own library and linked statically by `instance-host`; TLS comes from its `tls` configuration and seal/drain are served on the link's control lane. It has no `DESIGN.md` or `CONFIGURATION.md`. Its hostile-input codec limits, mTLS leaf pinning, bounded queues and owned workers are in place; lifecycle, observability, hosting and documentation work remains. Sink-side bridge work is in the bridge sink section (BS-NN).

<a id="br-01"></a>

#### Gate or refuse Pause in the bridge source (BR-01)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

Pause now refuses new data with a retryable `Busy` and returns once no
admission is in flight; receipts and control requests stay live, and Resume
reopens admission.

**What remains:**

- Define how long a sender may retry against a paused receiver; the sink's fixed retry count and delay fail the sender during a long pause (shared with BS-01).

**Example:** a managed update pauses a bridge source for longer than the sender's retry budget, and the sender fails.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/receiver.rs` (`Receiver::pause`, `admit`); `drasi-core/components/computation-plugins/common/bridge/tests/bridge/integration.rs` (`paused_receiver_refuses_new_data_as_busy_and_keeps_control_live`).

**Done when:** a test shows no admission after Pause and resumed flow after Resume (or an `unsupported` refusal), and a sender rides out a pause within its documented budget.

<a id="br-02"></a>
<a id="br-09"></a>

#### Honour StopMode, start context and admission readiness (BR-02, BR-09)

**Kind:** Issue. **Priority:** High. **Effort:** Medium.

Stop ignores the requested `StopMode` and the context deadline: it cancels the receiver and joins workers with the link's own timeout. Cancellation drops a data connection mid-commit, which can fence a durable channel. Start ignores cancellation and its deadline and does not wait for the admission mailbox to become active, which `lib/AGENTS.md` requires.

**What remains:**

- Drain: finish the in-flight frame's admit and receipt within `ctx.deadline()`; Abort: keep the current prompt cancel. On timeout keep ownership so stop can be retried.
- Start: observe `ctx.cancellation()`/`ctx.deadline()` across every await, and `wait_active` on the admission mailbox before reporting ready.
- Durable mode: wait for admission capacity (`admit_wait`) instead of failing fast with `Busy`; add jittered exponential backoff on the sender side (shared with BS-01).

**Example:** stopping a durable source while a frame is being committed drops the connection; the next start finds the journal fenced.

**Evidence:** `drasi-core/components/computation-plugins/sources/bridge/src/component.rs:133-259`; `drasi-core/components/computation-plugins/common/bridge/src/receiver.rs:888-904,553-556`; `drasi-core/lib/AGENTS.md:75`.

**Done when:** tests show Drain completes an in-flight frame without fencing the journal, Abort cancels promptly, a cancelled start leaves nothing running, and start waits for mailbox readiness.

<a id="br-03"></a>

#### Make erase and re-creation of a durable link work (BR-03)

**Kind:** Issue. **Priority:** High. **Effort:** Medium.

The source refuses `Erase` disposal, so removal with erase always fails. Its durable journal holds one producer keyed by the link contract fingerprint, so a re-created link with a different contract fails with "receiver journal is already bound to another bridge contract", and the refusal is a prose error.

**What remains:** either own the journal and erase the producer idempotently on `Erase`, or document that reprovisioning needs a fresh journal; replace the prose refusal with a typed error. Align with Host erasure of admission state ([NET-08](#net-08)), the erasing-disposal rule in [CW-98](#cw-98) and the bridge sink erase (BS-04).

**Example:** remove a durable link with erase, change `limits.timeout_ms`, and re-add it against the same journal: start fails with the "already bound" error.

**Evidence:** `drasi-core/components/computation-plugins/sources/bridge/src/component.rs:265-274`; `drasi-core/components/computation-plugins/common/bridge/src/receiver.rs:293-302,350-364`.

**Done when:** erase removes the link's producer state idempotently (or the documented refusal rule applies), a re-created link starts cleanly, and an erase-twice test passes.

<a id="br-04"></a>

#### Log bridge failures and surface link status (BR-04)

**Kind:** Issue. **Priority:** High. **Effort:** Medium.

The bridge source logs nothing. Accept and handshake failures are swallowed and retried every 10 ms, so a persistent error such as `EMFILE` spins at about 100 Hz with no trace, and `LinkStatus` is not visible outside the process.

**What remains:** structured, rate-limited logs for accept and handshake failures, generation changes, `Busy`, `AcceptanceUnknown` and worker exits; back off on repeated accept errors; surface `LinkStatus` through graph diagnostics or component status.

**Example:** a receiver that has run out of file descriptors shows as running, logs nothing, and uses a CPU core retrying `accept`.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/receiver.rs:871-904`.

**Done when:** each listed event produces a log record, accept errors back off, and link status is readable through the Host.

<a id="br-07"></a>
<a id="br-08"></a>

#### Separate local settings from the link contract and publish a full schema (BR-07, BR-08)

**Kind:** Issue. **Priority:** Medium. **Effort:** Medium.

Every limit, including local settings such as `timeout_ms`, `retry_*` and `queue_capacity`, is part of the link contract fingerprint, and peers must match it exactly. Tuning a local timeout therefore breaks the link. The factory schema lists field names only, with no descriptions, defaults or enum constraints; `delivery` and `profile` use different casing; the source has no management or update planning; and `receiver_endpoint` duplicates the graph-assigned component ID.

**What remains:**

- Split `Limits` into wire-contract fields and local settings; local settings become InPlace or Restart updates, contract changes Recreate (refused when the peer would diverge). Shared with BS-05/BS-12.
- Implement `Factory::assess` and `specification_from_config`; publish a full nested schema with descriptions, defaults and enum constraints, and make `delivery`/`profile` casing consistent.
- Remove `receiver_endpoint` and use the graph-assigned ID.

**Example:** raising the receiver's `limits.timeout_ms` from 5,000 to 10,000 makes the sender's Hello fail with "bridge protocol, identities, schema, namespace or delivery contract differs".

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/config.rs:168-181,372,385-410`; `drasi-core/components/computation-plugins/common/bridge/src/factory.rs:26-52`; `drasi-core/components/computation-plugins/common/bridge/src/config.rs:21,34`.

**Done when:** changing a local setting keeps the link up, the assess matrix covers every field, and the published schema is complete.

<a id="br-10"></a>
<a id="br-11"></a>
<a id="br-12"></a>

#### Harden bridge code, connections and TLS (BR-10, BR-11, BR-12)

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

Lifecycle paths can panic on `expect`, errors lose their cause through `to_string()`, worker roles are selected by array index, and drain waiting polls every 5 ms. Connections have no keepalive or post-hello idle timeout, two observer clients can occupy both control slots and lock out the sender's Hello, and the TLS handshake runs inside the serial accept loop.

**What remains:**

- Remove `expect` from lifecycle paths; keep causes (`with_cause`) instead of re-wrapping `to_string()`; name worker roles and magic constants; make `wait_drained` event-driven.
- TCP keepalive and an idle timeout after hello; per-role control slots; TLS handshake outside the serial accept.
- TLS: allow multiple pins for rotation, optional PEM and a reload path. Protocol: document an upgrade path or negotiate versions; consider windowed frames to lift the stop-and-wait throughput limit (shared with BS-21).

**Example:** two monitoring clients connected to the control port prevent the sender from reconnecting after a restart.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/receiver.rs:296,301,373,519-529,584,614,624,878`; `drasi-core/components/computation-plugins/common/bridge/src/config.rs:227,333,399`; `drasi-core/components/computation-plugins/sources/bridge/src/component.rs:207,215`; `drasi-core/components/computation-plugins/common/bridge/src/boundary.rs:117-126`.

**Done when:** no `expect` remains on lifecycle paths, the control-slot lockout has a test, and pin rotation is supported.

<a id="br-30"></a>

#### Add bridge source lifecycle tests (BR-30)

**Kind:** Qualification. **Priority:** High. **Effort:** Medium.

The bridge source crate has no tests; `common/bridge/tests` covers the protocol and link but not the source's lifecycle hooks.

**What remains:** tests for restart on the same object, pause/resume, erase retry, stop timeout and retry, negative factory validation, accept-error resilience and protocol-version mismatch. Run the SDK conformance kit. Prepared-peer partition tests are in [CW-91](#cw-91).

**Example:** `stop` → `start` on the same `BridgeSource` object, then a sender reconnect, is not tested.

**Evidence:** `drasi-core/components/computation-plugins/sources/bridge/` (no `tests/`, no `#[cfg(test)]`); `drasi-core/components/computation-plugins/common/bridge/tests/bridge.rs`.

**Done when:** the listed scenarios pass in CI, and the conformance kit passes on both Tokio flavours.

<a id="br-05"></a>
<a id="br-40"></a>

#### Correct and split the bridge documentation (BR-05, BR-40)

**Kind:** Issue. **Priority:** High. **Effort:** Small to medium.

The shared README states wire version 1 although the protocol is version 2, and its configuration example uses `"to-analysis"`/`"from-plant"` as endpoint IDs, which are not valid component UUIDs. One 446-line README covers both endpoints, and the source has no `DESIGN.md` or `CONFIGURATION.md`.

**What remains:** fix the version text and the endpoint examples; split the README into a source `DESIGN.md` (topology, v2 state machine, generations and sessions, receipts, cuts, durability ownership, TLS, failure semantics) and `CONFIGURATION.md` (every field, contract versus local, resources, update matrix, journal provisioning, erase), following `NATIVE-SOURCE-GUIDE.md`; add rustdoc.

**Example:** copying the README example fails validation because `receiver_endpoint` must be a UUID.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/README.md:26-27,36-40`; `drasi-core/components/computation-plugins/common/bridge/src/protocol.rs:16`; `drasi-core/core-types/src/component_identity.rs:33,51-52`.

**Done when:** both documents exist, match the code, and the README examples validate.

<a id="plugin-source-counter-mock"></a>

### Counter and mock sources

`drasi-core/components/computation-plugins/sources/counter` (`drasi.standard/volatile-counter`, dynamic and Rust hosting) is the minimal reference source plugin authors copy; it has management and `configuration_details` but known management bugs, three tests and no `DESIGN.md`/`CONFIGURATION.md`. No native equivalent of the ordinary `components/sources/mock` exists.

<a id="mk-01"></a>
<a id="ct-01"></a>
<a id="source-d-4"></a>

#### Provide a native mock source (MK-01, CT-01, Source D-4)

**Kind:** Issue. **Priority:** High. **Effort:** Large.

The ordinary mock source offers sensor and generic generators, update and relation events, a published schema and a paired bootstrap provider. The native counter emits only inserts of one node label, so tests, demos and examples that rely on mock cannot move to native plugins. The recommended choice (not yet taken) is a separate plugin, keeping counter minimal; the alternative is to extend counter.

**What remains:** a new crate `computation-plugins/sources/mock` (`drasi.standard/mock`) with:

- generators `counter`, `sensor_reading { sensor_count, mesh }` (Insert then Update per sensor; `CONNECTED_TO` relations with strength updates and rewiring) and `generic`;
- a `timestamp` property, unbounded runs, interval default and validation;
- `source_schema_reader` ported from `describe_schema`;
- a paired bootstrap factory emitting 10 deterministic nodes via `PluginDefinition::new_with_bootstrap`, keeping the FFI bootstrap-regression role of issue #602;
- `inject_event` as a test-support feature;
- lifecycle, conformance-kit and documentation compliance from the start.

**Example:** the ordinary configuration `{ "data_type": { "type": "sensor_reading", "sensor_count": 5, "mesh": true }, "interval_ms": 1000 }` should, in its native form, emit five sensor nodes, then updates and `CONNECTED_TO` relations, exactly as the ordinary mock does.

**Evidence:** `drasi-core/components/sources/mock/src/config.rs:40-58`, `mock.rs:190-275,631`, `bootstrap.rs:15-36`; `drasi-core/components/computation-plugins/sources/` (no `mock`).

**Done when:** the decision is recorded, and every ordinary mock capability above is available natively with tests, including the bootstrap path through the loaded library.

<a id="ct-02"></a>

#### Let the counter be removed with erase (CT-02)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

Counter does not declare `erases_owned_state`, so the Host fails every removal with erase before the plugin runs, although counter owns no state.

**What remains:** apply the rule chosen in [CW-98](#cw-98) (declare a no-op erase, or a Host-accepted "owns nothing" capability) and add an erase-twice test.

**Example:** removing a counter source with erase fails with "native component does not support permanent state removal".

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/src/lib.rs:59-64`; `drasi-core/components/host-sdk/src/computation/proxy.rs:1538-1543`.

**Done when:** removal with erase succeeds twice in a test.

<a id="ct-03"></a>
<a id="ct-04"></a>
<a id="ct-08"></a>

#### Fix counter management (CT-03, CT-04, CT-08)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

Plugin authors copy counter's management code, so its bugs spread. `apply` refuses a stopped or never-started object, mismatches are reported as `Failed` rather than `Refused`, and assessment calls `Instant::now()`, so it is not a pure function of the configuration.

**What remains:**

- Allow `apply` on a stopped object; return `ApplyError::Refused` for plan, identity or configuration mismatch.
- Replace the `Instant::now()` check with a fixed `intervalMs` maximum published in the schema.
- Make `count` increases and `paused` InPlace updates; turn control kinds and the annotation key into `pub const`s; remove `expect` from metadata; add rustdoc.

**Example:** applying an `intervalMs` change to a counter after `stop` fails with "counter update requires an activated instance".

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/src/component.rs:75-80,94-97,182-196,245,266-267`; `drasi-core/components/computation-plugins/sources/counter/src/lib.rs:48,53`.

**Done when:** apply works on a stopped object, refusals are typed, assessment is pure, and tests cover each case.

<a id="ct-05"></a>
<a id="ct-09"></a>

#### Make the counter interval drift-free and document its pause controls (CT-05, CT-09)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

Each `next()` sleeps the full interval after any time spent waiting, so emission drifts. Counter has two pause concepts (host Pause and the `drasi.counter.pause` control) that are not explained together, and if the control channel closes while paused, `next()` fails with a channel error.

**What remains:** use `tokio::time::interval` with `MissedTickBehavior::Delay`; document both pause concepts; handle a dropped control channel while paused; optionally set `effective_from` to wall-clock milliseconds instead of the sequence number. Unverified: whether a long interval delays host Pause or Stop.

**Example:** with `intervalMs: 1000` and a slow consumer, events arrive more than one second apart and the gap accumulates.

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/src/component.rs:209-214,234`.

**Done when:** a paused-time test shows no drift, a dropped control channel is handled, and the README or `DESIGN.md` explains both pause concepts.

<a id="ct-07"></a>

#### Publish the counter's data schema (CT-07)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Small.

Counter publishes no `source_schema_reader`, so it does not demonstrate schema publication to plugin authors, whereas the ordinary mock source publishes one.

**What remains:** add a static schema reader returning `Counter { value: Integer }`.

**Example:** schema discovery on a graph with a counter source returns no labels.

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/src/lib.rs:33-82` (no schema reader).

**Done when:** the schema reader exists and a test checks its output.

<a id="ct-06"></a>

#### Test the counter fully (CT-06)

**Kind:** Qualification. **Priority:** Medium. **Effort:** Small to medium.

Counter has three tests, all about updates, although it is the reference implementation.

**What remains:** tests for configuration validation (overflow, unknown field, defaults), the control handler (unknown kind, non-null payload), sequence monotonicity, timing with paused Tokio time, apply on a stopped object, erase, and the SDK conformance kit.

**Example:** `{"stream":"s","start":9223372036854775807,"count":2}` must be rejected for overflow; no test asserts it.

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/src/component/tests.rs:59-160`.

**Done when:** the listed tests and the conformance kit pass on both Tokio flavours.

<a id="ct-40"></a>

#### Write the counter design and configuration documents (CT-40)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

Counter has only a short README, so authors copying it have no explanation of its emission model, sequencing or update rules.

**What remains:** a `DESIGN.md` (purpose, emission model, sequencing, pause concepts, volatility, patterns to copy) and a `CONFIGURATION.md` (fields, defaults, ranges, controls, update matrix, erase), following `NATIVE-SOURCE-GUIDE.md`.

**Example:** the README does not say which configuration changes apply in place.

**Evidence:** `drasi-core/components/computation-plugins/sources/counter/README.md`; `drasi-core/components/computation-plugins/sources/NATIVE-SOURCE-GUIDE.md`.

**Done when:** both documents exist and match the code.

<a id="native-sinks"></a>

## Native sinks

Each sink has its own section, followed by the legacy reactions that still
need a native sink. Work shared by every plugin is under
[Native plugins: shared work](#native-plugins-shared-work).

<a id="plugin-sink-network-runtime"></a>
<a id="cw-101"></a>

### Shared network sink runtime

`drasi-core/components/computation-plugins/common/network` (sink features). It
supplies the HTTP, gRPC and SSE sinks with the retry policy (jitter,
`Retry-After`, `retryBudgetMs`), `x-drasi-delivery-id`, the failure policy,
log sanitising and the shared listener. The remaining work is a misleading
secret example, duplicated helpers, metrics and loaded-library tests.

<a id="ni-01"></a>

#### Fix and test object-valued secret fields (NI-01)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

HTTP `secretHeaders` and gRPC `secretMetadata` are single secret fields of type
object. The Host already resolves one `secret-json:NAME` reference to a whole
JSON object, and Drasi Server uses that form, but neither sink has a test of
it. The HTTP documentation shows a per-header reference inside a literal
object, which the graph specification rejects because a secret field must be
one unresolved reference.

**What remains:**

- Change the HTTP example to a single reference, as the gRPC documentation
  already does.
- Add a test for each sink that resolves a `secret-json` reference into the
  header map and checks that the value is sent and never logged.

**Example:** `"secretHeaders": {"x-api-key": "<secret reference>"}` (as
documented) fails with "configuration field secretHeaders requires a
compatible value or unresolved secret reference". The working form is
`"secretHeaders": <reference to secret-json:API_HEADERS>`, where the secret
holds `{"x-api-key":"..."}`.

**Evidence:** `drasi-core/components/computation-plugins/sinks/http/CONFIGURATION.md:87`,
`drasi-core/components/computation-plugins/sinks/http/src/config.rs:141`,
`drasi-core/components/computation-plugins/sinks/grpc/src/config.rs:163`,
`drasi-core/components/computation-plugins/sinks/grpc/CONFIGURATION.md:48-49,73`,
`drasi-core/lib/src/computation/v1/graph/specification.rs:141-145,161-162`,
`drasi-core/components/host-sdk/src/management.rs:384-421`,
`drasi-server/tests/native_computation_test.rs:236`.

**Done when:** both CONFIGURATION.md files show the single-reference form, and
an HTTP test and a gRPC test resolve a `secret-json` reference into sent
headers.

<a id="ni-03"></a>

#### Share the pause gate and exposure check (NI-03)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Small to medium.

Each network and log sink carries its own copy of the same behaviour, so a
fix to one copy does not reach the others. The SSE sink has its own pause
flag, non-loopback exposure rule and bearer check. (Templates are shared: every
templating sink renders through `drasi-computation-template-common`, with one
helper set, strict rendering, no escaping and one context.)

**What remains:**

- Make `common/network` expose a public pause gate and a token check that the
  SSE sink can call, and use its `validate_exposure` from SSE.

**Example:** a fix to the bearer-token comparison in `common/network` does not
reach the SSE sink's own check.

**Evidence:** `drasi-core/components/computation-plugins/sinks/sse/src/component.rs:278-291,482,796-803`,
`drasi-core/components/computation-plugins/sinks/sse/src/config.rs:196-200`,
`drasi-core/components/computation-plugins/common/network/src/config.rs:240-248`,
`drasi-core/components/computation-plugins/common/network/src/security.rs:20-40`.

**Done when:** SSE uses the `common/network` gate, exposure check and token
check, and HTTP, gRPC, SSE and log take their helper set from one
`reaction-common` definition.

<a id="ni-04"></a>
<a id="ns-03"></a>

#### Report sink metrics and count skipped deliveries (NI-04, NS-03)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Medium.

Operators can see network sink behaviour only in logs. Under
`failurePolicy: skip` a dropped change produces a warning and nothing else, so
data loss cannot be alerted on.

**What remains:** report deliveries, items, bytes, retries, skips, failures by
class, delivery latency and in-flight age for the HTTP, gRPC and SSE sinks,
plus subscriber counts for SSE, through the plugin metrics facility
([Sink X-11](#sink-x-11), [CW-26](#cw-26)).

**Example:** a receiver returns 400 for every change of one output under
`skip`. The pipe stays healthy and the only sign is a stream of
"deliberate delivery skip" warnings.

**Evidence:** `drasi-core/components/computation-plugins/common/network/src/config.rs:298-318`,
`drasi-core/components/computation-plugins/sinks/http/DESIGN.md:287-297`,
`drasi-core/components/computation-plugins/sinks/grpc/DESIGN.md:262,278`,
`drasi-core/components/computation-plugins/sinks/sse/DESIGN.md:237-243`.

**Done when:** each network sink reports the listed counters, skips are
counted, and a test reads them after a retry and a skip.

<a id="ni-07"></a>
<a id="hs-30"></a>
<a id="gs-30"></a>
<a id="ss-30"></a>

#### Run the sink conformance kit through loaded libraries (NI-07, HS-30, GS-30, SS-30)

**Kind:** Qualification. **Priority:** Medium. **Effort:** Medium.

`SinkConformance` runs the HTTP, gRPC, SSE and log sinks only through the
linked `plugin()` definition, so it does not check the dynamic library or both
Tokio runtime flavours, which is where ABI and runtime faults appear.

**What remains:** add a driver that loads each freshly built sink library and
runs the same kit on current-thread and multi-thread runtimes, and run it in
CI ([CW-41](#cw-41)).

**Example:** `cargo test -p drasi-computation-sink-http` passes the kit, but a
library built with a mismatched exporter would only fail under the Host SDK
loader, which the kit never exercises.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/testing.rs`,
`drasi-core/components/computation-plugins/sinks/http/src/tests/sink.rs:38-39`,
`drasi-core/components/computation-plugins/sinks/grpc/src/tests.rs:304-305`,
`drasi-core/components/computation-plugins/sinks/sse/src/tests.rs:368-369`,
`drasi-core/components/computation-plugins/sinks/log/src/tests.rs:15`.

**Done when:** the kit passes for all four sinks through their loaded
libraries on both runtime flavours in a named CI job, with no skipped case
counted as a pass.

<a id="plugin-sink-http"></a>

### HTTP sink

`drasi-core/components/computation-plugins/sinks/http` provides
`drasi.network/http-sink` (`Handled`, configurable `successStatusCodes`) and
`drasi.network/http-delivery` (a DeliveryRunner consumer confirmed by
`DeliveryBatchIdentity`); lifecycle, management, secrets, TLS and documentation
are complete. Shared work is under [Shared network sink runtime](#plugin-sink-network-runtime).

<a id="http-delivery-qualification"></a>

#### Qualify the delivery consumer through the Host DeliveryRunner (HS-30)

**Kind:** Qualification. **Priority:** Medium. **Effort:** Small to medium.

The `drasi.network/http-delivery` consumer is tested only by calling the
consumer directly. Its central claims (only the operation in flight repeats
after a crash, and erasing removal deletes its progress) depend on the Host
DeliveryRunner and are untested.

**What remains:**

- A Host SDK test that crashes between the receiver's confirmation and the
  runner's progress commit, restarts, and checks that only the confirmed batch
  is resent with the same identity.
- A test that removing the consumer with `StateIntent::Erase` deletes its
  host-owned DeliveryRunner progress, and that a second erase succeeds
  ([CW-98](#cw-98)).

**Example:** sequence: receiver confirms batch 7 → process killed → restart →
the receiver must see batch 7 once more with the same identity, and never
batch 6.

**Evidence:** `drasi-core/components/computation-plugins/sinks/http/src/tests/consumer.rs:90-226`,
`drasi-core/components/computation-plugins/sinks/http/DESIGN.md:196-223`,
`drasi-core/components/host-sdk/tests/native_network_loopback.rs:111-113`.

**Done when:** both tests pass through the loaded library.

<a id="plugin-sink-grpc"></a>

### gRPC sink

`drasi-core/components/computation-plugins/sinks/grpc` provides
`drasi.network/grpc-sink` (`Handled` after an exact `items_processed`
acknowledgement); lazy connection, `tonic::Code` classification, TLS/mTLS,
secrets, management and documentation are complete.

<a id="gs-08"></a>
<a id="ni-02"></a>

#### Add a destination-confirmed consumer mode (GS-08, NI-02)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Medium.

The gRPC sink can only be `Handled`: a crash after the receiver applied a
batch but before the host acknowledged the input replays the whole input. HTTP
avoids this with `drasi.network/http-delivery`; gRPC cannot, because
`ProcessResultsResponse` has no field to echo the batch identity.

**What remains:**

- Add a delivery-batch identity field to the `drasi.v1` request and
  `ProcessResultsResponse` in `grpc-common`.
- Add a consumer factory (for example `drasi.network/grpc-delivery`) on the
  Host DeliveryRunner that completes only when the response echoes the exact
  `DeliveryBatchIdentity`, with explicit `retryable` classification.
- Document the receiver contract and test crash/replay as for HTTP.

**Example:** a receiver writes 500 changes, replies, and the Drasi process is
killed before acknowledgement. Today all 500 are resent; in consumer mode only
the unconfirmed batch is resent with the same identity.

**Evidence:** `drasi-core/components/grpc-common/proto/reaction/drasi/v1/reaction.proto:40-64`,
`drasi-core/components/computation-plugins/sinks/grpc/DESIGN.md:266-271`,
`drasi-core/components/computation-plugins/sinks/http/src/consumer.rs`.

**Done when:** a gRPC consumer factory exists, is documented, and passes a
crash/replay test through the loaded library.

<a id="grpc-retryinfo"></a>

#### Honour gRPC `RetryInfo`

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

The sink reads retry hints only from `grpc-retry-pushback-ms` and
`retry-after` metadata; a receiver that sends the standard
`google.rpc.RetryInfo` status detail is retried on the default backoff.

**What remains:** decode `RetryInfo.retry_delay` from status details and feed
it to the shared retry policy ([Sink X-05](#sink-x-05)), capped by `retryBudgetMs`.

**Example:** a receiver returns `RESOURCE_EXHAUSTED` with `RetryInfo` of 10 s;
the sink retries after about 100 ms.

**Evidence:** `drasi-core/components/computation-plugins/sinks/grpc/src/transport.rs:99-131`,
`drasi-core/components/computation-plugins/sinks/grpc/DESIGN.md:274-275`.

**Done when:** a test shows the `RetryInfo` delay is honoured.

<a id="gs-02"></a>
<a id="sink-d-14"></a>

#### Report receiver readiness (GS-02, Sink D-14)

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

With the default lazy connection, Start succeeds while the receiver is down,
and the only signal is failing deliveries and warnings. Sinks have no status
API to report "not ready" (the SDK `ControlSender::not_ready` notifies
upstream components, not status).

**What remains:** give native sinks a way to report readiness to Host status
([CW-23](#cw-23)), then report not-ready from the gRPC sink until its channel
connects.

**Example:** `endpoint: "http://receiver:50051"` with the receiver stopped:
the sink shows Running while every input retries.

**Evidence:** `drasi-core/components/computation-plugins/sinks/grpc/DESIGN.md:202-204,278`,
`drasi-core/components/computation-plugin-sdk/src/control.rs:129-134`.

**Done when:** a stopped receiver is visible as not-ready in component status
and clears when it connects.

<a id="plugin-sink-sse"></a>

### SSE sink

`drasi-core/components/computation-plugins/sinks/sse` provides
`drasi.network/sse-sink` (`Accepted`, volatile broadcast) with a real Pause
gate, deadline-bounded Stop, loopback default, authentication, CORS, TLS,
subscriber limits and documentation. Shared work is under
[Shared network sink runtime](#plugin-sink-network-runtime).

<a id="ss-05"></a>

#### Re-enable the live-rebind tests after CW-94 (SS-05)

**Kind:** Qualification. **Priority:** Low (deferred with CW-94).
**Effort:** Small.

Two tests that check SSE follows a live input rebind are ignored while
[CW-94](#cw-94) is deferred, so rebinding an SSE sink's declared inputs has no
running test.

**What remains:** once CW-94 settles the rebind design, update and enable both
tests.

**Example:** `cargo test -p drasi-host-sdk --test native_network_loopback
native_sse_declared_inputs_follow_a_live_rebind` reports both as ignored.

**Evidence:** `drasi-core/components/host-sdk/tests/native_network/sse.rs:916-927`.

**Done when:** both tests run and pass on both runtime flavours.

<a id="plugin-sink-log"></a>

### Log sink

`drasi-core/components/computation-plugins/sinks/log` provides
`drasi.standard/log` with compiled templates, the `json` helper, consistent
routing, `renderFailurePolicy`, `level`/`target`/`output`, management and
documentation. Shared helper work is under
[Shared network sink runtime](#ni-03).

<a id="lg-30"></a>
<a id="ni-05"></a>

#### Add Host SDK real-library tests (LG-30, NI-05)

**Kind:** Qualification. **Priority:** Medium. **Effort:** Small.

The log sink is tested only in-crate. It is not in the Host SDK plugin
package mapping, so no test loads the built library, wires it to a real
producer and checks its output.

**What remains:** add the log package to the mapping and test the six
lifecycle hooks, templates, routes and both render-failure policies through
the loaded library.

**Example:** a query → log pipeline with `output: stdout` and a template using
`{{json after}}`, checked line by line after an add, an update and a delete.

**Evidence:** `drasi-core/components/host-sdk/tests/native_network_loopback.rs:70-77`.

**Done when:** the tests run in CI through the loaded library.

<a id="plugin-sink-capture"></a>

### Capture sink

`drasi-core/components/computation-plugins/sinks/capture` provides
`drasi.standard/capture`, which writes one envelope JSON frame per line and is
`Handled` after write and flush. It is the main test sink but has no erase,
update planning, design documents or in-crate tests.

<a id="cp-01"></a>
<a id="sink-d-11"></a>

#### Decide when the capture file is truncated (CP-01, Sink D-11)

**Kind:** Decision. **Priority:** Medium. **Effort:** Small.

With `append: false` every Start truncates the file, so stopping and
restarting the same component (for example around an update) erases what it
captured earlier.

**What remains:** add `truncate: onCreate | onStart` (recommended default
`onCreate`), or document per-start truncation as intended; test stop/start
retention.

**Example:** capture 100 frames, Stop, Start: the file is empty.

**Evidence:** `drasi-core/components/computation-plugins/sinks/capture/src/component.rs:100-112`,
`drasi-core/components/computation-plugins/STANDARD.md:84-86`.

**Done when:** the chosen behaviour is configurable or documented and covered
by a stop/start test.

<a id="cp-03"></a>
<a id="lr-09"></a>

#### Cover the legacy file reaction (CP-03, LR-09)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small to medium.

The ordinary `file` reaction has no native sink. It renders templates per
route and supports `append`, `overwrite` and `perChange` write modes and a
`filenameTemplate`; capture writes only raw envelopes to one path.

**What remains:** add these to capture or as a new `drasi.standard/file` sink
on the shared template engine (`computation-plugins/common/template`), with documentation of each mode's
duplicate behaviour on replay.

**Example:** `{"outputPath":"/data","writeMode":"perChange","filenameTemplate":"{{after.id}}.json"}`
has no native equivalent.

**Evidence:** `drasi-core/components/reactions/file/src/config.rs:22-56`,
`drasi-core/components/computation-plugins/sinks/capture/src/component.rs:18-44`.

**Done when:** every legacy file configuration has a native equivalent with
tests.

<a id="capture-erase-update"></a>

#### Declare erase and update boundaries

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

Capture does not declare `erases_owned_state` ([CW-98](#cw-98)) and has no `Factory::assess`,
so removal with erase always fails and any configuration change needs a
recreate.

**What remains:** declare erase and decide whether it deletes the file (it is
user data, so a no-op is likely); classify `path` and `append` changes as
Restart ([Sink X-10](#sink-x-10)).

**Example:** removing a capture sink with erase fails with "native component
does not support permanent state removal".

**Evidence:** `drasi-core/components/computation-plugins/sinks/capture/src/lib.rs:51`,
`drasi-core/components/host-sdk/src/computation/proxy.rs:1538-1543`.

**Done when:** erase succeeds twice and an assess test covers both fields.

<a id="cp-02"></a>
<a id="cp-04"></a>

#### Harden stop and frame writing (CP-02, CP-04)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

Stop flushes without regard to `StopMode` or the deadline, a cancelled write
can leave a partial last line, the 64 MiB codec limit is an unnamed literal,
each frame is copied before writing, and the crate has no unit tests.

**What remains:** honour Abort and the deadline in the final flush; detect or
skip a partial trailing line on restart with `append: true`; name the limit;
add an optional `fsync`; write without the extra copy; add unit tests.

**Example:** a host aborts Stop during a large write, then restarts with
`append: true`; the next frame is appended to a truncated line.

**Evidence:** `drasi-core/components/computation-plugins/sinks/capture/src/component.rs:61,113-119,132-135`,
`drasi-core/components/computation-plugins/sinks/capture/README.md:14-16`.

**Done when:** the listed changes have unit tests.

<a id="cp-40"></a>

#### Write the capture design and configuration documents (CP-40)

**Kind:** Issue. **Priority:** High (release). **Effort:** Small.

Capture has only a short README; its frame format, flush-not-fsync meaning,
truncation and erase behaviour are not documented in the standard sink
layout.

**What remains:** add `DESIGN.md` and `CONFIGURATION.md` following
`drasi-core/components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md`.

**Example:** n/a (documentation).

**Evidence:** `drasi-core/components/computation-plugins/sinks/capture/README.md`.

**Done when:** both documents exist and match the code.

<a id="plugin-sink-profiler"></a>

### Profiler sink

`drasi-core/components/computation-plugins/sinks/profiler` provides
`drasi.standard/profiler`, which keeps a sample window and logs latency
statistics periodically. Its Pause, input handling, metric definitions, tests
and documentation all need work.

<a id="pf-01"></a>

#### Stop reports while paused (PF-01, CW-96)

**Kind:** Issue. **Priority:** Low. **Effort:** Small.

The report worker runs on `ActivationScope` behind a `PauseGate`, so Pause
returns once reporting has stopped, and Stop honours the deadline.

**What remains:** emit a final report on Drain, and add a test showing no
report while paused, reports after Resume and the final report on Drain.

**Example:** stopping with `Drain` discards the counts gathered since the last
report.

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/component.rs`
(`start_activation`, `stop`).

**Done when:** the test passes.

<a id="pf-02"></a>

#### Accept input that is not query output (PF-02)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

The sink declares the generic graph-change input, but `handle` calls
`QueryChangeCodec::metadata(...)?`, which returns an error for any envelope
that is not current query output. Wiring a source or transformer directly to
the profiler fails every input and stalls the pipe.

**What remains:** skip and count envelopes without query profiling metadata
instead of failing.

**Example:** PostgreSQL source → profiler: the first input fails with
`InvalidRow` and is retried indefinitely.

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/component.rs:247-256`,
`drasi-core/components/computation-contracts/src/query_codec.rs:725-729`,
`drasi-core/components/computation-plugins/SINKS.md:1-8`.

**Done when:** a source-only pipeline runs through the profiler without
failures and the skips are counted.

<a id="pf-03"></a>

#### Define and correct the reported metrics (PF-03)

**Kind:** Decision. **Priority:** Medium. **Effort:** Small.

`reaction_receive_ns` and `reaction_complete_ns` are stamped back to back, so
`reaction_processing` is always about zero. `total` starts at source receive,
whereas legacy started at source send. Reports are in nanoseconds with no
standard deviation or header, unlike legacy.

**What remains:** decide the definitions (recommended: restore legacy ms,
stddev and `source_send → reaction_complete`), stamp completion after
processing, and document them. Whether to publish metrics instead of logging
is [Sink D-13](#sink-d-13).

**Example:** every report shows `reaction_processing: ... mean_ns=0`.

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/component.rs:117-125,250-252`,
`drasi-core/components/core-types/src/profiling.rs:102-106`,
`drasi-core/components/reactions/profiler/src/profiler.rs:267-268,572,706`.

**Done when:** the definitions are documented and tested against known
timestamps.

<a id="pf-04"></a>

#### Bound window memory and report outside the lock (PF-04)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

`windowSize` has no maximum and its buffer is allocated in full at create.
Each report sorts every metric while holding the mutex that `handle` needs,
so large windows delay delivery.

**What remains:** add a schema maximum and parse check, allocate lazily, and
copy the samples out before computing statistics.

**Example:** `windowSize: 100000000` allocates gigabytes at create.

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/component.rs:31-38,72,88-92,223`.

**Done when:** an oversized window is rejected and a test shows `handle` is
not blocked during a report.

<a id="pf-05"></a>

#### Declare erase and update boundaries (PF-05)

**Kind:** Issue. **Priority:** Low. **Effort:** Small.

The profiler owns no external state but does not declare
`erases_owned_state`, so removal with erase fails ([CW-98](#cw-98)), and it has
no `Factory::assess`, so changing the interval needs a recreate.

**What remains:** declare a no-op erase; classify `reportIntervalSecs` and
`windowSize` as InPlace at Pause with a `NativeManagement` apply.

**Example:** removing the profiler with erase fails with "native component
does not support permanent state removal".

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/lib.rs:53`.

**Done when:** erase succeeds twice and an assess/apply test covers both
fields.

<a id="pf-30"></a>
<a id="pf-40"></a>

#### Add tests and documentation (PF-30, PF-40)

**Kind:** Qualification. **Priority:** High (release). **Effort:** Medium.

The profiler has one unit test, no Host SDK test, no design or configuration
document, and is missing from the per-sink table in `SINKS.md`.

**What remains:** port the useful legacy statistics tests; add Host SDK tests
for pause gating, source-only input and reports; add `DESIGN.md` (metric
definitions, units, window) and `CONFIGURATION.md`; add a `SINKS.md` row.

**Example:** n/a.

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/component.rs:259-284`,
`drasi-core/components/reactions/profiler/src/tests.rs`,
`drasi-core/components/computation-plugins/SINKS.md:127-138`.

**Done when:** the tests run in CI and both documents match the code.

<a id="sink-d-13"></a>

#### Decide the profiler metric format (Sink D-13)

**Kind:** Decision. **Priority:** Low.

The profiler sink logs nanosecond count/mean/percentile strings. It is
undecided whether it should publish through the SDK metric facility instead.

**Options:** (a) keep log output; (b) publish through `plugin_metrics` with the names agreed in
[the metrics entry](#source-x-07). **Recommendation:** (b).

**Evidence:** `drasi-core/components/computation-plugins/sinks/profiler/src/component.rs:118`.

**Done when:** the format is chosen and documented.

<a id="plugin-sink-storedproc-shared"></a>

### Shared stored-procedure sink support

The three stored-procedure sinks (`drasi.postgres/storedproc`,
`drasi.mysql/storedproc`, `drasi.mssql/storedproc`) are production-ready
External consumers. Each procedure call commits in one destination transaction
with its row in a per-sink progress table (format 4, identical across the
three, with a per-owner `maxStreams` limit and idempotent erase). The cursor
protocol is implemented once (`drasi-core/components/computation-plugins/common/storedproc`)
over a per-database `Session`, with one protocol table run against an
in-memory session and live PostgreSQL and MySQL. They share: one attempt per
operation with the graph owning retries (no in-plugin retry budget); definite
commit errors classified, only a lost COMMIT reply "outcome unknown" and
resolved from the cursor; procedures that commit or roll back refused
permanently and never re-run (a `pending` mark); query resets rolling the
stream's row over (output generation in the position) instead of adding rows;
`onPermanentFailure: skip` recorded in a dead-letter table in the same
transaction as the cursor advance; `deliveryId` in every template context;
`host`, `port`, `database` and `progressTable` unsupported in place; and
secure-by-default TLS (MySQL now refuses remote plaintext like PostgreSQL).
They share one parameter-only template rule
(`drasi_computation_template_common::ParameterTemplate`), keep one connection per activation,
honour deadlines and Drain/Abort, plan updates field by field, and publish
flat configuration with only credentials secret. Behaviour is in each sink's `DESIGN.md` and
`CONFIGURATION.md`. Decisions Sink D-3 (consumer plus destination cursor),
Sink D-10 (configurable, per-owner progress table) and Sink D-15 (Entra ID
tokens) are implemented. The items below are what remains; the first is a
regression for the MySQL and SQL Server sinks relative to their earlier plain
sink versions and the legacy reactions.

<a id="sp-10"></a>

#### Deliver activation credentials to consumer sinks (SP-10, Sink X-09)

**Kind:** Issue. **Area:** Host SDK. **Priority:** High. **Effort:** Medium.

**Update (unified Host services):** a factory now declares every Host service
once (`Capabilities::services`) and the Host binds any declared combination
through one construction path, so a consumer declaring
`activation_credentials` receives its `identity` resource like any sink:
creation is sent with `activation_credentials_bound: true` and the consumer
proxy's start resolves credentials through the wrapped component. What remains
below is end-to-end coverage and refresh semantics.

Before that change the Host SDK never passed activation credentials to a consumer. The factory
proxy accepted an `identity` dependency only when the component was not a
consumer, and every consumer creation request was sent with
`activation_credentials_bound: false`. All three stored-procedure sinks
declare `HostServices::activation_credentials`, map username/password and token
credentials, and keep the last activation's credentials in memory so erase can
run after Stop (SQL Server maps a token to Tiberius `AuthMethod::aad_token`).
None of that was reachable; only a configured `password` (a secret reference)
worked.

This is a regression: the legacy reactions used identity providers
(PostgreSQL password, token and certificate; MySQL; SQL Server password), and
the native MySQL and SQL Server sinks supported activation credentials while
they were plain sinks. It also blocks Entra ID for SQL Server (Sink D-15) and
affects the HTTP `drasi.network/http-delivery` consumer.

**What remains:**

- Done: consumer factories declare activation credentials and bind a
  `NativeIdentityResource`, as plain sinks do; the consumer proxy's start
  resolves them. Add an end-to-end test through a stored-procedure sink.
- Refresh credentials on restart; decide whether a running consumer reconnects
  with refreshed credentials.
- Restore the credential-rotation live tests: the MySQL identity scenario was
  reduced to asserting this gap, and the SQL Server fixture records the host's
  refusal so that it fails once the host supports it.

**Example:** an Azure SQL database with SQL authentication disabled, or a MySQL
server whose password an identity provider rotates, cannot be targeted today.

**Evidence (before the change):** the Host factory's `identity` binding
excluded consumers and `create_consumer_with_inputs` sent
`activation_credentials_bound: false`;
`drasi-core/components/host-sdk/tests/fixtures/native_mssql/credentials.rs`;
`drasi-core/components/host-sdk/tests/fixtures/native_mysql/identity.rs`.

**Done when:** a consumer sink connects with provider-supplied credentials
through the loaded library, a test rotates them across a restart, and SQL
Server authenticates with an Entra ID token.

<a id="sp-11"></a>

#### Let the HTTP consumer sink opt in to at-least-once delivery (SP-11)

**Kind:** Improvement. **Area:** Native sinks. **Priority:** Low.
**Effort:** Small to medium.

Compatibility is declared: each plugin's pure describer returns a
`ComponentDescription`, and the graph compares the producer's output with
the consumer's input when it binds the edge, recording a mismatch as a
failed edge and a terminal binding failure on the consumer. The HTTP delivery
consumer describes its inputs as `ExactlyOnce` only, so an edge from a
producer with a volatile identity (for example an in-memory query) always
fails.

**What remains:** decide whether the HTTP consumer should offer
`AtLeastOnce` too. That needs a `deliveryGuarantee` configuration field read
by its describer and by `DeliveryHandler::delivery_guarantee`, and
receiver-side handling of the per-run `epoch` the at-least-once identity
carries (the receiver deduplicates by delivery identity today).

**Example:** an HTTP delivery consumer behind an in-memory query: the edge is
`Failed` with `Incompatibility::ProducerIdentity` on the consumer node.

**Evidence:** `drasi-core/components/computation-plugins/sinks/http/src/lib.rs`
(`HttpDeliveryFactory::describe`);
`drasi-core/components/computation-contracts/src/description.rs`
(`check_edge`).

**Done when:** the HTTP consumer either supports `deliveryGuarantee:
atLeastOnce` with receiver deduplication tests, or its documentation states
that it delivers exactly once only.

<a id="sp-12"></a>

#### Extend the sink conformance kit to consumers (SP-12, Sink X-02)

**Kind:** Qualification. **Area:** SDK, Host SDK. **Priority:** Medium.
**Effort:** Medium.

`drasi_computation_plugin_sdk::testing::SinkConformance` drives plain
`NativeSink`s only. The stored-procedure sinks, and the HTTP delivery consumer,
reuse only its `assess_matrix`; their lifecycle, erase and retry behaviour is
proven by hand-written unit and live tests instead.

**What remains:** add a consumer driver that runs lifecycle, erase twice,
retry-after-failure without Stop/Start, cancellation, configuration round trip
and the assess matrix through a `DeliveryRunner`, linked and through the
loaded library, on both Tokio flavours.

**Example:** a new database sink should get the same lifecycle proof by
supplying only a destination fixture.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/testing.rs`;
`drasi-core/components/computation-plugins/sinks/storedproc-postgres/src/tests.rs`;
`drasi-core/components/computation-plugins/sinks/storedproc-mysql/src/tests.rs`.

**Done when:** the kit runs for all three stored-procedure sinks and the HTTP
delivery consumer in CI.

<a id="sp-13"></a>

#### Report consumer delivery progress through management (SP-13, Sink X-04)

**Kind:** Improvement. **Area:** SDK, Host SDK. **Priority:** Low.
**Effort:** Small.

Operators cannot read a consumer sink's delivered position through
management. `consumer_progress_reader` exists only for producers (sources and
transformers); the Host SDK refuses a consumer-progress binding on a consumer.
Progress is visible only through the consumer proxy's `progress()` in tests,
or by querying the destination progress table.

**What remains:** decide whether consumer delivery progress belongs in
`consumer_progress_reader` or a separate read capability, and expose the
runner ledger (and optionally the destination cursor) through it.

**Example:** an operator investigating a stalled SQL Server sink should see
which stream and operation it last completed without querying the database.

**Evidence:** `drasi-core/components/computation-plugin-sdk/src/component.rs:64-66`;
`drasi-core/components/host-sdk/src/computation/factory.rs:500-509`.

**Done when:** a consumer's delivery progress is readable through management
for all three sinks.

<a id="sp-14"></a>

#### Document and tool destination-cursor recovery (SP-14)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

A cursor that is ahead of the host's ledger (the graph's storage restored from
an older backup) or rows from an escaped procedure commit stall the stream
with a permanent, never-skipped error ("receipt expired", "unfinished
operations", "an earlier attempt … ended the delivery transaction"). The
owner digest omits the construction scope, so two instances with the same
graph and component IDs on one database share a `maxStreams` budget and an
Erase; and `ComponentDescription` declares `ProgressPersistence::Durable` for
consumers even under `atLeastOnce`.

**What remains:** a runbook (and optionally a management command) that resets
one stream's cursor for an owner; decide whether the owner digest includes the
construction scope (a format bump) or document that graph IDs must be
globally unique; declare consumer progress persistence per guarantee.

**Evidence:** `drasi-core/components/computation-plugins/common/storedproc/src/cursor.rs`
(`owner`, `decide`); `drasi-core/components/computation-plugin-sdk/src/wire.rs`
(`DescribeRequest::consumer`).

**Done when:** each stalled-cursor error names its recovery and the owner
scope is decided.

<a id="sink-x-11-storedproc"></a>

#### Add delivery metrics (Sink X-11)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Small.

All three sinks log failures, reconnects and skips with sanitized fields, but
none reports metrics or readiness (each discards its `ControlSender`; see
[Source X-07 and Sink X-11](#source-x-07)). An operator cannot chart retries,
skips, cursor lag or delivery latency.

**What remains:** report the shared sink metrics (deliveries, operations,
retries by class, reconnects, failures, duration, stream count against
`maxStreams`) once [Source X-07 and Sink X-11](#source-x-07) define them.

**Example:** a rising deadlock retry rate on one output alias should be visible
without reading logs.

**Evidence:** no metric emission in
`drasi-core/components/computation-plugins/sinks/storedproc-*/src/` or
`drasi-core/components/mysql-common/src/storedproc/`.

**Done when:** each sink emits the shared sink metrics.

<a id="sp-08"></a>

#### Run the stored-procedure live suites in CI (SP-08, Sink X-15)

**Kind:** Qualification. **Priority:** High. **Effort:** Medium.

The PostgreSQL and MySQL live tests now cover erase twice, the per-owner
stream limit, reconnect after the session is killed, recovery and retry
without Stop/Start (including through the host proxy), partial failure with
replay against a fresh ledger, Abort with rollback, large inputs and template
rejection. Each sink crate also has a live `tests/live.rs`: the shared
protocol table (including skip, transaction control, generations and query
resets against `maxStreams`), a COMMIT whose reply is lost (a proxy), and for
PostgreSQL a commit-time deferred-constraint failure and a backend terminated
during COMMIT, for MySQL an InnoDB deadlock. They are `#[ignore]`d and need
Podman, and no CI job runs them.

**What remains:**

- Run `drasi-computation-source-postgres --test native_transactions` (the `storedproc`
  module), `drasi-host-sdk --test native_mysql_components`
  (`native_mysql_storedproc_*`) and
  `drasi-computation-sink-storedproc-{postgres,mysql} --test live` in named CI
  jobs ([CW-85](#cw-85)).
- Add the scenarios still missing: a database container restart during a
  delivery (not only a killed session), credential rotation once
  [SP-10](#sp-10) lands, and pause during delivery.
- SQL Server is [SMS-30](#sms-30).

**Example:** restart the MySQL container during a 5,000-change input; the sink
should finish without Stop/Start and without repeating committed operations.

**Evidence:** `drasi-core/components/computation-plugins/sources/postgres/tests/native_transactions/storedproc.rs`;
`drasi-core/components/host-sdk/tests/fixtures/native_mysql/storedproc.rs`.

**Done when:** both suites run in CI with the added scenarios.

<a id="dx-7"></a>

#### Teach database-writing sinks the mapping and delivery contract (DX-7)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Medium.

A plugin author writing a database sink must handle full replacements,
patches, removed properties and identity-only deletes. An absent before image
is not null. The author must also choose between `Handled`, `ConsumerMode::External`
and `NativeTransactionalConsumer`, which give different guarantees. The SDK
explains each part separately, but no example shows them together. Authors
copy legacy templates or assume every delete has `before.id`. The
stored-procedure sinks now implement the External profile with a destination
cursor, but they are large production crates, not a teaching example.

**What remains:**

- Add a database-writing sink example that decodes source and query envelopes
  through `GraphChangeBatch` and handles patches and identity-only deletes.
- Use `DeliveryItem::id` as the deduplication key, not projected values or
  transport counters.
- Demonstrate the lost-commit-response case, with destination-side
  deduplication where the effect and a receipt commit together.
- Add a table to `SINKS.md` and `NATIVE-SINK-GUIDE.md` stating whether each
  profile's guarantee covers an operation, a batch or a source transaction.
  `NativeTransactionalConsumer` does not enlist the vendor database.

**Example:** a sink writes `DELETE FROM t WHERE id = $1` from `before.id`. It
fails on an identity-only delete that has no before image. The example should
use the element identity instead.

**Evidence:** `drasi-core/components/computation-plugins/SINKS.md:10-38,79-117`;
`drasi-core/components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md:23-36`;
`drasi-core/components/computation-contracts/src/graph_view.rs:583-626`;
`drasi-core/components/computation-contracts/src/component_data.rs:21-31`;
`drasi-core/components/computation-contracts/src/delivery.rs:77-87`;
`drasi-core/components/computation-plugin-sdk/README.md:1092-1170`.

**Done when:** a tested example sink exists that covers patch, identity-only
delete and lost-commit replay, and the guides state the scope of each
guarantee.

<a id="plugin-sink-storedproc-postgres"></a>

### PostgreSQL stored-procedure sink

`drasi-core/components/computation-plugins/sinks/storedproc-postgres` (with
`drasi-core/components/postgres-common/src/delivery/progress.rs`).
Production-ready: see its [DESIGN.md](../../components/computation-plugins/sinks/storedproc-postgres/DESIGN.md)
and [CONFIGURATION.md](../../components/computation-plugins/sinks/storedproc-postgres/CONFIGURATION.md).
The shared items above apply. Its credential support waits on [SP-10](#sp-10),
its progress reporting on [SP-13](#sp-13) and its CI run on [SP-08](#sp-08).

<a id="spg-06"></a>

#### Retire the format-1 delivery handler with the legacy reaction (SPG-06, CW-63)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

`postgres-common` keeps the format-1 `PostgresDeliveryHandler` and its
database-wide `public.drasi_delivery_stream_v1` table unchanged, because the
legacy `drasi-reaction-storedproc-postgres` crate still exposes it as an
opt-in Rust API (its public `delivery` module and `tests/delivery.rs`); the
legacy reaction runtime itself never uses it. The legacy table
keeps its 4,096-stream database-wide cap and is never erased.

**What remains:** remove the format-1 handler and its table when the legacy
reaction is retired ([CW-63](#cw-63)), and document how to drop the old table.

**Example:** a database used by both the legacy reaction and the native sink
has two progress tables with different formats.

**Evidence:** `drasi-core/components/postgres-common/src/delivery.rs`;
`drasi-core/components/reactions/storedproc-postgres/src/executor.rs`.

**Done when:** only the format-4 progress table remains in `postgres-common`.

<a id="plugin-sink-storedproc-mysql"></a>

### MySQL stored-procedure sink

`drasi-core/components/computation-plugins/sinks/storedproc-mysql` (runtime in
`drasi-core/components/mysql-common/src/storedproc/`). Production-ready: see
its [DESIGN.md](../../components/computation-plugins/sinks/storedproc-mysql/DESIGN.md)
and [CONFIGURATION.md](../../components/computation-plugins/sinks/storedproc-mysql/CONFIGURATION.md).
The shared items above apply; [SP-10](#sp-10) is a regression for this sink.

<a id="smy-04"></a>

#### Stop requiring binlog fields for the sink's connection (SMY-04)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

`retry_attempts` is gone (the graph retries) and the binlog settings are no
longer exposed in the sink configuration. The shared MySQL connection
struct still requires a `server_id` and heartbeat, so the sink sets
placeholder values in one private place.

**What remains:** give `mysql-common`'s connection a constructor that needs no
replication settings, and use it for the sink.

**Example:** a reader of the sink's connection code should not see a
`server_id` that has nothing to do with the sink.

**Evidence:** `drasi-core/components/mysql-common/src/storedproc/session.rs`.

**Done when:** the sink builds its connection without replication fields.

<a id="plugin-sink-storedproc-mssql"></a>

### SQL Server stored-procedure sink

`drasi-core/components/computation-plugins/sinks/storedproc-mssql`.
Production-ready by code review and unit tests: see its
[DESIGN.md](../../components/computation-plugins/sinks/storedproc-mssql/DESIGN.md)
and [CONFIGURATION.md](../../components/computation-plugins/sinks/storedproc-mssql/CONFIGURATION.md).
The shared items above apply; [SP-10](#sp-10) (including Entra ID tokens) is a
regression for this sink.

<a id="sms-30"></a>

#### Qualify the SQL Server sink live (SMS-30, Sink X-15)

**Kind:** Qualification. **Priority:** High. **Effort:** Medium.

The sink has 39 unit tests, including the cursor protocol through a
database-free seam, and 4 database-free host tests. Nothing has run against a
real SQL Server: live qualification is deferred by the owner
([CW-78](#cw-78)). The shared protocol it uses is qualified live on
PostgreSQL and MySQL and against the in-memory session. The live tests
compile, and the fixture's `setup.sql` and `cleanup.sql` create and grant
`<schema>.drasi_delivery_progress` and its dead-letter table (format 4).

**What remains:** once the CW-78 environment exists, run the live cases:
the shared protocol table against `TiberiusSession`, per-operation
transactions, exactly-once replay, `@@TRANCOUNT`/266 detection of a procedure
that commits or rolls back (and no re-run after an escaped commit), a skipped
operation's dead-letter row, a definite versus lost COMMIT reply, Abort
dropping the connection, erase twice, the 2,098-parameter limit, Azure
redirect handling and Entra ID tokens (after [SP-10](#sp-10)).

**Evidence:** `drasi-core/components/host-sdk/tests/native_mssql_components.rs`;
`drasi-core/components/host-sdk/tests/fixtures/native_mssql/sink_effects.rs`;
`drasi-core/components/host-sdk/tests/fixtures/native_mssql/README.md`.

**Done when:** the SQL Server live suite runs in CI.

<a id="sms-08"></a>

#### Close the injection gap in the legacy SQL Server reaction (SMS-08)

**Kind:** Issue. **Area:** Legacy reaction. **Priority:** High.
**Effort:** Small.

The native sink rejects any template that writes a value into the SQL text.
The legacy `drasi-reaction-storedproc-mssql` still accepts inline
`{{json …}}` (and the other value helpers), which writes unescaped change data
into the command: the SQL injection that SMS-01 fixed in the native sink.

**What remains:** either apply `drasi_computation_template_common::validate_parameter_only` to the
legacy reaction (a deliberate breaking tightening, with a migration note), or
retire the reaction with [CW-63](#cw-63) and document the risk until then.

**Example:** `added: "EXEC dbo.upsert '{{json after}}'"` with a property value
`x'); DROP TABLE t;--` is accepted by the legacy reaction.

**Evidence:** `drasi-core/components/mssql-common/src/sql/template.rs`;
`drasi-core/components/reactions/storedproc-mssql/src/render.rs`.

**Done when:** the legacy reaction rejects non-parameter interpolation, or it
is retired.

<a id="plugin-sink-bridge"></a>

### Bridge sink

`drasi-core/components/computation-plugins/sinks/bridge` (with
`drasi-core/components/computation-plugins/common/bridge`) is a plugin-SDK
sink (`NativeSink`) exported by its own library. A durable sender keeps its
mapping in a file under `mappingDirectory`. Several items are shared with the
bridge source (Source BR-xx).

<a id="bs-01"></a>

#### Bound retries with jitter and a time budget (BS-01, Sink X-05)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

The send loop retries `Busy`, `Closed`, `AcceptanceUnknown` and
`StaleGeneration` with a fixed `retry_delay_ms` sleep, with no jitter and no
time budget. The sleep does not observe stop or pause. A paused receiver
exhausts the attempts and fails the input. Shared with Source BR-01 and BR-09.

**What remains:**

- Use the shared retry policy: exponential backoff with full jitter and a
  total budget.
- Treat receiver `Busy`/`Paused` as a cancellable wait.
- Make sleeps observe stop.

**Example:** with defaults (3 attempts, 50 ms), a receiver paused for 1 s fails
every input. It should wait within the budget.

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:253-317`;
`drasi-core/components/computation-plugins/common/bridge/src/config.rs:194-196,211-213`;
`drasi-core/components/computation-plugins/common/bridge/src/error.rs:54-62`.

**Done when:** tests cover retry exhaustion with attempts above 1, a receiver
paused longer than the budget, and stop during a retry sleep.

<a id="bs-02"></a><a id="bs-11"></a>

#### Honour the lifecycle context in start and stop (BS-02, BS-11, Sink X-01)

**Kind:** Issue. **Priority:** High. **Effort:** Medium.

Start ignores its context. It also clears `failed_cleanup` before loading
state, so a failed load leaves nothing to clean up. Stop ignores `StopMode`
and the deadline. `Drain` and `Abort` behave the same, and a stop after a
partial failure is not proven idempotent. Shared with Source BR-02.

**What remains:**

- `Drain` finishes the in-flight send and receipt within the deadline.
- `Abort` cancels.
- Start honours deadline and cancellation, and keeps `failed_cleanup` until
  load succeeds.
- Make stop idempotent.
- Test that a cancelled durable `handle` resumes through the receipt lane.

**Example:** `stop(Abort)` during a send should return immediately. On the next
start, the receipt lane should resolve the outcome of the abandoned send.

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:147-211`.

**Done when:** lifecycle tests cover Drain, Abort, a cancelled start and a
repeated stop.

<a id="bs-03"></a>

#### Recover a volatile sender after a restart against a live receiver (BS-03)

**Kind:** Investigation. **Priority:** High. **Effort:** Small to medium.

On restart, a volatile sender has no mapping. A receiver that still has history
returns non-retryable `StateLost` ("sender mapping is missing but receiver
history exists"), so the link may stay dead until the receiver restarts.
Unverified against a live receiver.

**What remains:**

- Reproduce the failure.
- Define the semantics: either a fresh volatile session epoch, or a typed
  recoverable error.
- Test it.

**Example:** restart only the sending instance of a volatile bridge. Today
every send may fail with `StateLost`.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/receiver.rs:401-405`;
`drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:107-134`.

**Done when:** a test restarts a volatile sender against a running receiver and
delivery resumes.

<a id="bs-04"></a>

#### Erase the durable mapping namespace (BS-04, CW-98 part)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

The sink owns a durable sender mapping but refuses `Erase`, so it cannot be
removed with erase. Shared with Source BR-03 and Source D-12.

**What remains:**

- Declare `erases_owned_state`.
- Delete the mapping namespace through its cleanup owner, idempotently.

**Example:** removing a durable bridge sink with erase fails with "this
component does not support permanent state removal".

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:165,217-226`.

**Done when:** erase removes the namespace, and a test runs erase twice.

<a id="bs-05"></a>

#### Keep local settings out of the wire contract (BS-05)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

`limits` (timeout, attempts, delay) is part of `LinkContract` and its
fingerprint. The saved ledger refuses a changed fingerprint. Tuning a retry
delay therefore breaks a durable link. Shared with Source BR-07.

**What remains:** remove timeout, retry and queue settings from the contract
and the fingerprint.

**Example:** changing `retryDelayMs` from 50 to 100 on a durable sink fails
start with "saved sender contract changed".

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/config.rs:346-373,398-400`;
`drasi-core/components/computation-plugins/common/bridge/src/state.rs:172-177`.

**Done when:** a limits-only change restarts the sink with its existing ledger,
and a test covers it.

<a id="bs-06"></a>

#### Log and measure bridge delivery (BS-06, Sink X-11)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

The bridge crates have no log calls. Retries, session changes, exhaustion and
seal are invisible. Shared with Source BR-04.

**What remains:**

- Trace each attempt, error code, session or generation change, exhaustion and
  seal.
- Surface egress status.

**Example:** a link that retries `StaleGeneration` should leave a log line with
the session and generation.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/` has
no `log::` or `tracing::` calls;
`drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:253-317`.

**Done when:** every attempt and state change is logged without payload data,
and egress status is visible to management.

<a id="bs-07"></a>

#### Correct the bridge documentation (BS-07, Sink X-14)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

The common README says wire version 1, but the protocol is version 2. The
retry and worker descriptions are wrong for the sink. The sink README is 21
lines, with no design or configuration document. Shared with Source BR-05 and
BR-40.

**What remains:**

- Fix the wire-version text, the retry description and the worker claim (true
  only for the receiver).
- Add the sink's `DESIGN.md` and `CONFIGURATION.md`.

**Example:** a reader of `common/bridge/README.md` is told wire 1, but the
sender negotiates protocol 2.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/README.md:24-27`;
`drasi-core/components/computation-plugins/common/bridge/src/protocol.rs:16`;
`drasi-core/components/computation-plugins/sinks/bridge/README.md`.

**Done when:** the docs match the code, and both sink documents exist.

<a id="bs-12"></a>

#### Plan bridge updates and publish the full schema (BS-12, Sink X-10)

**Kind:** Improvement. **Priority:** Medium. **Effort:** Small.

The sink has no `assess`. Every change recreates it, even a local limit.

**What remains:**

- Apply local settings in place; recreate on contract changes.
- Publish the full configuration schema.
- Unverified: whether the factory duplicates `sender_endpoint`. Only one field
  was found, at `factory.rs:30`. Check and remove any
  duplicate.

**Example:** raising `timeoutMs` should apply in place.

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/src/factory.rs:30`.

**Done when:** `assess` classifies every field, and tests cover InPlace and
Recreate.

<a id="bs-13"></a>

#### Stop concurrent control calls from failing delivery (BS-13)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

`BridgeControl::request` uses `try_lock`. A concurrent `control().status()`
call therefore makes delivery fail with `Busy`, and `invalidate` drops the
caller's control socket. The `producer_status` error is swallowed.

**What remains:**

- Use a queued lock or one connection per caller.
- Propagate the `producer_status` error.

**Example:** polling bridge status from management during load produces
spurious `Busy` delivery failures.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/client.rs:39-41,141-146`.

**Done when:** a concurrent-control test delivers with no `Busy`.

<a id="bs-14"></a>

#### Fix egress pending state and allow seal retry (BS-14)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

`handle` calls `begin`, which sets `pending`, before it validates the input.
The operation guard's `Drop` clears only `in_flight`, so a rejected input
leaves the sender "still owning an unresolved input". Seal marks the binding
sealed before the remote cut, so a failed remote seal cannot be retried.

**What remains:**

- Set `pending` only after validation.
- Allow a failed remote seal to be retried.

**Example:** send an envelope on the wrong port, then seal. The seal fails with
`AcceptanceUnknown`.

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:237-243`;
`drasi-core/components/computation-plugins/common/bridge/src/boundary.rs:234-257,275-285,368-377`.

**Done when:** tests cover a rejected input followed by a seal, and a retried
seal after a remote failure.

<a id="bs-20"></a>

#### Remove `expect`s and type the errors (BS-20, Sink X-13)

**Kind:** Improvement. **Priority:** Low. **Effort:** Small.

Several input- and state-reachable paths `expect`. Errors are converted with
`to_string`, and `wait_drained` polls every 5 ms.

**What remains:**

- Replace the `expect`s with errors.
- Keep typed errors.
- Name the constants.
- Make `wait_drained` event-driven.

**Example:** if a future change lets `handle` run with `started` set but no
control epoch, `component.rs:239` panics across the plugin boundary instead of
returning an error.

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/src/component.rs:178,239,257,264,268`;
`drasi-core/components/computation-plugins/common/bridge/src/config.rs:398-400`;
`drasi-core/components/computation-plugins/common/bridge/src/endpoint_control.rs:137-152`.

**Done when:** no `expect` remains on input or FFI-reachable paths.

<a id="bs-21"></a>

#### Align ALPN with the protocol and reduce round trips (BS-21)

**Kind:** Improvement. **Priority:** Low. **Effort:** Medium.

The ALPN is `drasi-bridge/1` but the protocol is version 2, and there is no
version negotiation. Every `handle` makes an extra `producer_status` round
trip. TLS pins cannot rotate. Shared with Source BR-11 and BR-12.

**What remains:**

- Align ALPN with the protocol and negotiate versions.
- Remove the per-`handle` status round trip.
- Support pin rotation.

**Example:** a protocol-3 receiver cannot reject a protocol-2 sender during
ALPN.

**Evidence:** `drasi-core/components/computation-plugins/common/bridge/src/transport.rs:16`;
`drasi-core/components/computation-plugins/common/bridge/src/protocol.rs:16`;
`drasi-core/components/computation-plugins/common/bridge/src/receiver.rs:380-400`.

**Done when:** versions are negotiated, and `handle` makes one round trip.

<a id="bs-30"></a>

#### Add bridge sink tests (BS-30, Sink X-02)

**Kind:** Qualification. **Priority:** High. **Effort:** Medium.

The sink crate has no tests of its own.

**What remains:** add tests for:

- retry exhaustion with attempts above 1;
- a receiver paused beyond the budget;
- a receiver restart mid-frame;
- `AcceptanceUnknown` resolved through receipts;
- a volatile restart;
- stop during `handle`;
- erase;
- concurrent control;
- a limits-only change.

Run the conformance kit. Cover partition qualification
with [CW-05](#cw-05) and [CW-91](#cw-91).

**Example:** kill the receiver after it accepts a frame but before the receipt.
The sender should resolve the outcome through the receipt lane.

**Evidence:** `drasi-core/components/computation-plugins/sinks/bridge/` has no
`tests/` directory and no `#[cfg(test)]` module.

**Done when:** the listed scenarios run in CI.

<a id="plugin-legacy-reactions"></a>

### Legacy reactions without a native sink

These ordinary reactions under `drasi-core/components/reactions/` have no
native sink. [CW-44](#cw-44) requires every required reaction to exist
natively with no omitted working functionality. Every port must meet the
native sink standard
(`drasi-core/components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md`) and
use the shared renderer and retry policy. Where the destination can confirm,
it should use the `DeliveryRunner` with an idempotency key built from the
delivery ID. The legacy file reaction is covered by the capture sink's
[CP-03](#cp-03).

<a id="sink-d-7"></a>

#### Decide which legacy reactions become native sinks (Sink D-7)

**Kind:** Decision. **Priority:** High.

Each reaction below needs a decision: port it, extend an existing sink, keep it
ordinary, or retire it. The recommendation:

- Port aws-sqs, rabbitmq, loki, eventgrid, azure-storage, a2a, dashboard and
  mcp.
- Extend capture into a `file` sink.
- Move application consumers to the existing native type.
- Platform: the owner decides with the Drasi Platform roadmap, and it stays
  ordinary meanwhile.
- Retire snapshot-test with [CW-63](#cw-63).

This replaces the "Legacy reactions" row of [CW-44](#cw-44).

**Example:** if rabbitmq is not ported, a Server configuration that uses
`kind: rabbitmq` stops working when ordinary plugins are removed
([CW-63](#cw-63)).

**Evidence:** `drasi-core/components/reactions/` has 18 reaction crates (plus
`tests`). Only application, the network, log and stored-procedure reactions
have native counterparts.

**Done when:** each reaction has a recorded decision, and the LR items below
match it.

<a id="lr-01"></a>

#### application: move consumers and retire the ordinary type (LR-01)

**Kind:** Issue. **Priority:** High. **Effort:** Small.

`NativeApplicationReaction` already exists. It is in-process and
acceptance-only. Examples still use the ordinary `ApplicationReaction`, which
blocks [CW-63](#cw-63). This is the "Application components" row of
[CW-44](#cw-44).

**What remains:**

- Move these examples to the native type:
  `drasi-core/examples/lib/{application-identity-provider,ris-live-getting-started,here-traffic-getting-started,gtfs-rt-getting-started}/main.rs`
  and `drasi-core/lib/examples/{stockroom,stockroom_persistent,fast_path,core_workload,computation_runtime}.rs`.
  Test consumers are in [CW-41](#cw-41) and [CW-77](#cw-77).
- Retire the ordinary type.
- Confirm that pause is host-driven. It uses the passive lifecycle macro.

**Example:** `examples/lib/ris-live-getting-started` still builds an ordinary
`ApplicationReaction`.

**Evidence:** `drasi-core/components/reactions/application/src/lib.rs:46`;
`drasi-core/components/reactions/application/src/native.rs:254-256`.

**Done when:** no example or consumer uses the ordinary type, and it is
removed.

<a id="lr-02"></a>

#### aws-sqs: native sink with stable deduplication IDs (LR-02)

**Kind:** Issue. **Priority:** Medium. **Effort:** Medium.

The legacy reaction sets `MessageDeduplicationId` to a random UUIDv4. A
retried send therefore defeats FIFO deduplication.

**What remains:**

- Build a Consumer External sink.
- Derive the deduplication ID from the delivery ID.
- Optionally use `SendMessageBatch` with per-entry results.
- Template the body, attributes and `message_group_id`.
- Use activation credentials.

**Example:** a retry after a lost `SendMessage` response creates a second FIFO
message today.

**Evidence:** `drasi-core/components/reactions/aws-sqs/src/reaction.rs:280`.

**Done when:** a native sink exists, and a retry of the same item is
deduplicated by SQS.

<a id="lr-03"></a>

#### rabbitmq: native sink with publisher confirms (LR-03)

**Kind:** Issue. **Priority:** Medium. **Effort:** Medium.

The legacy reaction publishes without publisher confirms, so its checkpoint
does not prove broker acceptance.

**What remains:**

- Build a Consumer External sink with confirms.
- Set AMQP `message_id` from the delivery ID.
- Support TLS.
- Reconnect through the shared retry policy.

**Example:** a broker crash after `basic_publish` returns loses the message,
and the checkpoint has already advanced.

**Evidence:** `drasi-core/components/reactions/rabbitmq/src/rabbitmq.rs:345`
(no `confirm_select` in the crate).

**Done when:** completion waits for a broker confirm, and tests cover a broker
restart.

<a id="lr-04"></a>

#### loki: thin native sink (LR-04)

**Kind:** Issue. **Priority:** Medium. **Effort:** Small.

Loki needs label templates, a tenant header, bearer or basic authentication,
and deterministic timestamps for Loki deduplication. The http-sink cannot
express these without new helpers.

**What remains:**

- Build a sink on the `DeliveryRunner` with these features.
- Derive timestamps from the delivery identity.

**Example:** `tenant_id: team-a` must send `X-Scope-OrgID: team-a`, and a
replayed push must not duplicate log lines.

**Evidence:** `drasi-core/components/reactions/loki/src/config.rs:38-40`;
`drasi-core/components/reactions/loki/src/descriptor.rs:81-93`.

**Done when:** a native Loki sink exists, and a replay produces no duplicate
entries.

<a id="lr-05"></a>

#### eventgrid: native sink with deterministic event IDs (LR-05)

**Related review finding:** [CR-P13](#cr-p13) records ordinary standalone
packaging debt; decide its disposition with the native replacement/retirement.

**Kind:** Issue. **Priority:** Low. **Effort:** Small to medium.

The legacy reaction derives a UUIDv5 event ID from the reaction, query,
sequence, operation and index. A native sink must keep IDs stable, seeded from
the delivery identity.

**What remains:**

- Build a Consumer External sink.
- Support the CloudEvents and EventGrid schemas.
- Support access key or Entra ID authentication.

**Example:** a retried publish must carry the same event `id`.

**Evidence:** `drasi-core/components/reactions/eventgrid/src/event.rs:43,70-78`.

**Done when:** a native sink exists, and a retry carries the same event ID.

<a id="lr-06"></a>

#### azure-storage: native sink on a current SDK (LR-06)

**Kind:** Issue. **Priority:** Low. **Effort:** Medium to large.

The legacy reaction writes to Blob, Queue and Table storage on the 0.21 Azure
SDK ([CW-66](#cw-66)).

**What remains:**

- Port all three targets.
- Make blob and table writes idempotent on keys derived from the delivery ID.
- Document queue delivery as at-least-once.
- Move to the current SDK.

**Example:** a replayed table write should upsert the same row key.

**Evidence:** `drasi-core/components/reactions/azure-storage/Cargo.toml:48-52`.

**Done when:** a native sink on the current SDK exists, and blob and table
replays are idempotent.

<a id="lr-07"></a>

#### a2a: native sink with task-activation state (LR-07)

**Kind:** Decision and Issue. **Priority:** Low. **Effort:** Medium to large.

The legacy reaction keeps per-key task-activation state and sends A2A
messages. A port needs a decision on where that state lives: in the
destination or in host state.

**What remains:**

- Decide where the state lives.
- Build a Consumer External sink using a deterministic message ID as the
  idempotency key.

**Example:** a one-shot activation that is retried must reuse its
`message_id`.

**Evidence:** `drasi-core/components/reactions/a2a/src/activation.rs:83,133`;
`drasi-core/components/reactions/a2a/src/process.rs`.

**Done when:** a native sink exists, and its state ownership is documented.

<a id="lr-08"></a>

#### dashboard: server-style sink or rebuild on SSE (LR-08)

**Related review finding:** [CR-K16](#cr-k16), heuristic matching only when row
identity is unknown; canonical row signatures already work.

**Kind:** Decision and Issue. **Priority:** Low. **Effort:** Large.

The legacy dashboard is an HTTP and WebSocket server with its own persisted
dashboards. A native version needs server-style exposure (Sink X-12), the
output-snapshot reader and a native state resource. The alternative is to
rebuild the UI on the SSE sink.

**What remains:** choose an approach and implement it.

**Example:** saved dashboards must survive a restart.

**Evidence:** `drasi-core/components/reactions/dashboard/src/storage.rs`;
`drasi-core/components/reactions/dashboard/src/api.rs`.

**Done when:** a native dashboard serves live and saved dashboards.

<a id="lr-10"></a>

#### mcp: server-style native sink (LR-10)

**Related review findings:** [CR-P11](#cr-p11), stable-key removal rather than
linear search, and [CR-P12](#cr-p12), configurable allowed origins. Carry these
decisions into the native design rather than assuming the ordinary gaps fixed.

**Kind:** Issue. **Priority:** Low. **Effort:** Medium to large.

The legacy reaction is an MCP server with resource subscribe, unsubscribe and
list. A native version needs Streamable HTTP per Sink X-12, `resources/read`
from output snapshots, bearer authentication and a session cap.

**What remains:** build the sink.

**Example:** an MCP client subscribes to a query resource and receives
updates.

**Evidence:** `drasi-core/components/reactions/mcp/src/mcp.rs:1220-1321`.

**Done when:** a native MCP sink serves subscribe and read with authentication.

<a id="lr-11"></a>

#### platform: owner decision (LR-11)

**Related review finding:** [CR-P16](#cr-p16), compatibility timing placeholders;
the port/keep decision must distinguish those from actual measurements.

**Kind:** Decision. **Priority:** Low. **Effort:** Medium if ported.

The platform reaction is not published. Its `export_plugin!` is gated on a
`dynamic-plugin` feature that `Cargo.toml` does not define. The owner decides
with the Drasi Platform roadmap ([Sink D-7](#sink-d-7)). If it is ported, it
becomes a consumer using a Redis pipeline with control-event mapping.

**What remains:** record the decision, then port it or keep it ordinary.

**Example:** `cargo build -p drasi-reaction-platform --features dynamic-plugin`
fails because the feature does not exist.

**Evidence:** `drasi-core/components/reactions/platform/src/lib.rs:216-217`;
`drasi-core/components/reactions/platform/Cargo.toml`.

**Done when:** the decision is recorded, and the crate is either ported or
explicitly kept.

<a id="lr-12"></a>

#### snapshot-test: retire with the ordinary ABI (LR-12)

**Kind:** Issue. **Priority:** Low. **Effort:** Small.

This test-only reaction exists for ordinary-ABI snapshot and
initialization-failure tests.

**What remains:**

- Confirm that native fixtures cover the snapshot-reader and
  initialization-failure paths.
- Remove the crate with [CW-63](#cw-63).

**Example:** removing the crate should not reduce test coverage of
initialization failure.

**Evidence:** `drasi-core/components/reactions/snapshot-test/src/`.

**Done when:** the crate is removed and the native fixtures cover both paths.

## Testing and qualification

The work below improves what the existing tests prove and makes sure they
actually run, rather than adding test count. Recovery, wire-format, shared
semantic, browser and packaging tests protect different contracts, and backend,
Tokio-flavor and static/loaded permutations are not automatically redundant. A
passing run is not proof that a test asserts the right thing.

### Priorities and work ownership

| Priority | Work | Benefit | Effort |
|---|---|---|---|
| 1 | [Production-path credential tests](#cw-83) | Covers Key Vault/Azure identity request and error paths that no test exercises today | Medium |
| 1 | [Run existing suites in CI](#cw-85) | More real coverage without writing new tests | Medium |
| 1 | [Honest Server test runners](../../../drasi-server/docs/remaining-work.md#sw-22) (Server) | Stops misleading "all tests passed" results | Small-medium |
| 2 | [Remove proven repeated execution](#cw-86) | Less repeated container work | Small |
| 2 | [Shared scenario backend wiring](#cw-89), [native delta assertions](#cw-77), [tutorial result oracles](../../../drasi-server/docs/remaining-work.md#sw-23) (Server) | Catches output and storage regressions | Medium |
| 2 | [Build and fixture cost](#cw-87), [asynchronous reliability](#cw-37) | Potentially the largest speed gain | Medium-large; measure first |

Keep each change independently reviewable. Measure build and database phases
before pooling fixtures or relaxing test serialization.

### Test quality, selection and evidence

<a id="cw-83"></a>

#### Test real credential-provider behavior instead of copied implementations (CW-83)

**Kind:** Test-quality defect. **Area:** Core identity and secret stores.
**Priority:** High. **Effort:** Medium.

The Azure Key Vault secret store's unit tests do not test the production code.
They exercise a test-only `TestableProvider` that re-implements the URL
building, request, status handling and JSON parsing of
`AzureKeyVaultSecretStoreProvider::get_secret`. A bug introduced in the real
`get_secret` (for example, dropping `urlencoding::encode` or treating a 403 as
success) would not fail any test.

**What remains:**

- Rewrite the six wiremock tests (`test_get_secret_success`,
  `test_get_secret_404_not_found`, `test_get_secret_403_forbidden`,
  `test_get_secret_null_value`, `test_get_secret_malformed_json`,
  `test_url_encoding_prevents_path_traversal`) to construct the real
  `AzureKeyVaultSecretStoreProvider` with a fake `TokenCredential` and the mock
  server's URL. This needs a test-only (`#[cfg(test)]`) constructor, because
  `validate_vault_url` correctly rejects non-HTTPS, non-Azure hosts. Keep the
  existing URL-validation tests and do not relax the validation.
- Assert what the provider sends: the requested token scope, the
  `Authorization: Bearer <token>` header, the exact path and `api-version`
  query, and that the mock was matched exactly once (`.expect(1)`).
- Fix the path-traversal test. It mounts a 404 response for the escaped path
  and then checks the error mentions "404", but wiremock also returns 404 for
  an unmatched request, so the test passes even if the escaped path is never
  requested. Example: `get_secret("../certificates/cert")` must be observed as
  `GET /secrets/..%2Fcertificates%2Fcert?api-version=7.4`, with the mock
  verified as matched.
- Delete `TestableProvider` once the replacements pass.
- In the Azure identity provider, `test_with_workload_identity_creates_provider`
  discards the constructor result (`let _result = ...`) and proves only that it
  does not panic. Make it assert the expected `Ok`/`Err` for a controlled
  environment. The scope tests check only the stored `scope` field; add a test
  with a fake credential showing that `get_credentials` requests the
  `CredentialContext` `scope` override (or the default) and returns the
  expected username and token. Keep the existing Send/Sync and trait-object
  compile checks.

**Evidence:** production path
`drasi-core/components/secret_stores/azure-keyvault/src/lib.rs:184-226`;
`TestableProvider` at `:285-333`; copied-path tests at `:458-569`; live test
(checks only a nonempty value) at `:570-593`. Identity:
`drasi-core/components/identity/azure/src/provider.rs:134-150` (scope
override), `:194-199` (discarded result), `:238-260` (field-only scope tests).

**Done when:** each scenario runs through the production `get_secret` and
`get_credentials`; a deliberate change to the production request path, header
or status handling fails a test; and no copied provider implementation remains.

<a id="cw-85"></a>

#### Bind existing suites to explicit CI gates (CW-85)

**Kind:** CI selection gaps. **Area:** Core, Learning, test-infra and CI.
**Priority:** High. **Effort:** Medium.

Several existing test suites are never run by any pull-request workflow, so they
can break silently. Core's main Rust selection is in good shape: `make test`
(called from `drasi-core/.github/workflows/test.yml`) runs the whole workspace
plus every Host SDK target with `test-support,registry,fetcher,watcher`; a
features job covers `--no-default-features` middleware/backend combinations and
`parallel_solver`; `test-integration.yml` discovers container-backed ignored
tests; `test-ffi.yml` runs the Host SDK on Linux, macOS and Windows; and the
developer book check runs in `test.yml`. The gaps are elsewhere.

**What remains:**

| Repository | Suite not run in CI | Evidence |
|---|---|---|
| Core | Python tests for CI tooling: `ci/test_native_dependencies.py`, `ci/test_install_protoc.py`. Node test `xtask/src/runtime_architecture/report.test.cjs`. | No workflow or Makefile target references them (`grep` of `drasi-core/.github/workflows`, `Makefile`). |
| Core | `make test-ffi-layout-mismatch` (nightly-toolchain layout-randomization regression for issue #602). | `drasi-core/Makefile:116-134`; not referenced by any workflow. |
| Core | Native SQL Server tests and cross-OS network peers. They need explicit credentials or prebuilt peers, so the integration-matrix discovery deliberately skips files that read `DRASI_MSSQL_TEST_MODE` or `DRASI_NETWORK_CONTAINER`. | `drasi-core/xtask/src/integration_matrix.rs:59-70`; `make test-native-mssql` at `drasi-core/Makefile:90-92`. Needs a declared environment job (scheduled or manual) that reports "not run" rather than passing. |
| Learning | Deterministic tutorial tests: `tutorials/*/scripts/test_demo.py` (6 tutorials), `tutorials/sse-cli/tests/test_{build,cli}.py` and the SSE CLI's `cargo test`, and the shared `scripts/test_{local_build,graph_view,webhook_receiver}.py`. The repository has no pull-request workflow. | Its only workflow, `learning-drasi-server/.github/workflows/release-tutorials.yml:17-22`, is started by hand and packages the tutorials for a release. |
| test-infra | Rust unit tests for `e2e-test-framework` and `data-tools/arn`. PR workflows only build images (`build.yml`) and run clippy/fmt (`lint.yml`). The recovery-comparison Python tests run only in the manual `e2e-building-comfort-completion.yml`. | `test-infra/.github/workflows/build.yml:17-31,85-87`, `lint.yml:17-28`, `e2e-building-comfort-completion.yml:3-4,55-59`. |

**Before merging this branch to `main`:**

- `drasi-core/.github/workflows/ci-lint.yml:10-11` adds a TEMPORARY trigger
  for the recovery branch, and its clippy command drops the
  `drasi-source-salesforce/dynamic-plugin` feature that `main` checks. Remove
  the trigger and restore the feature (keeping `--locked`), then fix any
  clippy findings in that feature.
- PR #1034 was merged while its Rust Lint checks failed; lint is not a
  required check. Formatting and clippy now pass locally on the branch head.
  **Unverified:** the FFI (Linux, macOS, Windows) and unit-test workflow
  results for the current head; no workflow run on the branch was found.
  Run them, and consider making lint a required check.

Example of the gap: a change that breaks
`learning-drasi-server/tutorials/building-comfort/scripts/verify_demo.py`'s
arithmetic is caught by its `test_demo.py`, but no workflow runs that file, so
the PR stays green.

Notes for implementation:

- Run each tutorial's tests in its own process (`python3 -m unittest discover
  -s tutorials/<name>/scripts -p 'test_*.py'`); the directories reuse module
  names.
- Keep the agent evaluation as supplementary evidence, not the only tutorial
  gate.
- Release-level checks for [CW-69](#cw-69) and
  [CW-73](#cw-73) (downstream compilation, package/docs builds, publication
  metadata) are owned by those entries. The nightly `release-plz` dry run
  (`drasi-core/.github/workflows/nightly.yml:42-60`) already performs semver
  checks.
- Changed recovery/ownership code still needs its deterministic fault and
  crash-parent tests on the PR; do not move all recovery coverage to nightly.

**Done when:** each suite above has an owning workflow with an explicit
selector, its environment prerequisites are declared, and a skipped or
unavailable prerequisite is reported as not run rather than as a pass.

<a id="cw-86"></a>

#### Consolidate proven repeated test execution (CW-86)

**Kind:** Confirmed redundancy. **Area:** Core MySQL stored-procedure reaction.
**Priority:** Medium. **Effort:** Small.

The four self-tests of the MySQL test helper run three times per `cargo test`,
each starting its own MySQL container. `mysql_helpers.rs` sits directly in
`tests/`, so Cargo builds it as its own test binary, and both
`mysql_storedproc_tests.rs` and `mysql_storedproc_routing_tests.rs` also include
it with `mod mysql_helpers;`. Its `#[cfg(test)] mod tests` block (with
`test_setup_mysql`, `test_create_stored_procedure`,
`test_stored_procedure_operations`, `test_clear_procedure_log`) is therefore
compiled into all three binaries: 12 container-backed executions of four tests.

**What remains:** move the helper to `tests/common/mysql_helpers.rs` (or
`tests/mysql_helpers/mod.rs`) so it is no longer a standalone target, and move
its four self-tests into a single test file so they run once. Do not share
mutable database state between tests just to reduce the count.

**Evidence:**
`drasi-core/components/reactions/storedproc-mysql/tests/mysql_helpers.rs:378-482`;
`mysql_storedproc_tests.rs:23` and `mysql_storedproc_routing_tests.rs:22` in the
same directory.

**Done when:** `cargo test -p drasi-reaction-storedproc-mysql -- --list` shows
each of the four helper tests exactly once, and the stored-procedure and routing
tests still pass.

### Test reliability and execution cost

<a id="cw-37"></a>

#### Make asynchronous tests reliable (CW-37)

**Related review finding:** [CR-T02](#cr-t02), the ordinary stored-procedure
container retry workaround. Its Docker Desktop condition is not evidence of a
Podman failure.

**Kind:** Test-infrastructure improvement. **Area:** Core and CI.
**Priority:** Medium. **Effort:** Medium to large.

Core runs its whole test suite single-threaded (`--test-threads=1` in
`make test`, the features job and the FFI job) because some tests share ports,
files or global state, and many tests wait with fixed sleeps rather than on
completion signals. This makes CI slow, hides which tests actually conflict,
and lets timing-dependent tests pass or fail depending on machine load.

**What remains:**

- Identify the tests that genuinely need isolation (shared ports, files,
  process-global plugin callbacks, crash workers, resource probes) and isolate
  them with `serial_test` or a dedicated target. Then add a normal parallel CI
  run, keeping serialized execution only where it is still needed. This is the
  deferred
  [PR #988 parallel-testing follow-up](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591575864).
- Replace fixed sleeps that stand in for "processing finished" with explicit
  completion signals. The existing `QueryTestControl`
  (`drasi-core/lib/src/test_support.rs`) and its use in
  `drasi-core/lib-integration-tests/tests/future_notification_dedup.rs` show the
  pattern: fixed logical input times and explicit before-due/due checks instead
  of wall-clock waits. Negative observation windows ("nothing arrives within N
  ms") and external watchdogs are legitimate and must not be removed blindly.
- Unverified: replace any remaining tests that depend on the real calendar date
  with a controlled clock. None were re-identified during this review.

Example: `grep -rn "tokio::time::sleep\|thread::sleep"` over
`drasi-core/lib/tests`, `drasi-core/lib-integration-tests` and
`drasi-core/components/host-sdk/tests` finds 134 call sites; each needs to be
classified as either a completion wait (replace) or a deliberate negative
window/watchdog (keep).

**Evidence:** `drasi-core/Makefile:66-70`;
`drasi-core/.github/workflows/test.yml:40-55`;
`drasi-core/.github/workflows/test-ffi.yml:85`.

**Done when:** a parallel CI run passes reliably on both Tokio runtime flavors,
serialized execution is limited to tests with a documented shared resource, and
no assertion was weakened or timeout inflated to get there. Fixture cost and
duplicate execution are tracked in [CW-87](#cw-87) and [CW-86](#cw-86).

<a id="cw-87"></a>

#### Measure and reduce build churn and fixture overhead (CW-87)

**Kind:** Testing-cost improvement. **Area:** Core and CI.
**Priority:** Medium. **Effort:** Medium-large.

Most of the time in a full local Core test run goes into repeated compilation
and per-test database containers, not test bodies. Reducing that needs
measurement first; unsafe fixture sharing can hide cleanup and restart defects.

A recorded local run (shared target directory, serial builds) gave these
figures. Unverified: the logs are not checked in, so the numbers cannot be
re-derived from the repository.

| Phase | Cargo build time | Summed test-target time |
|---|---:|---:|
| Core workspace | 464 s | 794 s |
| Runtime-matrix discovery (listing only) | 2,862 s | — |
| Runtime-matrix execution | 52 s | 2,221 s |
| Host SDK complete run | 226 s | 1,332 s |

In that run, discovery compiled `drasi-lib` and `drasi-plugin-sdk` 22 times
each because successive package/feature selections changed the build graph.
`native_mysql_components` took about 1,021 s for 31 tests, and the MySQL
source lifecycle tests about 744 s.

**What remains, in order:**

1. Stabilize build graphs: group compatible package/feature selections so one
   build serves several test runs, and keep incompatible host/plugin targets
   separate. Do not count cache deletion as an optimization.
2. Measure database start, readiness, assertions and cleanup separately for the
   MySQL native and lifecycle suites before choosing a fix.
3. Apply [CW-86](#cw-86)'s duplicate removal.
4. Trial one shared database service with a fresh database per test for
   non-restart cases. The Garnet scenario tests already do this with a
   process-wide Redis container and per-test UUID scopes
   (`drasi-core/components/indexes/garnet/tests/scenario_tests.rs:37-64`). Keep
   dedicated containers for restart, binlog retention, corruption, credential
   and ownership tests. Verify shuffled/concurrent execution and cleanup before
   relaxing serialization ([CW-37](#cw-37)).

**Done when:** matched before/after measurements (cold and warm cache, same
feature groups) show shorter feedback without lost coverage, weaker isolation,
hidden cleanup failures or changed result oracles. Application performance is
[CW-40](#cw-40) and [CW-79](#cw-79), not this item.

### Semantic, application and API coverage

<a id="cw-89"></a>

#### Restore shared semantic and backend conformance wiring (CW-89)

**Kind:** Coverage wiring gap. **Area:** Core shared tests and indexes.
**Priority:** Medium-high. **Effort:** Medium.

Shared query scenarios in `drasi-core/shared-tests` are meant to run against all
three index backends: in-memory, RocksDB and Garnet. Nothing enforces this
(`drasi-core/shared-tests/AGENTS.md:5-6`), and many scenarios run only in
memory. A storage bug in, say, future-queue handling on RocksDB would not be
caught by those scenarios.

**What remains:** for each scenario below, wire it into the missing consumer(s)
or record why the backend does not apply (for example, a middleware-only test
that never touches the index).

| Missing from | Scenarios |
|---|---|
| RocksDB and Garnet (index-dependent: temporal, future-queue, aggregation or operators) | `crosses_above_a_threshold` (both variants), `crosses_above_and_stays_above`, `crosses_above_three_times_in_an_hour`, `decrease_by_ten` (both), `rolling_average_decrease_by_ten`, `exceeds_one_standard_deviation`, `future_aggregations::{truefor_sum, truefor_grouped_sum, truelater_max}`, `windows::{sliding_window_max, sliding_window_avg_grouped}`, `sensor_heartbeat::not_reported_with_true_now_or_later`, `logical_conditions`, `string_operators::{contains_op, starts_with, ends_with}` |
| RocksDB only | `steps_happen_in_any_order` |
| Garnet only | `prev_distinct::prev_unique_with_match` |
| RocksDB and Garnet (likely backend-independent; confirm and record) | `document`, `unwind` (3), `remap` (3), `decoder`, `parse_json_test`, `promote_test`, `relabel`, `dapr_state_store::run_tests` |

Example: `shared-tests/src/in_memory/mod.rs:627-629` runs
`future_aggregations::truefor_grouped_sum` against memory; neither
`components/indexes/rocksdb/tests/scenario_tests.rs` nor
`components/indexes/garnet/tests/scenario_tests.rs` calls it.

**Evidence:** the scenario lists above come from comparing the scenario calls in
`drasi-core/shared-tests/src/in_memory/mod.rs` with the two backend
`scenario_tests.rs` files. Native-query delta assertions are tracked in
[CW-77](#cw-77), not here.

**Done when:** every index-dependent scenario runs on all three backends, or the
backend's exclusion is stated next to the wiring with its reason.

<a id="cw-77"></a>

#### Complete query behavior coverage through the loaded plugin (CW-77)

**Related review findings:** [CR-T01](#cr-t01) is a separate ordinary
status-only join-test gap, not evidence that native semantic tests are absent;
[CR-K11](#cr-k11) distinguishes persistence assumptions from native Garnet
qualification or its explicit refusal.

**Kind:** Behavioral-parity qualification. **Area:** Core, Host SDK, test fixtures.
**Priority:** High before claiming complete native behavioral parity.
**Effort:** Medium to large, split by behavior.

Shared semantic cases run the same query scenarios through the linked reference
query and through the native query plugin (statically linked and loaded), on
memory and RocksDB, on both Tokio flavors. Each step can assert both the
resulting rows (`rows`) and the emitted change set (`changes`). Only
`canonical_replacement` and `ordered_batch` set `changes`; the other six cases
(`disconnected` for Cypher and GQL, `optional_list`, `assignment_maps`,
`numeric_count`, `optional_join`) check rows only. A plugin that produced the
correct final rows but emitted a wrong or extra delta (for example, an
`update` where an `add` plus `delete` is expected) would pass those six.

**What remains:**

- Add expected `changes` to every step of the six row-only cases, including the
  optional/disconnected transitions and the aggregate transitions in
  `numeric_count`.
- Extend native coverage to other supported behaviors not yet exercised through
  the loaded plugin, one behavior at a time, without inferring completeness
  from test names or counts.
- Native Garnet is not qualified: the current Garnet provider lacks an explicit
  restart-durability guarantee and the bounded reads the native query needs.
  `native_query_rejects_unqualified_garnet_without_touching_storage` proves the
  native query refuses it without writing; the
  `garnet_native_and_standard_adapters_..._reset_markers` test uses the linked
  transformer and does not count as native evidence. Qualify Garnet natively or
  keep the refusal explicit.

**Evidence:** `Step { rows, changes }` and the change assertion in
`drasi-core/lib/tests/fixtures/query_semantics.rs:49-67,137-160`; cases at
`:216-435` (`changes` set only at `:385` and `:454`); native runner
`drasi-core/components/host-sdk/tests/native_query_plugin/semantics.rs:10-99`;
Garnet `drasi-core/lib-integration-tests/tests/computation_garnet_recovery.rs:148-219`.

**Done when:** every shared semantic step asserts both rows and changes, each
supported behavior has an identified native case for its applicable
storage/runtime combinations, and unsupported profiles (including Garnet unless
qualified) are explicit. Shared backend wiring is [CW-89](#cw-89); CI selection
is [CW-85](#cw-85).

<a id="cw-10"></a>

#### Finish public status and result consistency checks (CW-10)

**Kind:** Qualification. **Area:** Core, Server.
**Priority:** High. **Effort:** Medium to large.

Library callers, the Server REST API, logs and the UI should report the same
state for a component or query. Edge cases (partial startup or shutdown,
replacement, restart, failure) are where these views are most likely to
disagree, and they are not systematically tested.

**What remains:**

- Test public outcomes for: partial startup/shutdown, missing or duplicate
  components, replaced query readers, missing result history, and races between
  the initial result snapshot and live updates.
- Define what the compatibility graph view (`DrasiLib::get_graph`) shows versus
  full native inspection. `ComponentKind` has only Instance, Source, Query,
  Reaction, BootstrapProvider and IdentityProvider, so a generic native
  transformer (for example a `drasi.standard/view` component) has no node kind.
  Either represent it or mark the snapshot as a partial projection so it is not
  presented as the complete graph. Unverified: whether native transformers are
  currently mapped to some other node kind or simply omitted.
- Cover these cross-entry conformance obligations:
  - [CW-22](#cw-22): the same classification, typed cause and operation context
    across library, REST, logs and UI.
  - [CW-23](#cw-23): snapshot/changed acknowledgement, coalesced readiness and
    failure, late callbacks, slow subscribers, reconnect and replacement;
    clients consume the snapshot rather than busy-loop, and coalesced status is
    not presented as a full event history.
  - [CW-34](#cw-34) and [CW-69](#cw-69): generated/property tests of streams,
    roles, references, cycles and unused bindings through equivalent entry
    points at the same validation phase; missing prerequisites remain a valid
    waiting state.
  - [CW-72](#cw-72): wrapped and unwrapped components advertise the same
    capabilities through failure, batching, progress, retirement, cancellation
    and scoped deletion; unsupported combinations reject rather than report
    successful cleanup.
  - [CW-26](#cw-26): exact metric counts, units and timing boundaries across
    normal work, retries, batching and failure. End-to-end completion is
    [CW-36](#cw-36).

**Evidence:** `drasi-core/lib/src/component_graph/node.rs:32-45,154-161`;
`drasi-core/lib/src/lib_core_ops/graph_ops.rs:55-57`.

**Done when:** for each scenario above, the library and Server report the same
truthful state and results through replacement, restart and failure, with a
test asserting each.

<a id="cw-13"></a>

#### Verify remaining query matching reports (CW-13)

**Related review finding:** [CR-T01](#cr-t01) identifies status-only join tests;
those checks cannot serve as result oracles for the reproductions below.

**Kind:** Investigation and fixes. **Area:** Core evaluator and parsers.
**Priority:** Conditional on reproduction. **Effort:** Medium to large.

Seven open issues report wrong results or runtime errors for queries that parse
and start successfully. Users get silently wrong rows or a query that stops
producing results.

**What remains:** reproduce each issue on the current branch, including changes
and removals, then fix it with a test or close it with evidence that it no
longer reproduces.

| Issue | Report | Open fix PR |
|---|---|---|
| [#795](https://github.com/drasi-project/drasi-core/issues/795) | `WHERE` on `OPTIONAL MATCH` drops the unmatched parent row | — |
| [#796](https://github.com/drasi-project/drasi-core/issues/796) | Outer variable in a node property map fails with `UnknownIdentifier` | — |
| [#797](https://github.com/drasi-project/drasi-core/issues/797) | Optional variable introduced after an aggregation fails with `UnknownIdentifier` | — |
| [#798](https://github.com/drasi-project/drasi-core/issues/798) | `IN` inside a list-comprehension filter gives the wrong result | — |
| [#807](https://github.com/drasi-project/drasi-core/issues/807) | Cyclic `MATCH` paths can count one relationship twice | [#912](https://github.com/drasi-project/drasi-core/pull/912) |
| [#808](https://github.com/drasi-project/drasi-core/issues/808) | `OPTIONAL MATCH` can lose or duplicate a row when partial paths cannot complete | [#915](https://github.com/drasi-project/drasi-core/pull/915) |
| [#812](https://github.com/drasi-project/drasi-core/issues/812) | Nested aggregation drops a grouping-key switch when the inner aggregate is unchanged | — |

Example (#795): with only `Team {id:'engineering'}` inserted,

```cypher
MATCH (team:Team)
OPTIONAL MATCH (person:Person)
WHERE person.teamId = team.id
WITH team, collect(person.id) AS personIds
RETURN team.id AS team, personIds
```

returns no rows; it should return `{team: 'engineering', personIds: []}`.

Quantified relationship patterns (variable-length paths) are rejected with a
typed unsupported-feature error and are not part of this item.

**Done when:** each issue has a merged fix with a regression test covering add,
update and delete, or is closed with evidence that it does not reproduce.

<a id="cw-12"></a>

#### Investigate reported errors in grouped results (CW-12)

**Related review findings:** [CR-C05](#cr-c05) and [CR-C06](#cr-c06) are
aggregation design questions, not demonstrated reproductions of the reported
bugs below.

**Kind:** Investigation and fixes. **Area:** Core evaluator and indexes.
**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

Four open issues report wrong or leaked state for grouped aggregations,
including one restart crash.

**What remains:**

| Issue | Report | Current state |
|---|---|---|
| [#384](https://github.com/drasi-project/drasi-core/issues/384) | A group whose aggregate returns to its identity value stays in the result set. Example: `MATCH (a) RETURN a.Name AS key, sum(a.Value1) AS my_sum`; insert `{Name:'foo', Value1:5}` then delete it; the result is `[{key:'foo', my_sum:0.0}]` instead of `[]`. | Open PR [#409](https://github.com/drasi-project/drasi-core/pull/409) ("Emit Removing when aggregating groups empty out") is not merged; `core/src/evaluation/parts/mod.rs` still uses the `default_after` path. |
| [#498](https://github.com/drasi-project/drasi-core/issues/498) | `avg()` fails with `InvalidArgument(0)` after a restart with RocksDB persistence when a contributing value is `null`. | Still present: `Avg::snapshot` falls through to `InvalidArgument(0)` for `VariableValue::Null` (`drasi-core/core/src/evaluation/functions/aggregation/avg.rs:268-283`), while `apply`/`revert` handle it. |
| [#806](https://github.com/drasi-project/drasi-core/issues/806) | A second aggregation can drop a valid zero-count row. | Open fix PR [#907](https://github.com/drasi-project/drasi-core/pull/907). |
| [#411](https://github.com/drasi-project/drasi-core/issues/411) | `PartCurrent` result-owner entries are never cleared, so persistent backends accumulate one entry per distinct group key ever seen. | Applies once #409 lands; re-check then. |

**Done when:** each issue has a fix with exact result and restart/recovery
checks (including old group-record cleanup on RocksDB and Garnet), or is closed
with evidence that it does not reproduce.

### Recovery, lifecycle and failure qualification

<a id="cw-14"></a>

#### Extend native-input recovery qualification (CW-14)

**Kind:** Qualification. **Area:** Core, test-infra.
**Priority:** Medium for broader coverage; high before claiming crash recovery
for a deployment profile that is not listed below. **Effort:** Large.

When a process is killed, input that a source has already accepted but the query
has not yet committed must be reconstructed by the next process: nothing lost,
nothing invented, no duplicate committed output. This is proven for a few
source/storage combinations; most connectors and the loaded query plugin's
admission path are not yet covered by a real process-kill test.

**Currently proven (real SIGKILL, fresh process, RocksDB, both Tokio runtimes):**

| Test | Covers |
|---|---|
| `drasi-core/lib-integration-tests/tests/native_input_crash_recovery.rs:1228-1325` | 28 cases: lossless admission QoS (including a lost admission reply), standalone retained input and a shared journal; eager and paged storage; kill before and after query commit. Compares outputs, identities, payloads and progress with an uninterrupted oracle run. |
| `drasi-core/components/host-sdk/tests/native_network/durable.rs:352-626` | Native HTTP and gRPC sources recover accepted input after process exit when the client never received its receipt. |
| `drasi-core/components/host-sdk/tests/native_query_plugin/durable_shared.rs:487-561`, `durable_bootstrap_crash.rs:54-152` | Loaded query plugin: shared-journal exit before/after commit; SQLite bootstrap exit while partial and after completion. |
| `drasi-core/components/sources/{http,grpc}/tests/wal_integration.rs` | Ordinary HTTP/gRPC source WAL crash/replay. WAL pruning waits for the initial subscriptions (`drasi-core/lib/src/sources/base.rs:589-640`). |
| `drasi-core/components/host-sdk/tests/native_storedproc_crash.rs` (RCA-5, Podman, `make test-native-storedproc-crash`) | Loaded MySQL and PostgreSQL stored-procedure consumers: a child process exits after the destination committed an operation but before the host recorded its progress, and after the last progress commit but before acknowledgement, for `exactlyOnce` (rows == inputs; empty cursor epoch) and `atLeastOnce` (inputs plus at most the one in-flight batch for a restarted in-memory producer under a new epoch; a persistent producer keeps its epoch and repeats nothing). SQL Server needs its explicit fixture and is not in this matrix. |
| `drasi-core/components/host-sdk/tests/native_paired_crash.rs` (RCA-4, RCA-6, Podman for PostgreSQL/MySQL, `make test-native-paired-crash`) | Loaded native PostgreSQL (`drasi/postgres-transactions`, coordinated), MySQL (`drasi.mysql/transactions`, `startFrom: snapshot`) and SQLite (`replay-transactions`, coordinated) sources with their paired provider node connected to the source's bootstrap input, a durable reference query (RocksDB, `auto_reset`) and a capture sink. A child exits (a) mid-snapshot with another provider page ready, (b) after every snapshot row before the completion commit, (c) right after the completion commit before any change-stream commit, and (d) after the query applied 2 of the 4 changes of one upstream transaction, before it commits it. Concurrent writes after the first snapshot page (PostgreSQL, MySQL) and a write while no process runs must arrive exactly once: replaying the delivered result envelopes over the completed initialization never adds a present row or touches an absent one and ends at the database's rows; (a)/(b) redo the initialization once (`reset_generation` 1), (c)/(d) do not; the transaction arrives as one 4-operation envelope and nothing else carries it; PostgreSQL slots never confirm past the WAL position preceding the uncommitted changes. With `strict` recovery a restart after (a)/(b) stops with `IncompleteBootstrap` for an operator reset instead (observed, not asserted). SQL Server needs its explicit fixture and is not in this matrix. |
| `drasi-core/components/host-sdk/src/computation/consumer_handoff_tests.rs` (`native_consumer_storage_failure_is_retryable_and_resumes_without_loss`, RCA-7) | Loaded native consumer (external and transactional) in a graph whose consumer-progress storage fails from the first completion commit (applied or refused) until it recovers: the node shows a retryable `Processing` failure and does not acknowledge its input; after recovery the graph's in-place retry reopens the fenced ledger and resumes in the same generation without loss or a second effect. Operator Retry (replacement) while storage is still down is not qualified: replacement waits on the sink's drain. |

**What remains:**

1. The 28-case matrix builds the query with the linked reference
   `ContinuousQueryTransformer::new` (`native_input_crash_recovery.rs:511-528`),
   not the loaded `drasi-computation-transformer-query` plugin. Run the same
   admission/retained/shared boundaries, including the lost-admission-reply case,
   through the loaded plugin (coordinate with [CW-77](#cw-77) and
   [CW-78](#cw-78)).
2. Add process-kill cases for the remaining durable connectors and initial-load
   paths: the shared and row modes of the native database sources (the
   paired-handover matrix covers the single-owner transaction modes), SQL
   Server once its fixture runs in CI, and the ordinary sources that advertise
   durable input.

**Example gap:** A producer admits 100 events through lossless QoS into a loaded
query plugin; the query has committed 60 when the process is SIGKILLed and the
admission reply for event 100 is lost. No test currently proves that a new
process emits exactly outputs 61-100 with the original identities and accepts the
producer's retry of event 100 as a duplicate rather than new work.

**Out of scope:** power-loss durability, memory-only queries, and exactly-once
external effects.

**Done when:** For each selected connector/storage profile, a fresh process after
SIGKILL restores outstanding input and stable retry identities without losing
accepted work, inventing progress or deleting unhandled messages. Duplicate
external effects are excluded only where the destination's declared
deduplication/transaction contract guarantees it; otherwise the test reports the
permitted at-least-once repeats.

<a id="cw-36"></a>

#### Make passing recovery checks prove completion (CW-36)

**Kind:** Qualification. **Area:** test-infra, Core.
**Priority:** High. **Effort:** Medium to large.

An end-to-end recovery test should pass only when every input was processed and
every output delivered exactly once, not merely when a final snapshot looks
right. test-infra has one such strict check (Building Comfort over ordinary gRPC
with a SIGKILL), but it does not run against the current Server, and every
other transport/deployment profile still relies on snapshot-only checks.

**Current state:**

- `test-infra/.github/workflows/e2e-building-comfort-completion.yml` (manual
  `workflow_dispatch` only) captures a completed 100,000-input reference, then
  requires exact SIGKILL recovery using
  `test-infra/e2e-test-framework/examples/recovery_comparison/qualify_building_comfort.py`
  and the `recovery-compare` tool (duplicates or reordering fail).
- A recorded macOS debug run (test-infra `13c24c5`) passed: all 99,981 room
  outputs and 49,860 aggregate outputs matched with no duplicates or reordering,
  and final state matched an independent replay. No Linux CI run of this
  workflow is recorded.
- `test-infra/.github/workflows/e2e-building-comfort-recovery.yml` (nightly) is
  still a snapshot/golden-equality gate, not a completion proof.

**What remains:**

1. Make the strict workflow work with the current Server. Server now refuses
   ordinary queries unless the native query plugin is loaded
   (`drasi-server/src/query_plugin.rs:11-19`), but the workflow builds only
   `drasi-source-grpc` and `drasi-reaction-grpc`
   (`e2e-building-comfort-completion.yml:60-72`) and the script copies only those
   two libraries (`qualify_building_comfort.py:313-321`). Build and ship
   `drasi-computation-transformer-query` with `dynamic-plugin`, then record a
   passing Linux run.
2. Apply the same completion check to the other required profiles: HTTP
   standard/adaptive, gRPC adaptive, the native HTTP/gRPC sources, and additional
   lost-reply windows.
3. Keep snapshot-only workflows labelled as such so they are not cited as
   completion evidence.

**Example gap:** Dispatching the completion workflow against current Server
`main` should fail at query creation with "The native query transformer plugin is
not loaded", so the only strict recovery proof cannot currently be reproduced.
Unverified: not executed.

**Done when:** Each required profile has a completion workflow in which an
incomplete capture cannot pass, with an exact expected result and a recorded
passing run against current code.

<a id="cw-20"></a>

#### Verify interrupted reset across storage providers (CW-20)

**Kind:** Qualification. **Area:** Core and persistent providers.
**Priority:** High. **Effort:** Large, mainly failure testing.

Resetting or removing a query deletes its saved rows, progress and output in
several steps. If the process dies partway through, the next start must either
finish the reset or refuse to start; it must never reopen half-deleted state as
healthy. This is proven for RocksDB only.

**Currently proven:**

- RocksDB, linked query: child killed after reset intent, before/after the wipe,
  and the same three points for deprovision
  (`drasi-core/lib/tests/computation_query_recovery/processes.rs:422-431`,
  current-thread runtime only).
- Loaded query plugin, non-atomic profile: process exits preserve publication and
  reset intent (`drasi-core/components/host-sdk/tests/native_query_plugin/non_atomic.rs:392-401`).
- Loaded query plugin, corrupt rows/output with AutoReset
  (`native_query_plugin/corrupt_recovery.rs:327-338`).
- RocksDB and Garnet checkpoint clear followed by in-process reopen
  (`drasi-core/components/indexes/rocksdb/tests/computation_result_transactions.rs:1688`,
  `drasi-core/components/indexes/garnet/tests/computation_resources.rs:301`); no
  process kill.

**What remains:**

1. Process-kill reset/deprovision cases for the other persistent providers:
   Garnet (linked path; the loaded plugin refuses Garnet, see
   `drasi-core/lib-integration-tests/tests/computation_garnet_recovery.rs:169`),
   the redb state store and the redb WAL.
2. The same kill points for the loaded query plugin's atomic profile, and on the
   multi-thread runtime.
3. Kill while a submitted storage delete is still in flight (cancelled caller),
   and repeated deprovision after a partial one.

A memory provider is not crash-durability evidence.

**Example gap:** A Garnet-backed query is reset; the process is killed after the
reset marker is written but before all index keys are deleted. No test shows that
the next start finishes the wipe or refuses, rather than serving the surviving
rows.

**Done when:** For each provider that advertises persistence, partly cleared
state cannot reopen as healthy, and explicit recovery restores correct rows,
progress and output identity.

<a id="cw-05"></a>

#### Extend safe-stop coverage to remaining shared resources (CW-05)

**Kind:** Qualification. **Area:** Core, shared resources and hosted instances.
**Priority:** High. **Effort:** Medium to large.

Stopping a component must not drop work it already accepted, and if a stop times
out the caller must be told who still owns the unfinished work. Single-component
graceful stop and a few shared paths are tested; several shared-resource
topologies are not.

**Existing coverage:** graceful drain and deadline refusal
(`drasi-core/lib/tests/computation_graceful_stop.rs:235-252`), source admission
feedback shutdown (`drasi-core/lib/tests/source_admission_feedback_shutdown.rs:543-559`),
native network source stop with in-flight commits
(`drasi-core/components/host-sdk/tests/native_network/durable.rs:310-348`), and
bidirectional native data draining before process exit
(`drasi-core/components/instance-host/tests/processes.rs:946`).

**What remains:** graceful and forced stop tests for:

- multi-stage required fan-out (one producer, several required consumers, one
  of them slow);
- a disconnected consumer that still has unacknowledged required work;
- components sharing one transaction domain or a component-private shared
  resource;
- a stop whose caller is cancelled or times out, verifying the resource stays
  owned and reported until the real stop finishes;
- the remaining storage providers (Garnet, redb).

**Example gap:** Source → transformer → two required sinks; sink B is slow. A
graceful stop with a 1 s deadline times out. No test checks that the undelivered
items for sink B stay owned and visible in status rather than being dropped when
the stop caller gives up.

**Done when:** No supported stop path abandons accepted work, hides incomplete
cleanup or interrupts a shared owner that was not reported.

<a id="cw-07"></a>

#### Verify overlapping application changes (CW-07)

**Kind:** Qualification. **Area:** Core management.
**Priority:** High. **Effort:** Large, mainly testing.

Saved configuration changes and start/stop/replace/remove requests can arrive at
the same time. The result must keep each accepted change owned, keep conflicting
changes ordered, and report exactly which changes applied and which failed.
Individual conflicts are tested; adversarial interleavings are not.

**Existing coverage:** concurrent revision writers
(`drasi-core/lib-integration-tests/tests/computation_management.rs:1203`),
concurrent add/rename (`drasi-core/lib/tests/computation_component_identity.rs:386`),
conflicting batch bindings (`drasi-core/lib/tests/configuration_batch_identity.rs:810`),
and stale-owner shutdown (`drasi-core/lib-integration-tests/tests/computation_managed_resources.rs:1075`).

**What remains:** schedule-controlled tests that interleave:

- a saved configuration change with start, stop, replacement and removal of
  the same and of unrelated components;
- failed construction and cancelled callers while unrelated operations continue;
- [CW-71](#cw-71) retention against a concurrent snapshot change, replay of an
  expired request (`RequestExpired`, `drasi-core/lib/src/management/store.rs:136-138`),
  namespace isolation, and key rotation or an unavailable key.

Keep the existing contracts (stale-preview rejection, isolated start groups);
do not replace them with all-or-nothing deployment rollback. Persistent restart
schedules belong to [CW-39](#cw-39).

**Example gap:** Client A saves a configuration that replaces query Q while
client B issues `stop Q` and client C removes Q's source. No test pins down the
final topology and the per-request results for every ordering of these three.

**Done when:** Accepted changes remain owned, conflicting changes stay ordered,
and reported results distinguish applied changes from failures for each tested
interleaving.

<a id="cw-08"></a>

#### Complete cleanup checks across native providers (CW-08)

**Related review finding:** [CR-K04](#cr-k04) provides ordinary-FFI evidence for
the retained-owner versus leak distinction. Normal forwarder cleanup is
improved; abandoned disposal remains separate from native qualification.

**Kind:** Qualification. **Area:** Core, providers, SDK/Host.
**Priority:** High. **Effort:** Large, mainly testing.

When a component is stopped or removed, its tasks, threads, connections and
storage handles must be released, or, if cleanup fails or is cancelled, kept
and reported so a later retry can finish. The loaded query plugin has this
coverage; other native components and storage providers do not.

**Existing coverage (query plugin):**
`drasi-core/components/host-sdk/tests/native_query_plugin/retained_recovery.rs:396-451`
(retained reads wait for Host cleanup after plugin stop; failed queries do not
expose uncertain state).

**What remains:**

1. Combined cleanup tests for the other native components (sources, sinks,
   view/scheduling transformers) on each storage provider they support.
2. Failed and cancelled cleanup that is retried: the resources and the original
   error must survive until a retry succeeds.
3. Count tasks, threads, connections, already-submitted blocking storage work and
   rejected construction results ([CW-17](#cw-17)). Report retained
   failed-cleanup owners separately from actual leaks. Rust task/FD checks do not
   cover foreign allocations or whole-process RSS.

**Example gap:** A native HTTP source's `stop` fails because its listener close
times out. No test checks that the port stays owned and reported, and that a
second `stop` releases it.

**Done when:** Each selected combination releases its actual resources or
reports unfinished ownership truthfully, including after repeated cleanup
attempts.

<a id="cw-39"></a>

#### Test the remaining combinations of failures (CW-39)

**Kind:** Qualification. **Area:** Core, providers, connectors and test-infra.
**Priority:** High. **Effort:** Large.

Most recovery tests inject one failure at a time. Real incidents combine them
(for example a crash while a destination reply is lost and a storage write
fails). This item adds tests for selected combinations, compared against an
independent oracle.

**What remains:** combination tests, each checking rows, progress, scheduled work
and external effects against an oracle:

- lost retained history together with a storage failure;
- wall-clock change together with restart;
- held late input across a crash;
- a disconnected consumer with damaged history;
- a lost destination reply together with a failed completion write;
- live changes arriving during bootstrap; multi-source snapshots where one source
  fails or is slow; an unavailable source position;
- repeated timer bursts; reconstruction after each transaction-participant
  boundary.

Apply the same matrix to newer capabilities instead of duplicating it under their
own entries: view-transform snapshots and lifecycle, [CW-71](#cw-71) persistent
housekeeping and key recovery, slow monitoring destinations (Server OTLP export), and
[CW-29](#cw-29)/[CW-74](#cw-74) capacity and rate/window bounds (with controlled
clocks through cancellation, slow consumers, failed replies and restart). The
mixed query/non-query scheduling chain is [CW-82](#cw-82).

A failed configuration read already fails start without wiping state for the
linked query path (`drasi-core/lib/src/queries/checkpoint_tests.rs:3049`).
Unverified: no equivalent test was found for the loaded query plugin; add one.

**Example gap:** A reaction's destination applies a batch but the reply is lost;
while the sink retries, the checkpoint write fails and the process is killed. No
test asserts the exact delivered set and checkpoint after restart.

**Done when:** Each selected combination has exact expected data, progress,
effects and cleanup outcomes rather than an inference from single-failure tests.

<a id="cw-03"></a>

#### Check remaining extension failure boundaries (CW-03)

**Kind:** Investigation/hardening. **Area:** Core, providers and plugin boundaries.
**Priority:** High. **Effort:** Medium to large.

A panic inside a factory, provider, validation or destructor callback must not
take down unrelated components or leave a damaged object in use. Core runs
component callbacks through panic-catching wrappers
(`drasi-core/lib/src/computation/v1/graph/execution.rs:89-261`) and the plugin
SDK/Host wrap FFI callbacks, but no audit has confirmed that every callback path
is covered.

**What remains:** List factory `create`, provider (index, state store, WAL),
data-validation and drop/destroy callbacks reachable from Core and Host, and
mark which run inside a catch. For each uncovered path, write a test that panics
there; add protection only where the test shows a real unprotected failure.

**Example check:** an index provider whose `create_indexes` panics during query
construction should produce a failed component with a diagnostic, while other
components in the graph keep running.

**Done when:** Each audited callback has a test or a documented reason it is
safe; failures produce useful diagnostics, damaged objects are not reused, and
unrelated healthy work continues.

**Resolved (RCA-8): panics in owned workers.** A panic inside a worker
registered with `workers::spawn_owned_worker` (or its `try_`/blocking variants)
from a native operation is reported to the SDK instance when it happens; the
Host waits on the independent `WORKER_FAILURE` operation through
`ComputationComponent::worker_failures` while the node processes, so the node
fails within a bounded time as a retryable `Processing` failure, withdraws its
readiness, and Retry starts it cleanly
(`drasi-core/components/host-sdk/tests/native_worker_failure.rs`, loaded on both
Tokio flavours; `drasi-core/lib/tests/computation_worker_failures.rs`). Workers
started with plain `tokio::spawn` remain unobserved until joined.

<a id="cw-21"></a>

#### Investigate remaining retry and completion questions (CW-21)

**Kind:** Investigation. **Area:** Core merge, admission and delivery recovery.
**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

These are open questions about whether retries and completion confirmations are
classified correctly. Each needs a reproduction or an evidence-backed closure.

**Questions:**

1. **Delivery confirmation content (linked query path).** The stateful
   transformer runtime the loaded query plugin runs on passes the pending
   envelope to `validate_delivery`, so a confirmation whose operations differ
   from the pending output is rejected
   (`drasi-core/components/computation-plugin-sdk/src/transformer/engine.rs:1213`,
   `drasi-core/components/computation-plugin-sdk/src/transformer/output.rs:601`). The linked
   query path passes `None` (`drasi-core/lib/src/computation/v1/query_delivery.rs:383-389`),
   so only identity, generation and sequence are checked. Decide whether to add
   the content check or accept the gap until [CW-80](#cw-80) retires that path.
2. **Retries without an original timestamp.** Unverified which component this
   applies to; confirm whether a retried admission/replay that lacks the
   original source timestamp can be reordered or treated as late by the
   time-based merge.
3. **Clock changes during time-based merging.** The merge's grace deadlines use
   the monotonic `tokio::time::Instant` and restart from activation
   (`drasi-core/lib/src/computation/v1/time_merge.rs:21,265,736`), so wall-clock
   jumps should not affect them. Confirm with a test or close with that rationale.
4. **Uncertainty for writes never attempted.** Check whether a storage commit that
   was rejected before submission (for example cancelled while waiting for the
   mailbox) is reported as "uncertain" rather than "rejected"
   (`drasi-core/lib/src/computation/v1/qos_pipe/ingress.rs:739`,
   `admission.rs:407`).

Expired replay/request identities already reject explicitly in QoS admission,
delivery, time merge and the management store, so they are not listed here.

**Done when:** Each question has a clear result distinguishing rejected, saved
and genuinely uncertain work, with a fix where a failure is reproduced.

<a id="cw-35"></a>

#### Resolve the remaining ownership and limit findings (CW-35)

**Kind:** Investigation (two items confirmed by code reading). **Area:** Core
lifecycle and storage. **Priority:** Conditional; low practical impact for item 1.
**Effort:** Small for items 1-2; unknown for the rest.

These are specific review questions about lifecycle state, removal policy and
ownership at failure boundaries. Two are visible in the code; the others need a
reproduction before any change.

**Confirmed by code reading:**

1. **`open_run` sets `Starting` before validating the epoch.**
   `drasi-core/lib/src/computation/v1/graph.rs:2113-2123` replaces the state
   with `GraphState::Starting` and then increments the `u64` `run_epoch` with
   `checked_add`. On overflow it returns an error but leaves the graph in
   `Starting`, and later `open_run` calls reject that state. Overflow of a `u64`
   counter is practically unreachable; fix by validating before mutating and add
   a unit test.
2. **`Unbind` ignores `RemovalPolicy::Reject`.** The reference documentation says
   Reject refuses "a change that breaks required dependencies"
   (`drasi-core/lib/docs/computation-graph-reference.md:432`;
   `drasi-core/lib/docs/developer-guide/README.md:5732`), and component removal
   honours it (`drasi-core/lib/src/computation/v1/graph/reconcile.rs:465-480`).
   `DesiredMutation::Unbind` with `Reject` removes the relationship
   unconditionally (`reconcile.rs:696-726`), even if it is
   `required_for_binding`. Decide what Reject means for Unbind (for example,
   refuse when the consumer still requires the edge or has waiting work), then
   align code, docs and tests.

**Needs reproduction:**

3. Can a transient control failure leave a component blocked after its
   prerequisite recovers, with no retry or reconciliation path?
4. Can a rejected addition/resource, or competing reaction deprovision paths,
   release a claim too early or keep an unreported owner? Unverified: the graph
   reaction adapter does not own deprovision
   (`drasi-core/lib/src/computation/v1/legacy_reaction.rs:28-30`); the question
   may now apply only to the ordinary reaction manager.
5. Does a timeout or cancellation during a query source-transaction storage
   operation keep the submitted operation owned, and its outcome reported as
   uncertain, until it actually completes?

Missing prerequisites and retained/fenced cleanup ownership are expected
behaviour, not failures.

**Done when:** Each item has a fix with a test or an evidence-backed closure.

<a id="cw-82"></a>

#### Prove a mixed query and non-query scheduled pipeline (CW-82)

**Kind:** Optional cross-component qualification. **Area:** Core, native fixtures.
**Priority:** Required before advertising a combined query + non-query scheduling
profile. **Effort:** Medium.

Scheduled (timer-driven) work is qualified separately for queries and for a
standalone non-query transformer
(`drasi-core/components/host-sdk/tests/native_scheduling.rs`, which states it is
not this profile). No test runs both in one persistent application, so it is
unproven that their checkpoints, pending work and outputs recover consistently
together.

**What remains:** one test application built from existing parts, with no new
connector, evaluator or graph owner:

- native SQLite source with bootstrap
  (`drasi-core/components/host-sdk/tests/fixtures/native_sqlite`);
- the stateful non-query scheduling transformer
  (`drasi-core/components/host-sdk/tests/fixtures/native_scheduling.rs`);
- the loaded query plugin;
- a handled capture sink;
- real persistent (RocksDB) storage.

Scenarios: bootstrap while live input continues (exact rows, gap-free handover);
cancel initialization (ownership retained, cleanup awaited); schedule, replace and
cancel work alongside state changes (rollback leaks neither state nor timers);
advance the clock with no new input and check both components' due output and
provenance.

**Example:** The SQLite source bootstraps 1,000 rows; the scheduling transformer
delays each by 5 s; the query aggregates the delayed rows. The process is killed
after the transformer commits a scheduling request but before the query commits
the resulting output. After restart, the due item must be emitted once and the
aggregate must match an uninterrupted run.

**Done when:** Both Tokio runtimes pass real subprocess exits around bootstrap
completion, scheduling-request commit and due-work/output commit. Fresh processes
recover exact checkpoints, pending work, rows and output identities without
duplicate committed completion or abandoned cleanup. Other multi-failure
scenarios remain in [CW-39](#cw-39).

<a id="cw-111"></a>

#### Qualify chained bootstrap and subscription topologies (CW-111)

**Kind:** Qualification. **Area:** Core, Host SDK. **Priority:** High before
claiming support for multi-stage query pipelines. **Effort:** Medium.

Each consumer opens its own bootstrap session with its upstream, receives a
snapshot and a cut-off watermark, and then drops live input at or below that
watermark. Bootstrap evaluation is not emitted on the output pipe. This handover
is tested for chains that start at a source (relay fan-out, fan-in of two
relays, a late consumer off a relay, restart via a relay) but query-to-query
bootstrap is tested for only two hops: once in-process
(`source_relay_and_upstream_query_snapshot_merge`) and once with two volatile
loaded queries (`native_query_plugin/upstream_snapshot.rs`).

**What remains** (build a topology matrix, linked components first, then loaded
plugins; harnesses: `lib/tests/computation_graph_bootstrap.rs` helpers,
including late join via `control.add_component` + `control.connect`, and
`native_query_plugin/{upstream_snapshot,durable_chain,durable_sqlite,middleware_bootstrap}.rs`):

1. Three or more queries in a row over bootstrap edges.
2. A consumer added to a running chain off an upstream query or transformer,
   not only off a source relay.
3. Restart and process kill partway down a durable chain over edges (see
   [CW-109](#cw-109)).
4. One upstream query bootstrapping several downstream queries (fan-out over
   query edges).
5. One query bootstrapping from two upstream queries (fan-in over query edges).
6. An end-to-end chain into a sink, checking how initial state reaches it (see
   [CW-110](#cw-110)).
7. The refusals: `query → middleware`, and persistent-from-volatile once it is
   a binding refusal ([CW-106](#cw-106)).

**Known limits that bound this work:** [CW-92](#cw-92) (no graph-wide
rebootstrap), [CW-106](#cw-106) and [MY-36](#my-36) (progress persistence not
compared along edges), [CW-104](#cw-104) (late progress binding hold),
[CW-94](#cw-94) (no live rebinding), [CW-82](#cw-82) and [CW-05](#cw-05).

**Evidence:** `drasi-core/lib/tests/computation_graph_bootstrap.rs:607`
(two-hop merge), `:770` (late consumer via relay only);
`drasi-core/components/host-sdk/tests/native_query_plugin/upstream_snapshot.rs`;
`drasi-core/lib/src/computation/v1/graph_bootstrap.rs:62` (`served_source`),
`:611-660` (`serve_output_snapshot`, `BootstrapSnapshotServer`).

**Done when:** every topology above passes with independent result oracles
(no loss, no duplicate across the snapshot/live boundary) on both Tokio
flavours, through linked components and loaded plugins, or is refused at
binding with a documented reason.

### Native boundaries, platforms and execution qualification

<a id="cw-41"></a>

#### Complete checks using real plugin libraries (CW-41)

**Kind:** Safety audit/qualification. **Area:** ABI, SDK/Host, instance-host and contracts.
**Priority:** High. **Effort:** Large.

Most boundary tests construct plugins from linked Rust definitions, so they do
not prove that a separately built `cdylib` loaded through the C ABI rejects bad
input, honours cancellation and releases its resources. The unsafe FFI code in
the ABI, SDK, Host and instance-host has also not had a complete safety review,
and there is no fuzzing of the binary decoders.

**What remains:**

1. **Loaded-library contract matrix.** Using independently built dynamic
   libraries, test malformed data, ABI/version rejection, cancellation, delayed
   callbacks arriving after a replacement instance has started, and repeated
   create/dispose cycles. Measure per-instance resource release and distinguish
   it from libraries deliberately kept loaded for the process lifetime.
2. **Conformance kit through loaded libraries.** `SourceConformance` and
   `SinkConformance` (`drasi-core/components/computation-plugin-sdk/src/testing.rs:162,464`)
   are only run against linked definitions (the HTTP, gRPC, SSE and log plugin
   `tests.rs` files). Run them through freshly built dynamic libraries too.
3. **Ingress rejection has no graph effect.** HTTP and gRPC source tests check
   401 + `WWW-Authenticate`, 429 and gRPC `UNAUTHENTICATED`/`RESOURCE_EXHAUSTED`
   plus TLS/mTLS listeners in-process
   (`drasi-core/components/computation-plugins/sources/http/src/tests.rs`,
   `.../sources/grpc/src/tests.rs`). Add loaded-library tests asserting that a
   rejected single-event, batch or producer-session request produces no graph change.
4. **Connector parity ([CW-44](#cw-44)).** Compare required legacy behaviour
   against real native sources, initial-data providers and sinks, static and
   dynamic, on both Tokio flavours and with actual Server consumers: failure,
   cancellation and cleanup, bounded CDC paging, streamed snapshots,
   credentials, schema catalogs and procedure effects. Move old-interface test
   consumers to native construction, keeping their useful assertions. Restart
   ownership is shared with [CW-14](#cw-14).
5. **Format fixtures ([CW-18](#cw-18), [CW-19](#cw-19), [CW-76](#cw-76)).**
   Keep fixed byte fixtures for envelopes, graph definitions, QoS/delivery
   metadata and query state; check version/header dispatch happens before payload
   decoding and that refusal does not reset valid state. Cover oversize/over-deep
   input and drop on error/cancellation paths, and boundary-size or malformed
   fragments across snapshot, replay, recovery and follow without partial results,
   lost acknowledgements, unbounded allocation or leaked provider retention.
6. **Unsafe-code review** requested in the
   [PR #988 native FFI safety review](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591475780):
   inventory unsafe operations in the ABI, SDK, Host and instance-host; check
   each pointer's lifetime, callback/release order and cross-thread
   `Send`/`Sync` use against the opaque-handle contracts; add a failing test for
   every uncovered contract. A `SAFETY` comment is not proof of review, and an
   unsafe count or a missing defensive null check is not a finding without a
   reproduction through a supported call.
7. **Fuzzing and model checking.** No fuzz, proptest/quickcheck, loom or Miri
   setup exists in the workspace (custom seeded/model tests do). Fuzz
   envelope, binary, graph-change, query and wire decoding including error-path
   drop and bounded allocation; use Miri for applicable unsafe helpers and loom
   (or equivalent) for acknowledgement/journal/pipe interleavings. Add tooling
   only where it covers a named missing boundary.

Query-specific behaviour coverage belongs to [CW-77](#cw-77).

**Example:** load a source `cdylib`, start instance A, replace it with B, then
let A's delayed callback fire. Today no test proves the Host drops A's callback
rather than applying it to B's graph.

**Done when:** The selected loaded libraries reject invalid calls, release
per-instance resources, and never let an old instance's callbacks act on its
replacement; the unsafe inventory is complete with tests for each contract; and
the decoders have fuzz targets. Fixture runs (normal, repeated, failed-start)
remove only the containers and volumes they own. An environment failure or a
skipped database case does not count as a passing run.

<a id="cw-67"></a>

#### Qualify the pinned CI Protocol Buffers compiler (CW-67)

**Kind:** CI qualification. **Area:** Core native/coverage workflows.
**Priority:** Medium. **Effort:** Small to medium.

CI now installs a pinned, checksum-verified `protoc`, but the setup has only been
exercised locally (macOS and Linux ARM64). It has not run on GitHub-hosted
runners, and never on Windows, so it is unknown whether the FFI and coverage
workflows actually build with it.

**Current setup:** `drasi-core/.github/actions/setup-protoc/action.yml` first runs
`ci/test_install_protoc.py` (corrupt archive, wrong version, unsupported platform,
existing directory) and then `ci/install_protoc.py`, which installs protoc 36.2
(`ci/install_protoc.py:19`) with per-platform SHA-256 pins and exports
`PROTOC`, `PROTOC_INCLUDE` and `PATH`. It is used by `test-ffi.yml`
(Linux x86_64, macOS arm64, Windows x86_64 matrix at `:28-41`),
`coverage.yaml` (both jobs) and `test-integration.yml`.

**What remains:** Run those workflows on their real runners and fix any failures.
`test-ffi.yml` runs on pull requests to `main`, `feature/*` and `release/*` or on
`workflow_dispatch`; this branch has no recorded workflow runs. Remaining CI
supply-chain points from the
[PR #988 review](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591475780)
should be checked against the result.

**Example:** `gh workflow run test-ffi.yml --ref <branch>` should show
`libprotoc 36.2` in the setup step on all three runners and a green FFI build.

**Done when:** CI logs record the expected compiler version on Linux, macOS and
Windows, the installer's negative tests pass in CI, and the cross-platform
native build/wire checks pass.

<a id="cw-91"></a>

#### Qualify isolated hosts and prepared-peer deployment profiles (CW-91)

**Kind:** Platform/network qualification. **Area:** instance-host, bridge, Server and CI.
**Priority:** Required before advertising an additional profile. **Effort:** Large.
**Depends on:** The selected platform/containment capabilities in [CW-75](#cw-75).

Running graph instances in child processes or on prepared remote peers is only
tested on Unix and on local/VM networks. Any other platform or network profile
must not be advertised until it is tested; Windows cannot be inferred from
compilation or from a Unix run.

**Current state:** The process host is Unix-only
(`drasi-core/components/instance-host/src/lib.rs:8-17` are `#[cfg(unix)]`).
Process tests are in `components/instance-host/tests/processes.rs`; the cross-host
network test requires a prepared Podman peer named by `DRASI_NETWORK_CONTAINER`
(`components/instance-host/tests/network/mod.rs:397`, setup in
`components/instance-host/README.md:376-385`).

**What remains:** For each platform/provider to be advertised, prove:

- parent/child/grandchild ownership and cleanup on parent death;
- child graph failure while its PID survives;
- version and authentication rejection, lost admission receipts and stale connections;
- namespaced persistent reopen and interrupted reset;
- resource use measured across the whole process tree;
- prepared-peer links across real network partitions and prolonged disconnects,
  with a partition reported as unknown remote state — not as a stopped peer and
  not as permission to launch an unfenced duplicate.

Keep kernel-containment, in-process unwind and native-abort boundaries explicit.
This adds no remote method executor, distributed transactions or automatic remote
provisioning. Compiler/build-wire checks are [CW-67](#cw-67); the loaded-library
contract matrix is [CW-41](#cw-41).

**Example:** kill the parent Server while a child instance host is mid-commit
over a partitioned link; the peer must report unknown state and the restarted
parent must not start a second writer for the same namespace.

**Done when:** Current-source evidence supports each advertised hosting and
network profile; unavailable environments are listed under [CW-78](#cw-78)
rather than counted as passes.

<a id="cw-78"></a>

#### Run the complete existing suites on the current query integration (CW-78)

**Kind:** Execution/CI qualification gap. **Area:** Core, Server, CI.
**Priority:** High before declaring the current code fully tested.
**Effort:** Medium plus any reproduced failures.

The existing Core and Server suites have been run locally, but several required
cases need environments that were not available, so the code cannot yet be
called fully tested on all supported platforms. These cases are skipped or
gated today and must not be counted as passes.

**What remains:** Run the following in their required environments, fix any
failures, and record results against the current revision:

| Gated cases | Location | Requirement |
|---|---|---|
| Real SQL Server source/sink (static and dynamic, both runtimes) | `drasi-core/components/host-sdk/tests/native_mssql_components.rs:335-351` (ignored); `drasi-server/tests/native_mssql_test.rs` (needs `DRASI_MSSQL_*` settings) | User-approved isolated SQL Server environment |
| SQL Server connector integration tests | `drasi-core/components/sources/mssql/tests/integration_test.rs` (e.g. `:1989`) | `#[ignore]` and also compiled out on `aarch64` by `#[cfg(not(target_arch = "aarch64"))]`, so on ARM they do not even appear in ignore totals; needs an x86_64 run with SQL Server |
| FFI layout-mismatch tests | `drasi-core/components/host-sdk/tests/ffi_layout_mismatch_test.rs:78,184` | Nightly `-Zlayout-seed` + `MALLOC_CHECK_` via `make test-ffi-layout-mismatch` (`Makefile:116`); not in any workflow |
| Cross-OS instance-host network case | `drasi-core/components/instance-host/tests/network/mod.rs:397` | `DRASI_NETWORK_CONTAINER` Podman peer |
| Live credential/OS integrations | e.g. keyring missing-key case `drasi-core/components/secret_stores/keyring/src/lib.rs:135-182`; Azure Key Vault `drasi-core/components/secret_stores/azure-keyvault/src/lib.rs:571` (`DRASI_TEST_AZURE_KEY_VAULT_URL` and Azure credentials) | Real OS keychain / cloud credentials |
| Oracle source integration tests | `drasi-core/components/sources/oracle/tests/integration_test.rs:115,280,435,687,895,1069` | Oracle container plus Oracle Instant Client (`libclntsh`) on the host |
| Live market-data sources | `drasi-core/components/sources/sui-deepbook/src/grpc.rs:379`, `.../sui-deepbook/tests/integration_test.rs:346`, `drasi-core/components/sources/hyperliquid/tests/integration_test.rs:15` | Network access to Sui and Hyperliquid mainnet with live activity; results depend on external traffic |

Native Garnet currently refuses restart; that refusal test is not positive
restart qualification. The full suites should also be re-run on Linux x86_64
and Windows, not only on the local macOS/ARM machine.

**Example:** on an Apple Silicon machine,
`cargo test -p drasi-source-mssql --test integration_test` reports success while
running none of the SQL Server integration cases, even with `--ignored`.

**Done when:** All existing required suites pass with traceable current-revision
results; anything still unavailable is named as a blocker, not an exemption.
Preserve parent-driven subprocess fixtures and real backend cleanup. Test
selection policy is [CW-85](#cw-85). New
native assertions belong to [CW-77](#cw-77), Server failure scenarios to
[SW-21](../../../drasi-server/docs/remaining-work.md#sw-21), and live example
acceptance to [SW-23](../../../drasi-server/docs/remaining-work.md#sw-23).

### Application and native-query performance qualification

<a id="cw-40"></a>

#### Measure larger and longer-running applications (CW-40)

**Kind:** Performance qualification. **Area:** Core, Server, test-infra.
**Priority:** High for release claims. **Effort:** Large.

There is no reproducible performance comparison between this branch and `main`,
and no agreed limits for latency, CPU, memory or disk at realistic application
sizes. Without them, no release can make performance or scalability claims.

**What remains:**

1. Agree acceptable limits (response time, processing cost, memory, disk) per
   selected deployment profile before measuring.
2. Build a reproducible comparison against `main`, pinning both revisions,
   toolchain, features, input and expected-result oracle, providers and storage
   guarantees. Check output equivalence before comparing throughput.
3. Measure throughput, end-to-end and control tail latency, CPU, allocations,
   memory/disk growth and cancellation responsiveness, covering: one vs many
   queries, small vs large payloads, fast vs slow fan-out, recovery disabled vs
   enabled, both Tokio runtime flavours, shared storage, outages and prolonged
   operation.

**Existing tools to reuse:** `drasi-core/lib/tests/measure-core-workloads.py`
(release workloads) and `drasi-core/lib/examples/core_resource_soak.rs` (resource
soak; measures live Rust allocations, tasks and descriptors, not foreign
allocations or RSS). The CPU-scaling reference in the
[maintainers guide](computation-graph-maintainers-guide.md#measure-the-path-actually-changed)
is a separate bounded acceptance workload, not a general speedup claim. The
legacy `drasi-core/query-perf/run_perf_tests` launcher (four scenarios ×
memory/RocksDB/Redis) is not a correctness gate; its defects are in
[SW-22](../../../drasi-server/docs/remaining-work.md#sw-22) — keep its scenarios
and backend distinctions if it is replaced. Test-runner build cost is
[CW-87](#cw-87); native-query vs linked-query overhead is [CW-79](#cw-79).

**Example:** 50 continuous queries over one source at 5,000 changes/s for 24
hours on RocksDB with recovery enabled, run on both this branch and `main`,
reporting p99 end-to-end latency, RSS and disk growth against agreed limits.

**Done when:** Each selected deployment profile has repeatable measurements,
exact result checks and documented operating limits.

<a id="cw-79"></a>

#### Measure native query overhead against the linked implementation (CW-79)

**Kind:** Performance qualification and measured fixes. **Area:** Core, Host SDK.
**Priority:** High before claiming performance parity. **Effort:** Medium.

Running queries as native plugins adds ABI, codec and service-boundary costs
that have not been measured against an equivalent in-process (linked) query.
Until they are, performance parity with the linked implementation cannot be claimed.

**What remains:**

1. Run matched workloads — graph-driven projection, joins/aggregates, scheduled
   output, large reads and slow-consumer/backpressure — with identical inputs,
   providers, persistence guarantees and expected results. Pin revisions,
   toolchain and features; use the pre-plugin revision where a linked baseline is
   needed. A different recovery or storage profile is not a fair baseline.
2. Measure throughput, end-to-end/control tail latency, CPU, allocations, memory,
   evaluator setup count and codec/service-boundary cost on both runtimes.
   Separate static native-ABI overhead from dynamic-loading overhead.
3. Agree workload-specific bounds, then fix material regressions in Core/shared
   helpers — not by adding another evaluator, a cached row mirror or weaker recovery.

Existing starting points: `drasi-core/components/host-sdk/examples/native_fast_path.rs`
(recovery-disabled native arithmetic through the cdylib ABI) and
`drasi-core/lib/examples/fast_path.rs`. Unverified: whether a linked query can
still be constructed on the current revision for a same-revision comparison,
or whether the pre-plugin revision must be used.

**Example:** a two-hop join with `count()` aggregation over 1M changes, run as a
loaded native query and as the linked query with the same RocksDB recovery
settings, comparing p99 latency and allocations per change.

**Done when:** Reproducible matched runs meet the agreed bounds with exact
results. Overhead outside the bounds stays recorded as reproduced work; a report
alone is not parity. Larger deployments and long-duration comparisons against
`main` are [CW-40](#cw-40).

<a id="testing-audit-evidence"></a>

### Known testing limitations

These limits apply to the current test suites and should be kept in mind when
citing test results:

- No mutation testing, coverage-gap study or flakiness study has been done;
  passing tests are not proof of assertion quality.
- No fuzz, property-testing or Criterion harnesses exist; custom seeded/model
  tests and release workloads exist instead (see [CW-41](#cw-41), [CW-40](#cw-40)).
- JavaScript, tutorial, GPU Lab, Move-a-wall, cloud-provider and test-infra E2E
  behaviour has not been re-run against the current revision.
- Environment-gated cases (SQL Server, FFI layout seed, cross-OS network,
  live credential/OS, Oracle, live market data, native Garnet restart) are
  listed in [CW-78](#cw-78).
- `cargo test --workspace -- --include-ignored` also compiles doc examples
  marked ```` ```ignore ````, which are not meant to build, so doctests fail in
  that mode only. Run ignored tests with `--lib --tests` or per target.
- Some tests named "native" still use the linked query; only targets such as
  `drasi-core/components/host-sdk/tests/native_query_plugin.rs` load actual
  factories. Check construction before citing a test as native-path evidence.
- Standalone GDELT/Wikidata generated-file tests in `test-infra` and the
  standalone GPU Lab workspace are outside their parent Cargo workspaces and run
  only when explicitly selected.

<a id="code-review-findings"></a>

## Items derived from a review of existing code

This section incorporates all **59 retained findings** from the review of
implementation comments and adjacent unfinished paths: **41 Present**,
**7 Partial** and **11 Uncertain**. It covers the query engine, shared value
types, Lib, ordinary plugins and their supporting crates, not only native
ComputationGraph plugins. Definitely resolved or obsolete findings are excluded.

**Evidence basis:** the original scan of main
`5ec0ed68c158ba7b654d6b26ccc70a183f941a48`, reassessed against
`fe005b97526a9453381a8f6b8785929ed7ef5996` and the working-tree changes.
This is source-based evidence, not a new runtime qualification. Source locations
below are relative to `drasi-core/`; moved files were checked at their current
locations rather than treated as resolved.

**How to read these items:**

- **Present:** the limitation, compromise or workaround remains observable.
  This does not by itself establish a bug or mandate its replacement.
- **Partial:** related improvements exist; only the remaining concern is
  retained, not the part already fixed.
- **Uncertain:** an open design question or unresolved applicability needs
  evidence or a requirements decision before implementation.
- **Related work:** an overlap, owning-area follow-up or native counterpart,
  not a claim that two entries describe the same defect. Ordinary-only findings
  remain distinct from native capabilities.

Ordinary-plugin findings inform [CW-63](#cw-63)'s retirement work and
[Sink D-7](#sink-d-7)'s port/keep/retire decisions; they are not a blanket plan to
extend legacy plugins. Working dependency adaptations remain review decisions,
not missing features. In particular, incompatible saved formats may be
explicitly refused under [CW-18](#cw-18); automatic migration of old state is
not a requirement.

<a id="code-review-core"></a>

### Core and shared value types

| ID | Status | Remaining finding | Current evidence | Related work |
|---|---|---|---|---|
| <a id="cr-c01"></a>CR-C01 | Present | Aggregation APIs still use the broader `ResultIndex` instead of `LazySortedSetStore`. Both narrowing TODOs remain. Their compiler-prerequisite wording is not a fresh assessment of the current toolchain. | `core/src/evaluation/functions/aggregation/lazy_sorted_set.rs:39`; `core/src/evaluation/functions/mod.rs:85` | [CW-69](#cw-69): API curation |
| <a id="cr-c02"></a>CR-C02 | Uncertain | Broader middleware dependency injection is not demonstrably implemented. The factory moved to `core-types` and its TODO disappeared, but `create()` still receives only configuration. `process()` receives an element index, and factories can capture dependencies; neither proves the broader evaluator/dependency proposal complete. | `core-types/src/interface/source_middleware.rs:23-38` | [CW-72](#cw-72): explicit services and capabilities |
| <a id="cr-c03"></a>CR-C03 | Present | Change evaluation still constructs empty query variables. The query-parameter TODO moved from `ContinuousQuery` into the shared evaluator rather than being implemented. | `core/src/query/evaluator.rs:282` | - |
| <a id="cr-c04"></a>CR-C04 | Present | Duration ordering still treats each month as 30 days, with the questioning comments in all four comparisons. This is an approximation, not calendar-aware ordering. | `core/src/evaluation/expressions/mod.rs:764`, `896`, `1052`, `1184` | - |
| <a id="cr-c05"></a>CR-C05 | Uncertain | `collect` reversal still removes the first equal occurrence. The provisional comment remains; whether a different occurrence identity or ordering rule is required is not established. | `core/src/evaluation/functions/aggregation/collect.rs:109-116` | [CW-12](#cw-12): related aggregation investigations, not the same reported defect |
| <a id="cr-c06"></a>CR-C06 | Uncertain | Temporal averaging still covers durations, not broader date/time types. The comment questions whether averaging dates/times makes sense; no requirement for those types is established. | `core/src/evaluation/functions/aggregation/avg.rs:111-119` | [CW-12](#cw-12): related aggregation area |
| <a id="cr-c07"></a>CR-C07 | Uncertain | The question whether `drasi.stdevp` should be an aggregate remains. It is still a scalar function over a list. | `core/src/evaluation/functions/drasi/stdevp.rs:26-28` | - |
| <a id="cr-c08"></a>CR-C08 | Uncertain | The custom map representation question moved to `core-types`. Objects still use `BTreeMap<String, VariableValue>`; the comment proposes no concrete replacement or demonstrated defect. | `core-types/src/evaluation/variable_value/mod.rs:43` | [CW-69](#cw-69): public collection-type exposure |
| <a id="cr-c09"></a>CR-C09 | Uncertain | The positive/negative integer representation question also moved to `core-types`. `PosInt(u64)` and `NegInt(i64)` remain; the questioning comment does not establish that the representation is wrong. | `core-types/src/evaluation/variable_value/integer.rs:26-29` | [CW-69](#cw-69): deliberate public value contracts |
| <a id="cr-c10"></a>CR-C10 | Present | Element values still cannot preserve IANA timezone names. The shared representation omits them, and conversion back to evaluation values supplies `None`. | `core-types/src/models/element_value.rs:47`, `208` | [CW-18](#cw-18): value-format consequences if this representation changes |

<a id="code-review-lib"></a>

### Lib contracts and telemetry

| ID | Status | Remaining finding | Current evidence | Related work |
|---|---|---|---|---|
| <a id="cr-l01"></a>CR-L01 | Present | Ordinary reaction bootstrap-context validity remains contractual rather than lifetime-enforced. Documentation prohibits retaining it past `bootstrap()` because FFI pointers can become invalid and still mentions future lifetime parameters. This is not a claim about native bootstrap handles. | `lib/src/reactions/bootstrap_context.rs:91-105` | [CW-63](#cw-63): ordinary bootstrap boundary |
| <a id="cr-l02"></a>CR-L02 | Partial | The new logging runtime counts full/closed transport losses, reports them and exposes `losses()`. The remaining uncertainty is their registered/exported metrics integration: no binding was found in that logging path. Generic `drasi.queue.drops` does not prove this binding. Lost logs are no longer wholly uncounted. | `lib/src/observability/log_runtime.rs:150-167`, `202-216`, `236-241`, `376-381`; `lib/src/observability/queues.rs:85` | [CW-23](#cw-23): loss visibility; [CW-26](#cw-26): metric boundaries |
| <a id="cr-l03"></a>CR-L03 | Partial | State-error mapping now preserves the typed cause, but supplied component type and ID remain reserved and unused. Retain the possible diagnostic-context enhancement, not a claim that the cause is discarded. Whether the cause already carries sufficient identity depends on the caller. | `lib/src/component_ops.rs:144-164` | [CW-22](#cw-22): actual component context |

<a id="code-review-plugins"></a>

### Plugins and shared supporting crates

These entries concern ordinary plugins unless a shared/native scope is stated.
A native counterpart does not demonstrate that an ordinary path was fixed.

| ID | Status | Remaining finding | Current evidence | Related work |
|---|---|---|---|---|
| <a id="cr-p01"></a>CR-P01 | Present | Ordinary gRPC `request_bootstrap` still returns an empty bootstrap response. This is separate from attaching a bootstrap provider to a source or native query. | `components/sources/grpc/src/lib.rs:817` | [CW-63](#cw-63): ordinary source retirement |
| <a id="cr-p02"></a>CR-P02 | Partial | Shared PostgreSQL origin/type/logical-message handling remains incomplete as an emitted/represented capability. Origin and type now receive strict structural validation but still return `Ok(None)`; logical messages remain skipped. Retain the omissions, not an absence of strict validation. | `components/postgres-common/src/decoder.rs:197-205`, `281-289`, `430-433` | [CW-112](#cw-112): related PostgreSQL work; shared code is not ordinary-only |
| <a id="cr-p03"></a>CR-P03 | Present | The shared PostgreSQL binary-numeric converter still calls its algorithm simplified, reconstructs a decimal from base-10000 digits and maps numeric NaN to `Decimal::ZERO`. It is not a complete PostgreSQL numeric representation. | `components/postgres-common/src/decoder.rs:925-983` | [CW-112](#cw-112): related PostgreSQL work; shared decoder |
| <a id="cr-p04"></a>CR-P04 | Present | Shared PostgreSQL SASL preparation still only escapes `=` and `,`. Moving the helper into `postgres-common` did not implement full string preparation. | `components/postgres-common/src/scram.rs:168-171` | [CW-112](#cw-112): related PostgreSQL connection work |
| <a id="cr-p05"></a>CR-P05 | Uncertain | COPY format-code parsing remains deferred. `CopyBothResponse` ignores its body because replication uses a known format. This is a conditional extension concern, not proof that supported replication needs variable formats. | `components/postgres-common/src/protocol.rs:587-601` | [CW-112](#cw-112): related PostgreSQL scope |
| <a id="cr-p06"></a>CR-P06 | Present | Kubernetes CRD support remains future work. The shared supported-kind whitelist is limited to the listed core/apps resources. | `components/kubernetes-common/src/config.rs:141-149` | [CW-44](#cw-44): Kubernetes native port scope, not a promise to add CRDs |
| <a id="cr-p07"></a>CR-P07 | Uncertain | Kubernetes bootstrap-specific concurrency/timeout settings remain a future extension of an empty config. Existing settings come from source configuration; no operational failure is demonstrated. | `components/bootstrappers/kubernetes/src/config.rs:17-26` | [CW-44](#cw-44): initial-data provider scope |
| <a id="cr-p08"></a>CR-P08 | Present | MSSQL Windows authentication still returns an unimplemented error. The adjacent Azure AD branch is also unimplemented. This finding concerns the reviewed `mssql-common` connection path, not proof about native SQL Server authentication. | `components/mssql-common/src/connection.rs:56-63`, `141` | - |
| <a id="cr-p09"></a>CR-P09 | Uncertain | Application source/reaction properties still use flexible flattened maps with the same "for now" comments. A stricter schema has not appeared, but no requirement for one is established. | `components/sources/application/src/config.rs:25-27`; `components/reactions/application/src/config.rs:23-25` | - |
| <a id="cr-p10"></a>CR-P10 | Present | Garnet's serialization-efficiency TODO remains: directly serialize Redis values instead of going through `StoredElement`. | `components/indexes/garnet/src/storage_models/mod.rs:22-27` | [CW-32](#cw-32): measured serialization cost |
| <a id="cr-p11"></a>CR-P11 | Present | Ordinary MCP result removal still linearly searches a vector. The suggestion to use stable keys for constant-time removal remains. | `components/reactions/mcp/src/mcp.rs:823` | [LR-10](#lr-10): carry the requirement into the native MCP design |
| <a id="cr-p12"></a>CR-P12 | Present | Ordinary MCP CORS origins are still not configurable here. The TODO remains immediately before `.allow_origin(Any)`. | `components/reactions/mcp/src/mcp.rs:921-922` | [LR-10](#lr-10): native MCP exposure policy |
| <a id="cr-p13"></a>CR-P13 | Present | Event Grid standalone packaging remains deferred under `TODO(#574)`: workspace dependencies and loader/SDK alignment are not the published standalone model. | `components/reactions/eventgrid/Cargo.toml:33-36`, `49-56` | [LR-05](#lr-05): native replacement; [CW-63](#cw-63): ordinary retirement; [DX-1](#dx-1): native packaging counterpart |
| <a id="cr-p14"></a>CR-P14 | Present | Ordinary HTTP standalone packaging retains workspace/published dependency and SDK-alignment TODOs. Native HTTP packaging does not change this manifest. | `components/reactions/http/Cargo.toml:33-36`, `48-58` | [CW-63](#cw-63): ordinary retirement; [DX-1](#dx-1): native packaging counterpart |
| <a id="cr-p15"></a>CR-P15 | Present | Ordinary gRPC standalone packaging retains `FIXME(#575)` and workspace dependencies. Published SDK/core dependencies and standalone test dependencies remain deferred. | `components/reactions/grpc/Cargo.toml:33-38`, `70-73` | [CW-63](#cw-63): ordinary retirement; [DX-1](#dx-1): native packaging counterpart |
| <a id="cr-p16"></a>CR-P16 | Present | Ordinary platform profiling still reuses source timestamps as dispatcher/router placeholders and fallback values, not distinct measured phases. This is the platform compatibility mapping, not a claim that native telemetry lacks real spans. | `components/reactions/platform/src/transformer.rs:30-37`, `46-79` | [LR-11](#lr-11): platform owner decision; [CW-26](#cw-26): truthful timing boundaries |
| <a id="cr-p17"></a>CR-P17 | Present | redb state-store table names still use process-lifetime `Box::leak` allocations. Keys are now typed partitions, but retention remains; clearing the lookup cache does not reclaim strings. | `components/state_stores/redb/src/provider.rs:187-198`, `337-339`, `363-375` | - |
| <a id="cr-p18"></a>CR-P18 | Partial | redb WAL storage now validates format/owner and rejects incompatible or unversioned storage without wiping it. It does not migrate incompatible formats; the old blanket "wiped on upgrade" claim is inaccurate. Document the supported/refused boundary, not automatic migration or a claim that every upgrade changes the format. | `components/wals/redb/src/provider.rs:41-98`, `101-152`, `179-187` | [CW-18](#cw-18): explicit format policy |
| <a id="cr-p19"></a>CR-P19 | Present | Ordinary FFI state-store `get_many`/`set_many` still loop over individual calls for simplicity. This does not describe the separate native state API. | `components/plugin-sdk/src/ffi/state_store_proxy.rs:141`, `156` | [CW-28](#cw-28): per-item storage cost; [CW-63](#cw-63): ordinary ABI retirement |

<a id="code-review-tradeoffs"></a>

### Retained tradeoffs and dependency workarounds

These are not automatically proposed changes. Several deliberately preserve
safety or compensate for an external API; decide whether replacement is needed
before treating them as implementation work.

| ID | Status | Remaining compromise | Current evidence | Related work |
|---|---|---|---|---|
| <a id="cr-k01"></a>CR-K01 | Present | Ordinary plugin libraries remain process-pinned. The loader retains an extra library reference to prevent execution of unloaded code. Native libraries also being pinned does not resolve this tradeoff. | `components/host-sdk/src/loader.rs:125-135` | [CW-63](#cw-63): ordinary loader boundary |
| <a id="cr-k02"></a>CR-K02 | Present | Ordinary plugin shutdown clears its runtime pointer without reclaiming the intentionally retained runtime. This is not a fully joined/destroyed runtime shutdown. | `components/plugin-sdk/src/lib.rs:560-575` | [CW-63](#cw-63): ordinary SDK boundary |
| <a id="cr-k03"></a>CR-K03 | Present | Ordinary Source/Reaction log/status callback contexts retain process-pinned raw `Arc` references. The retained-call owner preserves this choice; ordinary disposal does not reclaim the transferred raw reference. | `components/host-sdk/src/proxies/source.rs:728-737`; `components/host-sdk/src/proxies/reaction.rs:416-423`; `components/host-sdk/src/proxies/retained_call.rs:54-59` | [CW-63](#cw-63): ordinary callback boundary |
| <a id="cr-k04"></a>CR-K04 | Partial | Normal reaction stop now joins the result forwarder and releases its context; the former unconditional success-path retention is resolved. Remaining retention is abandonment without confirmed disposal: the actual plugin and unjoined worker are pinned rather than freed unsafely. This is not successful cleanup; retryable awaited cleanup is intended. | `components/host-sdk/src/proxies/reaction.rs:87-110`; `components/host-sdk/src/proxies/retained_call.rs:199-215` | [CW-17](#cw-17): ownership; [CW-08](#cw-08): retention versus leaks, with ordinary/native scope kept distinct |
| <a id="cr-k05"></a>CR-K05 | Present | The serialized plugin-drop worker still lets a slow or hung foreign `drop_fn` block unrelated drops. Its macOS TLS-destructor safety rationale remains, and the retained-call owner still invokes it. | `components/host-sdk/src/proxies/drop_worker.rs:17-34`; `components/host-sdk/src/proxies/retained_call.rs:54-56` | [CW-17](#cw-17): owned teardown, not permission to weaken the safety boundary |
| <a id="cr-k06"></a>CR-K06 | Partial | Host logging now consults configured Drasi logging and supports explicit policy injection. Only the custom-host fallback remains ambiguous: without that policy, `log::max_level() == Off` cannot distinguish unset logging from silence, so forwarding can default to Trace. Configured Drasi `Off` is not generally broken. | `components/host-sdk/src/callbacks.rs:105-128`, `171-177` | [CW-24](#cw-24): distinguish the ordinary-host fallback from native logging |
| <a id="cr-k07"></a>CR-K07 | Present | HTTP XML-to-JSON conversion moved into `http-common` but retains the simplified object/attribute/`_text`/array projection. Parsing fixes do not make it a full XML data model. | `components/http-common/src/content.rs:165-173` | - |
| <a id="cr-k08"></a>CR-K08 | Present | Adaptive batching estimates backlog from throughput instead of measuring queued messages. The shared HTTP and ordinary HTTP/gRPC reaction helpers retain the heuristic. | `components/http-common/src/adaptive_batcher.rs:300-312`; `components/reactions/http/src/adaptive_batcher.rs:310-320`; `components/reactions/grpc/src/adaptive_batcher.rs:304-314` | [CW-26](#cw-26): measurement boundaries; not evidence that native queue metrics are absent |
| <a id="cr-k09"></a>CR-K09 | Present | Automatic numeric timestamp units still use magnitude thresholds. Explicit format selection is the alternative; this is not a claim that all timestamps are decoded incorrectly. | `components/sources/mapping/src/engine.rs:654-668` | - |
| <a id="cr-k10"></a>CR-K10 | Present | Garnet's next sorted-set lookup loads every member to merge buffered mutations. It still relies on aggregation-group sets being typically small rather than performing a targeted next-member query. | `components/indexes/garnet/src/result_index.rs:167-174` | [CW-32](#cw-32): measured rescanning cost |
| <a id="cr-k11"></a>CR-K11 | Uncertain | Persistence inference still depends on provider/environment assumptions. The config helper assumes persistence without a provider; Garnet declares nonvolatility assuming server persistence. No active native reliance on the config-only fallback, or lack of server persistence, was established. | `lib/src/indexes/config.rs:99-106`; `components/indexes/garnet/src/plugin.rs:209-211` | [CW-77](#cw-77): native Garnet qualification/refusal is separate evidence |
| <a id="cr-k12"></a>CR-K12 | Present | RocksDB per-column-family buffer sizes remain fixed after construction even when the manager's overall budget changes. | `components/indexes/rocksdb/src/options.rs:27-35` | [CW-29](#cw-29): distinguish backend buffers from queue budgets |
| <a id="cr-k13"></a>CR-K13 | Present | Ordinary Neo4j timestamp startup still begins at the earliest CDC cursor and filters rather than seeking to the timestamp. This remains a cost imposed by the described CDC API limit. | `components/sources/neo4j/src/config.rs:23-26`; `components/sources/neo4j/src/cdc.rs:95-98` | - |
| <a id="cr-k14"></a>CR-K14 | Present | HERE traffic start-position configuration supports only `Now`. The comment does not promise historical replay. | `components/sources/here-traffic/src/config.rs:113-120` | [CW-117](#cw-117): declare actual replay support |
| <a id="cr-k15"></a>CR-K15 | Present | Ordinary MySQL binlog position ordering assumes one lineage, not ordering across failover lineages. | `components/sources/mysql/src/types.rs:180-187` | [MY-15](#my-15): native failover counterpart, not an ordinary ordering fix |
| <a id="cr-k16"></a>CR-K16 | Present | Dashboard matching falls back to `id`/equality when row signatures are unknown. Canonical signatures already work; only the unknown-identity fallback remains. | `components/reactions/dashboard/src/websocket.rs:447-465`; `components/reactions/dashboard/static/js/widgets.js:345-349` | [LR-08](#lr-08): dashboard replacement/port decision |
| <a id="cr-k17"></a>CR-K17 | Present | Ordinary SourceBase WAL replay reads events into memory before live delivery. Streaming remains a possible peak-memory improvement; current replay is bounded by WAL capacity. This does not describe every native replay implementation. | `lib/src/sources/base.rs:1249-1265`, `1288-1314`; `lib/src/channels/dispatcher.rs:423-441` | [CW-29](#cw-29): distinguish replay memory and byte limits |
| <a id="cr-k18"></a>CR-K18 | Present | Ordinary SourceBase broadcast mode still lacks per-subscriber replay filtering. Channel mode has filtering; do not generalize the limitation to all subscriptions or native pipes. | `lib/src/sources/base.rs:250`, `1044` | [CW-69](#cw-69): distinguish similarly named broadcast contracts |
| <a id="cr-k19"></a>CR-K19 | Present | SDK schema UI hints require JSON post-processing because the schema dependency lacks the property-level extension support. The adaptation works; the dependency workaround remains. | `components/plugin-sdk/src/schema_ui.rs:15-20` | - |
| <a id="cr-k20"></a>CR-K20 | Present | The proxy macro requires an explicit vtable binding to work around `macro_rules!` hygiene. This is a completed adaptation, not an unimplemented proxy feature. | `components/ffi-primitives/src/macros.rs:80-87` | - |
| <a id="cr-k21"></a>CR-K21 | Present | Date parsing appends a day because the dependency cannot directly parse `YYYYMM`. Date support is implemented; only the dependency workaround remains relevant. | `core/src/evaluation/functions/temporal_instant/temporal_instant.rs:2025-2034` | - |

<a id="code-review-tests"></a>

### Test gaps and misleading comments

Test changes belong with [Testing and qualification](#testing-and-qualification).
Documentation mismatches below are not missing implementations.

| ID | Status | Remaining finding | Current evidence | Related work |
|---|---|---|---|---|
| <a id="cr-t01"></a>CR-T01 | Present | The three identified join tests still do not assert emitted joined rows. Their status-only compromise remains after working-tree edits. This is not a claim that other tests lack join coverage. | `lib/src/queries/joins_test.rs:260-262`, `281`, `392-393`, `476`, `618`, `635` | [CW-77](#cw-77): related native semantic coverage; [CW-13](#cw-13): result-based reproductions |
| <a id="cr-t02"></a>CR-T02 | Present | Ordinary stored-procedure MySQL/PostgreSQL helpers retry container creation around Docker Desktop port-mapping problems. This is conditional test infrastructure, not production behavior or evidence that the current Podman setup has this problem. | `components/reactions/storedproc-mysql/tests/mysql_helpers.rs:86-88`; `components/reactions/storedproc-postgres/tests/postgres_helpers.rs:74-76` | [CW-37](#cw-37): infrastructure reliability |
| <a id="cr-d01"></a>CR-D01 | Present | SourceBase says explicit position-handle cleanup awaits query-manager wiring, but the current subscription adapter calls `remove_position_handle()` when clearing a requested position. The reference-count fallback remains useful; the historical explanation is misleading. | Comment: `lib/src/sources/base.rs:649-652`; current cleanup: `lib/src/computation/v1/plugin_source.rs:952-965` | - |
| <a id="cr-d02"></a>CR-D02 | Present | Ordinary HTTP checkpoint documentation says errors are not propagated, but the function returns `Result` and explicitly returns `Err(e)` after logging. This is a documentation mismatch, not missing propagation. | `components/reactions/http/src/standard_loop.rs:223-243` | [CW-22](#cw-22): accurate error semantics |

<a id="code-review-paths"></a>

### Adjacent unimplemented code paths

These were separate from comment findings in the original report and remain so.

| ID | Status | Remaining finding | Current evidence | Related work |
|---|---|---|---|---|
| <a id="cr-a01"></a>CR-A01 | Uncertain | Unary `Exists` evaluation still contains `todo!()`. Parser/runtime reachability was not established; this is a potential unimplemented path, not a demonstrated reachable crash. | `core/src/evaluation/expressions/mod.rs:147` | [CW-22](#cw-22): classify public-path panic reachability |
| <a id="cr-a02"></a>CR-A02 | Partial | PostgreSQL TRUNCATE still does not produce graph row removals/reset. The ordinary source warns without applying it. Shared/native replication now permanently rejects selected-table TRUNCATE and requires erase/re-add/bootstrap; unselected tables can be ignored. Explicit rejection is safer but does not implement selected-table recovery. | Ordinary: `components/sources/postgres/src/stream.rs:485-487`; shared/native: `components/postgres-common/src/replication/transaction.rs:240-264` | [Source D-9 follow-up](#pg-truncate-reset): exact reset gap; [CW-92](#cw-92): downstream reset coordination; [CW-112](#cw-112): ordinary source scope |

Close an investigation with code/test evidence or an explicit requirements
decision. For a deliberate limitation or dependency workaround, record the
supported contract and replacement decision rather than inventing a missing
feature. Keep ordinary/native applicability and partial-resolution evidence
when an item is moved into an implementation or qualification entry.
