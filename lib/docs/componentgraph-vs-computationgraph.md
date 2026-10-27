# ComponentGraph and ComputationGraph

**Scope:** Comparison of the
[ComponentGraph-based implementation][old-core] with the current Core and
matching Server checkouts, including native database components, shared hosting,
the unified graph-change data contract, OTel progress measurements and the verified standard-gRPC crash-recovery path.
This is a development capability comparison, not a published-release
or full-workspace qualification claim.

## The main difference

ComponentGraph recorded components, relationships and status. Separate source,
query and reaction managers controlled the work around it. The application model
was primarily **sources -> queries -> reactions**.

ComputationGraph owns the running components, their connections, resources and
starting/stopping behavior. Queries are steps within that graph, alongside other
kinds of processing. Applications can share work, build paths without a query,
and apply common rules for delivery, recovery and replacement.

Continuous queries, Cypher/GQL, persistent query state, initial snapshots,
saved-output replay, supported query transactions, shared sources and independently
running queries already existed. The improvements below extend or strengthen
those capabilities; they are not a new query engine.
Here, **native components** use the new graph's direct interfaces rather than
the older source/reaction interfaces.
Both ordinary and native queries run through `ContinuousQueryTransformer` and
the same evaluator. Converting every plugin to the native interfaces, deleting
the ordinary adapters and making queries dynamically loadable are separate work;
they are not prerequisites for using this single runtime.

<a id="new-built-in-capabilities"></a>

## Building and connecting applications

| Area | ComponentGraph-based implementation | ComputationGraph |
|---|---|---|
| Processing beyond queries | Processing paths were specialized around sources, queries, reactions and query-local middleware. | Connect sources, arbitrary processing steps, queries, destinations and background services. A path need not contain a query. |
| Sharing one producer | Sources could serve multiple queries, and query results could feed multiple reactions. | Any supported producer can feed several downstream steps. One transformer can feed other transformers, queries and sinks without duplicating the producer. |
| Reusable preprocessing | Middleware generally ran within each query's input path. | Run normalization or enrichment once before several queries. Stateful middleware can save its input progress and unfinished output so restart does not repeat committed processing. The `map` and `jq` stages are stateless event mappers; updates carry explicit property removals and full replacements are preserved end to end. |
| Checked connections | Data and queue behavior followed the specialized component interfaces. | Named inputs and outputs declare their data formats. Incompatible connections and invalid records are rejected; branches can add their own context without changing shared input. |
| One data format for sources, processing and queries | Sources produced source changes, queries produced a separate JSON result format for reactions, and middleware ran inside each query's own input path. | Sources, middleware and queries all publish the same graph-change records: typed nodes and relationships (full images, patches and before-images) that keep rich values such as unsigned integers, durations, ranges and references without reducing them to JSON. A sink, middleware stage or tool written for graph changes works on any of them. |
| Queries feeding queries | A query's results were not source data; chaining meant writing a converting component. | A query's result rows are ordinary nodes (label `Result` by default, with one property per `RETURN` alias), so one query's output connects directly to another query's input with no adapter. Row identities are scoped to each output, so two outputs cannot collide, and equal-valued rows stay separate rows. A correction with an older business time is still applied when it comes from a validated producer. |
| Scheduled work and ordering | Scheduled reevaluation travelled as source-change variants, and stale-change checks always used the change's own time. | Each event states how it is ordered (source event time by default, producer sequence for derived query output), and scheduled "due" and "reevaluate" signals are a separate event-local control rather than fake data patches. Business times are preserved unchanged. |
| Choice of delivery behavior | Queue, broadcast and acknowledgement behavior lived in the individual paths. | Choose how each connection buffers data, waits for a slow receiver, handles overflow and confirms completion. Supported queues also limit bytes and read saved history in bounded portions. |
| Time-based merging | Queues ordered the events already available. | A dedicated step can wait and reorder within explicit time and size limits, with a chosen policy for late input and optional restart recovery. |
| Independent execution | Queries already had independently running workers. | Independent native sources, processing steps, queries, sinks and services can run separately. One slow step need not prevent unrelated work from progressing. |
| Direct component control | Managers and lifecycle events supplied specialized coordination. | Related components can exchange control messages separately from data queues. An old connection cannot control its replacement. |

## Saving work and recovering it

| Area | ComponentGraph-based implementation | ComputationGraph |
|---|---|---|
| Independent consumer progress | Recovery was organized around source, query and reaction subscriptions. | A shared producer can observe the actual progress of every required consumer. Different initial snapshots and processing speeds remain independent; a fast consumer cannot release history still needed by a slow one. |
| Completion without a query | Processing progress was mainly tied to the specialized query/reaction paths. | Ordinary transformers and handled sinks can have graph-owned in-memory input progress, published only after handling and required output work. This does not claim persistence or atomic external effects. |
| One saved message, many readers | Sources and queries had their own logs, output history and subscriber handling. | A shared saved queue keeps one copy with independent reader positions. Disconnecting a required reader does not erase its obligation; removal is explicit. |
| Retrying an input request | Sources could retain input, but there was no common native service for identifying client requests and recording their acceptance. | An optional service saves input and its acceptance record together before confirming receipt. Retrying the same request can recover the outcome without appending it twice. |
| Replaying output into another queue | Query output could be saved and resent after failure. | Supported receiving queues recognize already accepted logical output. A retry need not append a second copy merely because the sender lost its acknowledgement. |
| Restoring a consumer from a snapshot | Reactions were told through query-specific recovery paths. | Each sink input can optionally offer its own snapshot handler. An input without one rejects a snapshot rather than silently accepting it, and a plugin's declared snapshot inputs must exactly match the handlers it really provides. |
| Saving processing and outgoing queues together | Supported query transactions covered query state, input progress and saved query output. | Where the actual storage supports it, selected outgoing queues can join the producer's transaction, closing the gap between committing processing and saving its handoff. |
| Tracking destination work | Completion and recovery behavior were reaction-specific. | A reusable delivery service records which operations really finished and retries the remainder. Local business state and completion can share a transaction; remote effects still require cooperation from the destination. |
| Several processing steps in one transaction | Atomic query evaluation existed. | Selected stateful processing steps can run in one ordered transaction, committing their state, input progress and final output together. |
| Whole database transactions | Database changes were supplied as individual source events. | Supported sources deliver a complete committed transaction as one query input. The query can publish the final result instead of an intermediate half-transfer. |
| Initial data from several providers | Initial loading was coordinated through source-specific subscription and bootstrap paths. | The query owns preparation, loading and handover. Multiple real providers can be composed with separate initialization state and checked completion, including PostgreSQL plus HTTP. |
| SQLite snapshot/live ownership | The ordinary bootstrapper opened a separate read-only file connection; direct Rust source handles and REST supplied the write path. | Independently loaded paired bootstrappers can use the actual source-owned snapshot/journal engine and query owners. Standalone file loading remains available; no public DrasiLib SQL read/write API is added. |
| Checking recovery promises | Guarantees depended on the selected source/query/reaction implementations and settings. | The graph checks actual processing owners, storage, connections and completion behavior before accepting a requested recovery guarantee. A flag or matching resource name is not enough. |
| Retained input during staggered restart | WAL cleanup used the confirmed positions of subscribers already attached. | Ordinary HTTP/gRPC cleanup waits for the initial subscription group before releasing retained history. Replay registration is protected against pruning; missing history fails explicitly rather than looking like an empty replay. The actual two-query standard-gRPC crash case is verified below. |

## Managing, hosting and using applications

| Area | ComponentGraph-based implementation | ComputationGraph |
|---|---|---|
| Ownership and cleanup | Each manager coordinated its component type and associated tasks. | Components, connections and declared resources follow a common ownership model. Producers drain before their actual progress owners retire; accepted stop operations survive cancelled waiters. Failed cleanup remains owned without blocking unrelated cleanup unnecessarily. |
| Missing dependencies | Startup and dependency handling were spread across the specialized managers. | Empty graphs and valid unconnected declarations can wait for missing sources, resources, factories or connections, while respecting an explicit stop. Invalid connected outputs without stream bindings are rejected. A waiting instance host remains manageable after library startup returns, without falsely reporting readiness. |
| Live changes | Managers supplied category-specific update and removal behavior. | Updates check that the expected application version is still current, coordinate affected resources and protect pending work. Unrelated instances stay unchanged. This is not rollback of external effects. |
| Failure handling | Reporting and recovery behavior depended on the manager/component path. | Supported returned errors and catchable failures leave the failed component inspectable, preserve causes and prevent reuse of a damaged instance. Healthy unrelated branches can continue. |
| Saved application definitions | Server and component configuration described the specialized application model. | Rebuildable definitions include arbitrary native components, connections and resource recipes. Accepted changes and retry records can be saved before construction; a failed component does not erase the accepted definition. |
| Configuration administration | There was no equivalent native service for saving and administering complete application definitions. | The configuration store supports encrypted definitions, controlled cleanup of request records, deletion of saved configurations only when their version matches, and changing encryption keys. These operations do not delete or rewind processing data. |
| Plugin model | Plugins implemented the older source, reaction, bootstrap and provider interfaces. | Independently built native plugins can supply sources, processing steps, sinks, services and initialization providers. The host drives their operations; the plugin does not create another graph executor. |
| Smaller plugin dependencies | Component plugins used the older library/SDK integration. | Native component libraries use shared data contracts without pulling in the query engine or parsers. Load the particular libraries an application needs rather than a combined bundle. |
| Database component availability | MySQL, SQL Server and SQLite used the ordinary source/bootstrap/reaction interfaces; their supported roles differed. | Independent MySQL source/bootstrap/stored-procedure libraries and SQLite source/bootstrap libraries have real static/dynamic and Server qualification. SQL Server libraries exist but remain incomplete and unqualified against a real database. |
| Source data discovery | Sources could report best-effort data schemas through the ordinary source interface. | Native sources can expose bounded, asynchronous data-schema readers while data processing is pending. Reads are fenced across stop/replacement and use the same DrasiLib/Server inspection surface, not factory configuration schemas. |
| Credential refresh | Supported ordinary reactions resolved their identity provider on each start. | Native factories can opt into an actual Host identity resource resolved on every activation, preserving provider precedence without caching stale credentials or changing saved configuration. This preserves a legacy capability rather than inventing one. |
| Separate-process hosting | Applications could arrange their own processes and connections. | The host can own child and grandchild DrasiLib processes, track their failures and connect them through explicit data/control links, including authenticated encrypted links and selected saved-delivery modes. The link carries one graph-change format for source and query data; an export allowlist names the exact output namespaces permitted to cross. |
| Inspection and query access | ComponentGraph supplied shared metadata and asynchronously received manager/component status updates. | The running graph publishes coherent status and dependency information. Native queries use the same result-reading surface; Server streams begin with current rows, then subsequent changes, including after reconnect. |
| Processing-progress measurements | Query-output and reaction-checkpoint metrics existed in specialized paths. | Read actual committed query input and produced-output positions, with lifecycle and output identity. Server optionally exports them through an application-owned OTel provider. An input producing no result still advances input progress; missing measurements are not zero progress. This is not yet all-component OTel coverage or destination-completion proof. |
| Server configuration | Configuration and routes included older single-instance and component-family forms. | Server uses explicit instances and supports native component/resource definitions, batch creation and metadata-derived configuration. Secret settings can remain references rather than copied values. |

The runtime graph is the application's processing layout, not the graph of
business data evaluated by Cypher/GQL. Persisted configuration remains the
accepted desired state; neither inspection nor plugin hosting introduces another
execution engine or an independently authoritative component registry.

## Bugs in the old implementation that are resolved

These include fixes in query processing and plugin hosting, not just changes to
the graph data structure.

| Old problem | Current behavior |
|---|---|
| **A settings-read failure could erase valid saved query state.** The old recovery code could treat failure to read the configuration hash as a reason to clear state. | The read failure is reported. It does not authorize deleting saved data and starting fresh. |
| **Timestamp sorting could skip an event that had never been processed.** A later source sequence could be handled first and advance the checkpoint past an earlier event. | Available source heads are merged without moving a later admitted event ahead of an earlier event from that producer. |
| **Repeated inserts could count the same row twice.** Aggregates could receive a second contribution for an existing identity. | An insert replaces the existing row and adjusts its previous contribution. An identical insert makes no additional contribution. |
| **Older changes could overwrite newer state, and old inserts could resurrect deleted rows.** | Older mutations are rejected while input progress advances. Supported persistent query storage retains deletion history across restart. |
| **A node and a relationship could collide on one live identity.** | The conflicting kind is rejected before the existing object is changed. |
| **Cancelling a query stop could lose ownership of its worker. A timeout could report stopped without proving cleanup finished.** | Work and join handles remain owned. Graceful stop waits for its required completion; forced interruption is explicit and is not reported as successful graceful cleanup. |
| **A failed instance stop could prevent a useful retry, while final shutdown hid stop failures.** | Unfinished ownership and member failures remain available for retry and reporting instead of being converted into apparent success. |
| **An unreadable plugin source event could be silently skipped.** Later progress could then move past the lost change. | The decoding failure is delivered as an error and ends that change stream; later events are not accepted as if the missing event had succeeded. |

Code references: [old settings recovery][old-hash-error] and [current recovery][query-code];
[old ordering][old-ordering] and [current input queue][ranked-input];
[old mutation handling][old-mutations] and [current mutation cases][mutation-tests];
[old query stop][old-query-stop], [old instance shutdown][old-instance-stop] and
[current task ownership][execution];
[old event decoding][old-decoder] and [current event decoding][current-decoder].

## What the completed recovery check proves

The [versioned Building Comfort evidence][completed-recovery] uses the actual
Server, separately rebuilt ordinary gRPC source/reaction libraries running in
ComputationGraph, and the external test-service. It is not an embedded-only test
or qualification of the native gRPC factories.

| Check | Recorded result |
|---|---|
| Uninterrupted reference | Both queries committed all 100,000 inputs; receivers recorded and finalized the complete output |
| Pending-input SIGKILL/restart | The aggregate query restored input 88,748 and replayed the remaining 11,252 events without reapplying definitions |
| Exact delivery comparison | All 99,981 room and 49,860 aggregate outputs matched, with no missing, extra, duplicate, conflicting or reordered records |
| Independent final-state check | 12 room rows and 3 floor rows matched the captured source data; each floor appeared once |

Completion uses fresh OTel measurements from the current Server process, complete
recorded producer batches and awaited logger finalization, not quiet output or
record counts alone. The local run used Rust 1.95.0 debug builds on macOS and a
100,000-entry outbox per query; it is not a throughput or universal exactly-once
guarantee. The [manual strict workflow][completed-workflow] requires fresh
matching-build reference and recovery runs. Its recipe is not evidence that the
Linux CI run has passed. The older scheduled multi-transport workflow remains a
separate snapshot-only check, and the incomplete v1 reference is preserved.

<a id="performance-and-qualification-boundaries"></a>

## Current limits

- **Native replacement is incomplete.** Other selected families and old
  consumers remain. SQL Server has deferred real-database qualification and
  outstanding permission, large sink-batch and orphan-marker gaps. Older
  interfaces still run through graph-owned adapters.
- **Storage is per component by design.** There is no mandatory state store or
  materializer; each component chooses its own storage and durability, if any.
- **Read capabilities are optional, not implied by the data format.** The output
  catalogue discovers snapshot, replay and bounded snapshot/live providers for
  sources, queries and transformers. Queries use their existing result state;
  other producers supply their own implementations. A snapshot alone is not a
  gap-free subscription, and a memory-backed live lease is not crash-durable
  replay. Complete source transactions use the same graph records with explicit
  completeness metadata, not another data schema.
- **Custom formats must be implemented by both peers.** Matching custom-schema
  ports connect directly; only formats that really differ need an intentional
  conversion. A factory that really implements several formats on a port declares
  them, and configuration selects one per port (`ports: {<port>: {format: ...}}`).
  Selection is validated before construction; an unknown port or format is
  rejected, never defaulted or converted. The built-in standard factories declare
  no alternatives and expose graph-change only. Shorthand endpoints resolve only a
  direction's single port or a factory-designated primary port, never a guess.
- **Event mappers are stateless; a view transform is future work.** The `map` and
  `jq` stages apply their rules to the data in each event and ignore events that
  make no sense for a rule (for example, a removal update cannot be mapped without
  prior context, so that rule is skipped). A separate, stateful view-transform
  component that maintains a derived dataset is tracked as
  [CW-68](computation-graph-remaining-work.md#cw-68); it does not exist yet and
  is not a mode of those stages.
- **Builds must match.** The native wire is version 1 with ABI layout 1.0 and
  there is no compatibility with earlier wire or query-row formats. Rebuild the
  host and every native library together.
- **Database modes have explicit limits.** MySQL 5.7 positional factories retain
  their own metadata/history profile; they do not claim the stronger factories'
  coordinated snapshots, commit clocks or whole-transaction query visibility.
  Standalone SQLite file initialization requires quiescent writers. Parameterized
  procedure calls do not become exactly-once external effects.
- **Query processing is still built in**, not an independently loadable query
  plugin. Some reusable processing capabilities still require Rust-level setup.
- **Variable-length relationship execution is unfinished.** Some patterns parse,
  but their hop bounds are discarded by the current single-edge planner. Parsed
  syntax must not be mistaken for working multi-hop execution.
- **Recovery guarantees remain specific.** Standard gRPC with the ordinary
  adapters and a source WAL has the completed pending-input restart evidence
  above. Cold restart with undrained native persistent input-pipe frames and
  other transport/failure combinations still need qualification. HTTP
  snapshots/live prices and SSE notifications do not gain durable replay merely
  by being connected to the graph.
- **Transactions remain local to their actual owners.** Several queries,
  databases or remote effects do not become one atomic operation. The time merger
  also cannot prove that no unseen earlier event will arrive. Processing layouts
  cannot contain feedback loops.
- **Lifecycle and scale work remains.** The complete startup/shutdown deadline
  policy is unfinished. Large-graph reporting and repeated removal/addition costs
  still need measurement and improvement; no universal speedup is claimed.
- **Monitoring is only partly implemented.** Optional OTLP/HTTP JSON export
  currently covers query input/output progress, not every component's throughput,
  latency, errors, queue pressure or completed destination work. Metrics do not
  replace stored recovery state.
- **Local qualification is not final acceptance.** Focused and library tests
  check individual boundaries. Final acceptance of the unified data contract needs
  all supported tests plus the GPU Lab, Move-a-Wall and Building Comfort
  demonstrations on matching rebuilt binaries. The Building Comfort recovery
  evidence above was recorded before this contract and does not stand in for that
  rerun. Broader CI checks and recorded test-infra lint failures remain separate.
- **Process isolation has limits.** The selected child-host behavior is qualified
  on macOS/Linux, not Windows or as a security sandbox. Arbitrary non-yielding
  native code cannot always be stopped safely inside a process.

The [remaining-work list](computation-graph-remaining-work.md) includes unresolved
query investigations and the other open items; this comparison does not claim
that every defect in the old system has been fixed.

[old-core]: https://github.com/drasi-project/drasi-core/tree/261b8d40d35ce203a7f4aaae0028956d76a8def4
[old-hash-error]: https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/queries/manager.rs#L1969-L2017
[query-code]: ../src/computation/v1/query.rs
[old-ordering]: https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/queries/priority_queue.rs#L47-L53
[ranked-input]: ../src/computation/v1/ranked_pipe.rs
[old-mutations]: https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/core/src/query/continuous_query.rs#L298-L390
[mutation-tests]: ../../core/src/query/tests/mutation_integrity.rs
[old-query-stop]: https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/queries/base.rs#L215-L244
[old-instance-stop]: https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/lib_core.rs#L537-L630
[execution]: ../src/computation/v1/graph/execution.rs
[old-decoder]: https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/components/host-sdk/src/proxies/change_receiver.rs
[current-decoder]: ../../components/host-sdk/src/proxies/change_receiver.rs
[completed-recovery]: https://github.com/drasi-project/test-infra/tree/13c24c57f5c0f4bd4fb56ae493d0bd9359e9cd57/e2e-test-framework/examples/recovery_comparison/goldens/building-comfort-small-v2
[completed-workflow]: https://github.com/drasi-project/test-infra/blob/13c24c57f5c0f4bd4fb56ae493d0bd9359e9cd57/.github/workflows/e2e-building-comfort-completion.yml
