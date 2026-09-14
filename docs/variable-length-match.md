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

## Why there are two solvers

A query without repetition uses `MatchPathSolver`. A query with a bounded
repetition uses `VariableLengthSolver`. `QueryBuilder` picks one at build time.
A running query never switches.

The fixed solver fills named slots. For `(a)-[r]->(b)-[s]->(c)` the plan is
five slots: `a`, `r`, `b`, `s`, `c`. A change is tested against each slot. If it
fits slot `s`, it is pinned there. Neighbors are filled one hop at a time from
that pin. Joins, `OPTIONAL MATCH`, and source middleware all depend on that
slot graph.

A bounded pattern is not extra slots. `(a)-[r:ROAD*1..3]->(b)` does not know
how many hops a match has, which hop the change is, or which edges belong in
`r`. That binding is a list. The change is a starting point, not a pin.

Teaching `MatchPathSolver` hop ranges would send every one-hop query through a
walker it does not need. Unrolling `*1..3` into three fixed paths would make
`r` three separate pins instead of one list. It would also fake trail
uniqueness. A second solver for the hop range leaves the existing solver
unchanged for every other query.

## Slots in the element index

The element index does not store paths. It stores each element, the slots that
element currently fits, and adjacency lists for relationships.

Both solvers ask the same neighbor APIs:
`get_slot_elements_by_inbound(slot, node)` and
`get_slot_elements_by_outbound(slot, node)`. The slot integer is what differs.

On the five-slot path, relationship `r` is stored under slot `1`. A lookup of
slot `3` from `b` returns only edges that belong in `s`. Different hops are
different lists.

On `(a)-[r:ROAD*1..3]->(b)` the stretch is one slot, `0`. Every matching `ROAD`
edge is stored under slot `0`, inbound at its start node and outbound at its
end node. `*1..3` and `*1..10` use that same slot. Nodes get an empty affinity
list. The solver finds them by walking edges.

The ordered list bound to `r` is not stored. The solver rebuilds it for the
change. Projected rows go to the result index like any other query.

A hop count does not belong on a stored slot. Portland to Eugene is hop 1 of
one path and hop 2 of another. Stamping one count on that edge would lie.
Putting the edge in hop 1, 2, and 3 would return every `ROAD` again. The walker
already counts hops as the length of the trail it is building in memory.

When a relationship's endpoints change, inbound and outbound keys must move
even if the slot number stays `0`. Garnet and RocksDB rebuild those keys from
the previous nodes and the new nodes.

## How a change is matched

The examples use this graph and query:

```text
Seattle --e1--> Portland --e2--> Eugene --e3--> Medford

MATCH (a:City)-[r:ROAD*1..3]->(b:City)
RETURN a.id, b.id, [e IN r | e.id]
```

The bounded solver starts at the changed element and walks both ways under the
hop budget. It never reuses a relationship on one path. Nodes may repeat. It
prepares the old snapshot and the new snapshot before any index write. The new
snapshot is the stored graph plus this one change laid on top.

### Change inside a variable-length stretch

`e2` (Portland to Eugene) is inserted or updated. That edge can sit at more
than one hop index.

The fixed solver, on `(a)-[r]->(mid)-[s]->(b)`, would pin `e2` in slot `s` and
fill the nodes that touch it.

The bounded solver cannot pin `e2` to a hop. It walks left from Portland and
right from Eugene, up to the remaining budget, and keeps every trail whose
length is in `1..3`:

- Portland to Eugene (`e2`)
- Seattle to Eugene (`e1`, `e2`)
- Portland to Medford (`e2`, `e3`)
- Seattle to Medford (`e1`, `e2`, `e3`)

### Change outside a variable-length stretch

Seattle's name changes. Seattle is an endpoint, not a `ROAD` hop. The bounded
solver still starts at Seattle and walks 1, 2, and 3 `ROAD` hops.

On `(a)-[:ROAD*1..3]->(b)-[:IN]->(s)`, a change to the `:IN` edge or to `s` is
also outside the repeating stretch. The solver binds that side first, then
walks `ROAD` from `b`.

A node change always starts a walk. That includes an internal city with no
endpoint label, and a node whose labels do not match. Relationship changes
skip a stretch whose type does not match.

## Tradeoffs and performance

Index size does not grow with the hop cap. Plugin `set_element(element, slots)`
is unchanged. One-hop queries keep precise per-hop adjacency.

The neighbor lists are coarser. Slot `0` is every `ROAD` on the stretch, not
"the second hop." From Portland the index returns every matching `ROAD`. The
walker then drops illegal trails.

Node changes have no slot filter. The fixed solver skips a node that fits no
slot. The bounded solver walks from every node insert or update. An unrelated
`(:Other)` insert still charges `max_work`. A tiny work budget can reject that
insert even when the node cannot appear in a match.

Walk cost grows with branching and the upper bound. A dense `*1..3` can hit
`max_work`, `max_states`, or `max_matches` on one change. The engine then
rejects the change with `QueryExecutionError::MatchResourceLimit`. It does not
emit a partial result.

Each change solves two snapshots. Future wakes still identify a match by
`group_signature`. They do not share a timer across two paths that only share
a node.

Storing partial paths in the index would avoid some rewalks. Storage would
then grow with paths, not edges. One insert would create and delete many
prefixes. Trail uniqueness needs the edges already used, not a hop count.
That path index is not present.

## Bootstrap

Bootstrap is a sequence of `SourceChange::Insert` calls. Each insert is one
prepare, the same as a live change. There is no separate "load the whole
graph, then match once" pass.

Because node inserts always walk, a large snapshot costs more on a bounded
query than on a one-hop query. Loading a hub node can exhaust `max_work`
before later edges arrive. Raise the limits on that query, or load in an
order that does not present a high-degree node against a dense stretch in one
insert.

Insert order still converges. An edge whose endpoints are not in the index
yet produces no match. Inserting the missing node later walks from that node
and can recover the path. Deleting an internal node drops paths through it.
Reinserting that node can restore them without reinserting the edges.

Exact zero-hop patterns create a match when a node that satisfies both
endpoints is inserted, even if no relationship exists yet.

Existing persisted indexes for a query containing repetition must be rebuilt
when upgrading from an engine that treated that repetition as one hop. The old
candidate slot layout is not compatible with the bounded matcher. A snapshot
rebuild still drops timer history.

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
