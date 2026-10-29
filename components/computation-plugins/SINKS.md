# Native sink input: standard graph changes from any producer

Every native sink accepts the **default graph-change schema**
(`GraphChangeCodec::schema()`) on its `in` port. A source, a transformer and a
continuous query publish the same records, so the same sink configuration,
decoder and processing path serve all three. Connecting a PostgreSQL source
directly to the HTTP sink needs no query, no query annotation and no conversion
stage; connecting a query to the same sink needs nothing more.

## What a sink may rely on

| Information | Where it comes from |
|---|---|
| Element identity | The record: output-scoped namespace plus an opaque key (`ElementReference`) |
| Node or relation, labels, endpoints | The element image |
| Property values | Typed `DataValue`s; JSON only at a protocol boundary |
| Add / replace / patch / delete | The operation and its record image: a full after-image replaces, an explicit patch changes only the stated properties and lists removed names, a delete may have a full, partial or identity-only before image |
| Event time | `SystemMetadata.timestamp`, optional. Absent means unknown: it is never replaced by the element's business time (`effectiveFrom`, a separate optional fact), a sequence or a clock reading |
| Sequence | The producing stream's own `SystemMetadata.sequence` |
| Producing output | The sink's **configured output binding**: a real component and output port (below) |

A query result row is an ordinary `Result` (or configured-label) node whose
properties are the `RETURN` columns and whose identity is the query evaluator's
opaque scoped row identity. Query annotations (`drasi.query-output.v1` and
friends) are optional context. No sink needs them to decode, acknowledge or
deliver default input, and **none invents a query identity for a source**.

`GraphChangeBatch::from_envelope` (in `drasi-computation-contracts`) is the one
producer-neutral decoder. It fails clearly — never drops or defaults — for:

* a schema other than the graph-change schema;
* an event-local scheduling control (`SystemMetadata.control`), which is not data;
* a query snapshot or progress-only recovery frame, which are recovery
  controls that no ordinary native sink implements;
* a record image that is not valid for its operation.

An empty change set with no control is valid and delivers nothing.

## Output bindings

A sink accepts the **output ports** of every component the graph connects to
its input. The graph supplies each incoming edge to the sink at construction:
the producing component, its actual output port and the stream that carries that
port's data (`InputBinding`). Nothing is written by hand, and a sink with no
declared incoming edge fails to construct.

The optional `outputs` setting only **names** incoming outputs. It maps an
**alias** to `{component, port?}`. The alias names the output in routes, URLs
and templates; an output without an alias is named after its producing
component. A component with several incoming outputs needs an alias for each,
and an alias that selects no incoming edge fails.

```json
"outputs": {
  "rooms":        {"component": "rooms",        "port": "out"},
  "rooms-errors": {"component": "rooms",        "port": "errors"},
  "facilities":   {"component": "facilities-db", "port": "out"}
}
```

* The alias is a route/display name, not an identity. Protocol bodies, headers,
  snapshot lookup and gRPC batches always carry the real `component` and `port`
  as well, and two outputs of one component stay distinct.
* The port comes from the graph's declared edge. A sink never infers it from a
  stream name and never chooses a component's first port; an alias without a
  `port` selects the only incoming output of its component.
* A component may be a source, a transformer or a query. An input whose stream
  is not a declared incoming edge fails. When an incoming edge or stream changes
  the graph reconstructs the sink with the new bindings; it never keeps serving
  stale ones.
* Template `routes` are keyed by alias (a full alias takes precedence over its
  last dotted segment).

| Sink | Incoming outputs |
|---|---|
| HTTP, gRPC, SSE | declared edges; optional `outputs` aliases |
| PostgreSQL, MySQL, SQL Server stored procedures | declared edges; optional `outputs` aliases |

## Truthful change projection

Protocols still differ, and the shared decoder does not force one body, but no
protocol is shaped like a query result. Every native sink states, per change:

| Field | Meaning |
|---|---|
| `operation` | `ADD`, `REPLACE`, `PATCH` or `DELETE`, exactly as the record states it |
| `identity` | `{namespace, id}`: the canonical element identity. For a query row `id` is the lowercase hex of the evaluator's opaque row identity; nothing is hashed or defaulted |
| `element` | `{labels, kind?, inNode?, outNode?}` only when the record carries it. An identity-only delete has none |
| `before` | Only when the record carries a before image. An unknown before is **absent**, never `null` |
| `after` | The complete element for `ADD`/`REPLACE`; **only the properties a `PATCH` sets** (an explicit `null` stays `null`) |
| `removedProperties` | Names a `PATCH` removes; present only on a patch |
| `effectiveFrom` | The element's business time, only when known |
| `output` | `{alias, component, port}` (HTTP/SSE/gRPC/SQL contexts) |
| `sequence` | The producing stream's sequence |
| `timestamp` | `SystemMetadata.timestamp` (RFC 3339), only when the envelope carries it |

A valid identity-only deletion with no before image and no envelope timestamp is
delivered as `{operation: "DELETE", identity, output, sequence}` and nothing
else. A template that reads an absent field fails strictly instead of rendering
an empty value.

Template routes use the operation's group: `added` (ADD), `updated` (REPLACE
and PATCH; the context's `operation` tells them apart) and `deleted` (DELETE).
The template context is the same object as the default body.

Where a protocol needs a publication or observation time (SSE frames), it uses
an explicitly named field (`publishedAt`, the sink's own wall clock in
milliseconds), never a sequence or an invented event time.

Queries keep their useful tabular display and `before`/`after` diffs through
their own result projection: a result node's properties are the row, so the same
`after` and `before` objects are the row values. A query's `Replace` states its
before image when the evaluator has one and otherwise states none.

## Materialization owner

Native sinks are stateless. They transmit changes truthfully and never keep a
second copy of the table. Whoever already owns row state applies the changes:
the `@drasi/react` client applies `ADD`/`REPLACE`/`PATCH`/`DELETE` to its
identity-keyed rows. A patch with no base row is an explicit retryable
`MISSING_BASE_ROW` error that triggers a fresh snapshot; a delete of an absent
element is a no-op.

## Per-sink behavior

| Sink | Input handling | Delivery |
|---|---|---|
| `drasi.standard/capture` | Writes the complete envelope frame; needs no decoding | `Handled` after write and flush |
| `drasi.network/http-sink` | One request per ordered change (or one `{"batch":[…]}` request per output) | `Handled` after every request completes or is deliberately skipped |
| `drasi.network/grpc-sink` | One `ChangeService.ProcessChanges` item per ordered change, batched per output and rendered metadata | `Handled` after an exact acknowledgement |
| `drasi.network/sse-sink` | One browser frame per envelope (or one event per routed change); empty envelopes publish nothing | `Accepted` (volatile broadcast) |
| `drasi.postgres/storedproc`, `drasi.mysql/storedproc`, `drasi.mssql/storedproc` | Each change selects the output alias's `added`/`updated`/`deleted` template | `Handled` after the commands complete; PostgreSQL may also use transactional delivery |
| `drasi.bridge/sink` | Forwards graph records unchanged, including patches and removals | Per its transport contract |

The legacy query reactions (`ReactionService`, query-only SSE/HTTP reactions)
keep their query-result wire. Native sinks do not use it.

PostgreSQL and MySQL live qualification against the new shapes is pending final
qualification. SQL Server is compile- and unit-tested only; its live
qualification is explicitly deferred.

## Testing a producer against a sink

Use `GraphChangeCodec::encode_changes` for a source-shaped envelope. It has no
query annotation, and `QueryChangeCodec::is_query_output` on it is `false`. The
host-sdk `native_network_loopback` test delivers such envelopes through the real
HTTP and gRPC sink libraries, and `native_network_wire` projects the same
records from a source and from a query, including a second output port of one
component and an untimed identity-only deletion.
