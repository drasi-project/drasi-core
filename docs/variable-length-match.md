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
