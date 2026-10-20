# ComponentGraph and ComputationGraph

**Current snapshot: 20 October 2026.** Comparison of the
[ComponentGraph-based implementation][old-core] with
[current Core][current-core] and [Server][current-server].

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

<a id="new-built-in-capabilities"></a>

## Building and connecting applications

| Area | ComponentGraph-based implementation | ComputationGraph |
|---|---|---|
| Processing beyond queries | Processing paths were specialized around sources, queries, reactions and query-local middleware. | Connect sources, arbitrary processing steps, queries, destinations and background services. A path need not contain a query. |
| Sharing one producer | Sources could serve multiple queries, and query results could feed multiple reactions. | Any supported producer can feed several downstream steps. One transformer can feed other transformers, queries and sinks without duplicating the producer. |
| Reusable preprocessing | Middleware generally ran within each query's input path. | Run normalization or enrichment once before several queries. Stateful middleware can save its input progress and unfinished output so restart does not repeat committed processing. |
| Checked connections | Data and queue behavior followed the specialized component interfaces. | Named inputs and outputs declare their data formats. Incompatible connections and invalid records are rejected; branches can add their own context without changing shared input. |
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
| Saving processing and outgoing queues together | Supported query transactions covered query state, input progress and saved query output. | Where the actual storage supports it, selected outgoing queues can join the producer's transaction, closing the gap between committing processing and saving its handoff. |
| Tracking destination work | Completion and recovery behavior were reaction-specific. | A reusable delivery service records which operations really finished and retries the remainder. Local business state and completion can share a transaction; remote effects still require cooperation from the destination. |
| Several processing steps in one transaction | Atomic query evaluation existed. | Selected stateful processing steps can run in one ordered transaction, committing their state, input progress and final output together. |
| Whole database transactions | Database changes were supplied as individual source events. | Supported sources deliver a complete committed transaction as one query input. The query can publish the final result instead of an intermediate half-transfer. |
| Initial data from several providers | Initial loading was coordinated through source-specific subscription and bootstrap paths. | The query owns preparation, loading and handover. Multiple real providers can be composed with separate initialization state and checked completion, including PostgreSQL plus HTTP. |
| Checking recovery promises | Guarantees depended on the selected source/query/reaction implementations and settings. | The graph checks actual processing owners, storage, connections and completion behavior before accepting a requested recovery guarantee. A flag or matching resource name is not enough. |

## Managing, hosting and using applications

| Area | ComponentGraph-based implementation | ComputationGraph |
|---|---|---|
| Ownership and cleanup | Each manager coordinated its component type and associated tasks. | Components, connections and declared resources follow a common ownership model. Cleanup remains owned after a caller stops waiting, and unfinished work stays visible. |
| Missing dependencies | Startup and dependency handling were spread across the specialized managers. | Valid declarations can wait for missing sources, resources, factories or connections and activate when they arrive, while respecting an explicit stop. |
| Live changes | Managers supplied category-specific update and removal behavior. | Updates check that the expected application version is still current, coordinate affected resources and protect pending work. Unrelated instances stay unchanged. This is not rollback of external effects. |
| Failure handling | Reporting and recovery behavior depended on the manager/component path. | Supported returned errors and catchable failures leave the failed component inspectable, preserve causes and prevent reuse of a damaged instance. Healthy unrelated branches can continue. |
| Saved application definitions | Server and component configuration described the specialized application model. | Rebuildable definitions include arbitrary native components, connections and resource recipes. Accepted changes and retry records can be saved before construction; a failed component does not erase the accepted definition. |
| Configuration administration | There was no equivalent native service for saving and administering complete application definitions. | The configuration store supports encrypted definitions, controlled cleanup of request records, deletion of saved configurations only when their version matches, and changing encryption keys. These operations do not delete or rewind processing data. |
| Plugin model | Plugins implemented the older source, reaction, bootstrap and provider interfaces. | Independently built native plugins can supply sources, processing steps, sinks, services and initialization providers. The host drives their operations; the plugin does not create another graph executor. |
| Smaller plugin dependencies | Component plugins used the older library/SDK integration. | Native component libraries use shared data contracts without pulling in the query engine or parsers. Load the particular libraries an application needs rather than a combined bundle. |
| Separate-process hosting | Applications could arrange their own processes and connections. | The host can own child and grandchild DrasiLib processes, track their failures and connect them through explicit data/control links, including authenticated encrypted links and selected saved-delivery modes. |
| Inspection and query access | ComponentGraph supplied shared metadata and asynchronously received manager/component status updates. | The running graph publishes coherent status and dependency information. Native queries use the same result-reading surface; Server streams begin with current rows, then subsequent changes, including after reconnect. |
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

<a id="performance-and-qualification-boundaries"></a>

## Current limits

- **Native replacement is incomplete.** Remaining connector families,
  MySQL/SQLite source packaging, native database schema discovery and some
  standard Server deployments still need work. Older interfaces currently run
  through graph-owned adapters.
- **Query processing is still built in**, not an independently loadable query
  plugin. Some reusable processing capabilities still require Rust-level setup.
- **Recovery guarantees remain specific.** Shared-source restart with undrained
  persistent input queues is not qualified. HTTP snapshots/live prices and SSE
  notifications do not gain durable replay merely by being connected to the graph.
- **Transactions remain local to their actual owners.** Several queries,
  databases or remote effects do not become one atomic operation. The time merger
  also cannot prove that no unseen earlier event will arrive. Processing layouts
  cannot contain feedback loops.
- **Lifecycle and scale work remains.** The complete startup/shutdown deadline
  policy is unfinished. Large-graph reporting and repeated removal/addition costs
  still need measurement and improvement; no universal speedup is claimed.
- **Process isolation has limits.** The selected child-host behavior is qualified
  on macOS/Linux, not Windows or as a security sandbox. Arbitrary non-yielding
  native code cannot always be stopped safely inside a process.

The [remaining-work list](computation-graph-remaining-work.md) includes unresolved
query investigations and the other open items; this comparison does not claim
that every defect in the old system has been fixed.

[old-core]: https://github.com/drasi-project/drasi-core/tree/261b8d40d35ce203a7f4aaae0028956d76a8def4
[current-core]: https://github.com/drasi-project/drasi-core/tree/e900b70d932e350d4d1ec7acb36ae2ba3cbf913c
[current-server]: https://github.com/drasi-project/drasi-server/tree/15dafa225ac76ba9868452a7082f693027621ad7
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
