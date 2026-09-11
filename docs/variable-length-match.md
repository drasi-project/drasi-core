# Variable-length MATCH

Drasi Core supports bounded variable-length relationship patterns in continuous
queries. The same patterns work with the memory, RocksDB, and Garnet indexes.

```cypher
MATCH (start:Station)-[legs:CONNECTS*1..3]->(end:Station)
RETURN start.name AS origin,
       end.name AS destination,
       size(legs) AS hops,
       [leg IN legs | leg.distance] AS distances
```

## Bounds

| Pattern | Number of relationships |
| --- | --- |
| `[:R*2]` | Exactly two |
| `[:R*1..3]` | One through three |
| `[:R*..3]` | One through three |
| `[:R*0..3]` | Zero through three |
| `[:R*0]` | Zero |

The upper bound is inclusive. Negative bounds, reversed ranges, and unbounded
patterns such as `[:R*]` are rejected when the query is built.

A zero-hop match binds both endpoints to the same existing node. That node must
satisfy both endpoint patterns. No relationship of the requested type is needed.

## Bindings and path identity

A relationship variable with a repetition binds to a list, including an
exact one-hop pattern. A zero-hop match binds an empty list. A relationship
variable without a repetition still binds to one relationship.

The list follows the written order of the pattern. In `(a)<-[rs:R*2]-(b)`,
the first entry is the relationship next to `a`, regardless of its stored
direction. Directed and undirected patterns are supported.

Intermediate nodes must exist, but they do not inherit the endpoint labels or
property predicates. Repeated node variables must satisfy all their occurrences.
The property map in `[rs:R*2 {enabled: true}]` applies to every relationship.
List-valued filters can contain literal objects, as in
`[rs:R*2 {config: [{enabled: true}]}]`. Nested variable references and calls remain
unsupported in these inline filters.

Nodes can repeat in a path. A relationship cannot repeat within one `MATCH`,
including across fixed segments, repeated segments, and comma-separated
patterns. Separate `MATCH` clauses can reuse a relationship.

Distinct paths remain distinct results even when their projected values are
equal. Parallel relationships count separately. Segment boundaries also affect
identity. On a three-edge chain, `[:R*1..2]->()-[:R*1..2]` has separate
one-plus-two and two-plus-one matches.

Property-only changes retain the row signature of an existing path. Adding or
removing a path changes aggregation counts according to its multiplicity.

## Continuous updates

Node and relationship changes can create, update, or remove matches. Changes to
an internal node are relevant even if that node matches no endpoint label.
Deleting an internal node removes paths through it. Reinserting the node can
restore those paths without reinserting their relationships.

The matcher also handles changes to relationship endpoints, types, and properties.
Existing projection, aggregation, and future-reprocessing functions receive
the resulting match changes.

## DrasiLib source selection

When a repetition can cross an intermediate node, DrasiLib requests all available
node labels rather than only the endpoint labels. Exact zero-hop and at-most-one-hop
segments do not require this expansion for intermediate nodes.

Labels assigned to another configured source subscription remain excluded.
Unassigned labels can come from any subscribed source. An unrestricted selection
admits an unlabeled node or a node with at least one eligible label.
Physical relationship labels retain their source allocations.

Bootstrap requests can fetch a superset. DrasiLib applies each subscription's exact
selection after source middleware, using the source event's envelope rather than
the element reference's namespace. The same selection applies to bootstrap, replay,
and live events. An excluded insert or update becomes a deletion of that reference,
so relabeling cannot leave an earlier admitted version indexed. Label-less deletions
pass through. Excluded events still advance their source's sequence and position
checkpoint.

An empty bootstrap label list requests all available labels of that element kind.
It does not mean no elements. `LabelSelection::Labels` with an empty set represents
that case inside DrasiLib. Supported fixed-length queries with source middleware
request raw bootstrap inputs broadly because middleware can change labels and
element kinds. Bounded patterns still reject configured source middleware.

Core source-change methods accept `SourceInput` for a per-query normalizer.
Existing calls that pass `SourceChange` retain their call syntax through its
`From<SourceChange>` conversion. Normalizers are pure, synchronous, run after middleware,
and do not apply to scheduled future processing.

## Index layout

The matcher reuses one relationship candidate slot for a segment at every depth.
The number of candidate slots does not grow with the upper bound. Ordered
relationship lists belong to completed matches, not to the index's scalar
element slots.

Nodes are fetched directly by reference. They do not need candidate-slot
memberships. Exact zero-hop segments do not index relationship candidates.

The engine discovers affected paths from the changed element. Ordinary matching
does not require a global table of every path or a transitive-closure index.

Each query keeps only its selected matcher. Fixed-only queries continue to use
the existing fixed-length solver. The `variable_length` modules implement repeated
relationship traversal within the same engine, indexes, and query evaluator.

Existing persisted indexes for a query containing repetition must be rebuilt
when upgrading from an engine that treated that repetition as one hop. The old
candidate slot layout is not compatible with the bounded matcher.

## Resource limits and failures

`QueryBuilder::with_variable_length_match_limits` accepts
`drasi_core::query::VariableLengthMatchLimits`.

| Field | Default | Limit |
| --- | --- | --- |
| `max_hops` | `64` | Largest upper repetition bound accepted at build time |
| `max_work` | `1_000_000` | Relationship candidates and traversal or completion states visited |
| `max_states` | `100_000` | Partial trail and binding states generated |
| `max_matches` | `10_000` | Buffered matches, counting old and new snapshots separately |
| `max_duration` | `Duration::from_secs(5)` | Preparation time, checked between index operations |

The work limits cover both snapshots of one source event. They do not cap the
size of source properties or allocations inside an index provider. A finite hop
bound can still produce many paths on a dense graph.

Ordinary source changes prepare both snapshots before writing to indexes.
`QueryExecutionError::MatchResourceLimit` rejects the change without returning a
partial result. A preparation failure leaves the query available for a retry.
Execution errors use the existing `EvaluationError::IndexError` variant and are
available through `EvaluationError::execution_error()`.

After mutation starts, recovery depends on the outer transaction's outcome.
This includes a future that has been popped but whose evaluation or hook fails.

| Root outcome | Meaning |
| --- | --- |
| `Active` | The transaction accepts explicit nested calls |
| `Committing` | Commit is in progress. A cancelled await does not imply rollback |
| `Committed` | A matching `CommitReceipt` releases provisional results |
| `RolledBack` | All participating writes were restored. The query permits retry |
| `RequiresRebuild` | Writes could not be restored. The query rejects further processing |
| `Indeterminate` | The durable outcome is unknown. The query rejects further processing |

`RollbackSupport::Complete` covers the graph, accumulators, future queue, and
other stores that participate in the transaction. The default memory controller
does not undo writes. A dirty abort therefore requires reconstruction.
`ContinuousQuery::check_health()` reports `QueryExecutionError::QueryRequiresRebuild`
for an unrecoverable or indeterminate outcome.

## Transaction API

Ordinary source and future processing methods own their transactions. An unrelated
call cannot join an active transaction implicitly. It receives
`SessionError::SessionBusy` through `IndexError::Other`.
`IndexError::session_error()` exposes the typed session error.

Explicit nested methods accept a mutable `SessionGuard` from the same controller
as the query. Each call returns a `Provisional<T>` without waiting for the outer
commit.

```rust
let mut outer = SessionGuard::begin(session_control.clone()).await?;
let first = query.process_source_change_in(&mut outer, first_change).await?;
let second = query.process_source_change_in(&mut outer, second_change).await?;
let receipt = outer.commit_with_receipt().await?;
let first_results = first.into_committed(&receipt)?;
let second_results = second.into_committed(&receipt)?;
```

`process_source_change_in_with_hook` and `process_due_futures_in_with_hook`
expose tentative results for writes that must precede the outer commit.
These results are not ready for publication. The corresponding root-owning
methods are `process_source_change_with_result_hook` and
`process_due_futures_with_hook`.

`SessionGuard::rollback()` consumes the guard and returns the resulting
`RootOutcome`. Dropping an active guard also aborts it. Neither operation makes
nontransactional writes reversible. On Tokio, an owned finalizer establishes the
outcome even if the caller stops awaiting the commit. Other executors poll the
commit inline. Cancelling an inline commit leaves the root indeterminate.
A retained `RootHandle` exposes that outcome and can recover the receipt through
`commit_receipt()` after a confirmed commit.

The existing `SessionControl::begin`, `commit`, and `rollback` methods remain
available with their original signatures. `SessionGuard::commit()` still returns
`Result<(), IndexError>`. Nested calls use the additive `commit_with_receipt()`
method. `NoOpSessionControl` remains a unit struct.

## Cache lifetime

`CachedElementIndex`, `CachedResultIndex`, and `ShadowedFutureQueue` can share the
query's `Arc<dyn SessionControl>` through `new_with_session`.
Mutable cache entries belong to a transaction generation. Nested calls can reuse
them within a root, but an outcome change invalidates them. Delayed fills and
streams cannot populate a newer generation with older data.

This policy sacrifices cache reuse between source events to make rollback safe.
It avoids retaining tentative aggregate counts or adjacency data after an abort.
Cross-transaction promotion of mutable entries is not implemented.
The original `new` constructors retain their signatures but act as read-through
wrappers without retaining mutable cache entries. Without a shared controller,
they cannot detect rollback safely.

## Unsupported combinations

Bounded variable-length matching rejects the following combinations at build time:

- `OPTIONAL MATCH`, disconnected patterns, or a new `MATCH` in a later query part.
- Virtual joins or source middleware.
- Repeated relationship aliases, or an alias shared by a node and a relationship.
- Inline property maps with nonliteral values, including references to another
  variable or function calls.
- `SET` or `DELETE` clauses.
- A custom parser without valid `QueryParser::parse_scoped` metadata.

Ordinary `WHERE` clauses, projections, and aggregations remain supported.
Unbounded traversal, named path values, and shortest-path syntax are not supported.
