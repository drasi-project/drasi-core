# ComputationGraph internal data design

This document explains what happens to data **inside DrasiLib**: its Rust
representations, the schemas carried between components, how processing changes
those representations, and what is stored and replayed. It is for Rust developers
who already use Drasi and want to understand the implementation.

The boundary starts when a source supplies Drasi data and ends at the internal
input of a sink or reaction. External database schemas, network request formats,
destination protocols, plugin authoring, component lifecycle management and
application configuration are outside its scope. Recovery is covered because it
determines which data is read and processed after an interruption.

## Contents

1. [Source and sink](#1-source-and-sink)
2. [Ports and schemas](#2-ports-and-schemas)
3. [Records, change sets and envelopes](#3-records-change-sets-and-envelopes)
4. [Pipes and delivery](#4-pipes-and-delivery)
5. [Identity, sequence, time and context](#5-identity-sequence-time-and-context)
6. [Transformers and schema changes](#6-transformers-and-schema-changes)
7. [Fan-out and fan-in](#7-fan-out-and-fan-in)
8. [Queries](#8-queries)
9. [Initial data and the live-data boundary](#9-initial-data-and-the-live-data-boundary)
10. [Result snapshots, retained output and readers](#10-result-snapshots-retained-output-and-readers)
11. [Persistence and transaction boundaries](#11-persistence-and-transaction-boundaries)
12. [Recovery and duplicate handling](#12-recovery-and-duplicate-handling)
13. [An input followed through processing and replay](#13-an-input-followed-through-processing-and-replay)
14. [Reading progress correctly](#14-reading-progress-correctly)
15. [Implementation map](#15-implementation-map)

## 1. Source and sink

Start with two components:

```mermaid
flowchart LR
    source["Source"] -->|"Data"| sink["Sink"]
```

The **source produces data**. The **sink consumes it**. Everything in the first
five sections can be understood using this connection alone.

Three mechanisms define the connection:

| Mechanism | What it determines |
|---|---|
| Port | Where data leaves the source or enters the sink |
| Schema | What the data means and how its bytes are represented and checked |
| Pipe | How data travels between the ports, including buffering and delivery behavior |

Inside ComputationGraph, the unit being transported is a `ChangeEnvelope`.
It contains an ordered set of changes, their schema and identifying metadata.
The source's output and the sink's input must agree on that representation.

Neither a port nor a pipe changes the data's meaning. The source supplies valid
data; the pipe transports it; the sink handles it. Buffering and persistent
storage can be added to this same connection without introducing another
processing component.

External input and output are outside this diagram. The source and sink shown
here are DrasiLib components, and the arrow describes their internal data flow.

## 2. Ports and schemas

### 2.1 Ports name the endpoints

Give the source an output port called `readings` and the sink an input port
called `in`:

```mermaid
flowchart LR
    source["Source<br/>output port: readings"] -->|"Pipe carries data with schema S"| sink["Sink<br/>input port: in"]
```

The boxes still represent the same two components. Port names identify the
endpoints inside those components; the arrow remains the pipe carrying data.

A `PortDescriptor` contains:

| Field | Meaning |
|---|---|
| `id: PortId` | Name within the component |
| `direction: PortDirection` | Input or output |
| `schema: SchemaDescriptor` | The one data representation accepted or produced here |
| `requirements: PipeRequirements` | Required delivery properties, such as backpressure |

Port names must be unique within a component. Connected names need not be equal:
`Source.readings` can connect to `Sink.in`. An output must connect to an input,
their complete schema descriptors must match, and the pipe must meet the
requirements of both ports.

A port is not a queue, stream cursor or storage object. It describes a boundary.
When the graph invokes the sink, `InputEnvelope.port` identifies which of its
input ports received the data.

### 2.2 A schema describes the data, not its transport

The schema answers what a record's key, image and payload mean. It does not say
whether the data was delivered, saved or handled.

`SchemaDescriptor` contains:

- A `SchemaId` and positive `SchemaVersion`.
- An encoding name describing the payload representation.
- Nonempty definition bytes supplied in the schema's canonical form.
- A calculated, noncryptographic fingerprint.

The connection compares the **complete descriptor**, not just its name,
version or fingerprint. Two definitions that look equivalent to a person but
have different definition bytes are not automatically compatible.

For the simple source/sink connection, both ports use schema S. The source
cannot emit a differently encoded record and expect the pipe to convert it.
The sink must receive exactly the representation it agreed to consume.

### 2.3 Validation checks the records

`Schema` pairs the descriptor with an executable `RecordValidator`. The
descriptor is descriptive data; the validator supplies the rules that actually
check record identity, image kind, payload encoding and content.

For example, an application-defined reading schema could require a nonempty
sensor key and a numeric measurement. That rule must exist in its validator.
Definition bytes are not automatically interpreted as JSON Schema, and attaching
a familiar field name does not establish its type or meaning.

`Record::try_new` checks the key and payload through that validator.
`RecordReference::try_new` checks a key without requiring a payload, which is
useful for an identity-only deletion. `ChangeSet::try_new` then checks the
relationships between the operations and their images.

Records are immutable after construction. Normal in-process routing need not
decode their payloads again just to forward them, but the graph still checks
the envelope's schema against the port at emission and delivery. Decoding
serialized envelopes reconstructs and validates their records.

Different providers of the same schema descriptor must implement the same
validation meaning. Descriptor equality cannot prove that arbitrary validation
code is correct.

Sources: [port contracts][ports], [schemas and checked records][records].

## 3. Records, change sets and envelopes

The source and sink exchange the same checked data structure. The envelope,
schema and record types are defined in `drasi-computation-contracts` and
re-exported through `drasi_lib::computation::v1`.

### 3.1 The nested structure

The graph's message representation is:

```text
InputEnvelope or OutputEnvelope
  port: PortId
  envelope: ChangeEnvelope
    event: Arc<ChangeEvent>
      id: EnvelopeId
      changes: Arc<ChangeSet>
        id: ChangeSetId
        schema: SchemaDescriptor
        operations: Arc<[ChangeOperation]>
          ordinal
          record identity
          optional before record
          after record, where applicable
      system: Arc<SystemMetadata>
      lineage: Option<Arc<Lineage>>
    context: ProcessingContext
```

The port address is outside the shared event. A receiving component gets its
own local input-port name without changing the producer's event.

A `Record` is a validated combination of:

- `RecordReference`: schema descriptor plus `RecordId`.
- `RecordImage`: whether the bytes are a full image, patch or partial image.
- `payload: Bytes`: immutable schema-defined bytes.

The payload is not necessarily UTF-8, JSON or a Rust struct in memory. The
schema's codec determines how to decode it.

### 3.2 Before/after images and operations

`RecordImage::Partial` and `RecordImage::Patch` are different:

- **Full:** complete record in that schema.
- **Patch:** instructions/data for updating the record under that schema's rules.
- **Partial:** known values from a before-image; not a complete record and not
  an update instruction.

The generic change-set rules are:

| Operation | After image | Before image |
|---|---|---|
| `Added` | Required, `Full` | None |
| `Updated`, `Replace` | Required, `Full` | Optional `Full` or `Partial` |
| `Updated`, `Patch` | Required, `Patch` | Optional `Full` or `Partial` |
| `Deleted` | None; a validated `RecordReference` is required | Optional `Full` or `Partial` |

`ChangeSet::try_new` checks that all records use its exact schema, before/after
identities agree, and operation ordinals are unique and increasing. Ordinals need
not be consecutive. Construction does not sort an invalid batch into validity.
The codecs may assemble operations in a temporary collection and explicitly
order them before calling that constructor.

Repeated **record identities** are not forbidden across different operations:
one batch can describe several changes to the same record. An operation's stable
identity is the change-set ID together with its ordinal.

An empty change set is valid. It can carry metadata without record changes,
including a separately marked control message. Empty data alone does not
identify its intended meaning.

### 3.3 Record encoding versus envelope encoding

There are two outer envelope codecs:

| Codec | Outer representation | Format version |
|---|---|---:|
| `EnvelopeCodec` | Explicit JSON frame; opaque record/identity bytes remain encoded byte data | 1 |
| `BinaryEnvelopeCodec` | Named MessagePack frame with binary payloads | 2 |

The records inside either frame retain their schema-defined payload encoding.
For example, a binary-encoded record stays encoded bytes inside a JSON envelope;
its fields are not automatically converted into untyped JSON properties.

Both codecs reconstruct checked records/change sets using explicitly registered
executable schemas. Unknown schema versions, conflicting definitions and
unsupported outer versions fail. A serialized descriptor does not carry an
executable validator.

Codec limits apply to the encoded frame. `BinaryEnvelopeCodec::encoded_size`
measures the full binary envelope, including metadata, lineage and context,
without allocating the encoded output. This is the measure used by relevant
byte-budget mechanisms; it is not a measure of Rust heap use.

Source positions used by the shared progress contracts are bounded to 65,536 bytes.
Other bounds belong to their specific reader, journal or transaction rather
than one universal maximum for all data. A frame-size check does not itself
prove that every nested value or custom validator is safe at arbitrary depth.

Sources: [records][records], [envelopes][envelopes],
[JSON envelope codec][json-codec], [binary envelope codec][binary-codec].

## 4. Pipes and delivery

### 4.1 Following one envelope

At the graph's envelope boundary, the source returns an `OutputEnvelope`
containing its output-port name and a `ChangeEnvelope`. The graph checks the
emission and passes it to the bound pipe.
The pipe receiver supplies that envelope to the graph, which invokes the sink
with an `InputEnvelope` naming the sink's input port.

The port address changes from the source's local name to the sink's local name.
The shared event, records and schema do not change merely because the data
crossed the pipe.

`EnvelopeSender::send` returns an `EnqueueReceipt`: confirmation that the pipe
accepted the envelope, optionally with a journal position. It does not say that
the sink handled it.

`EnvelopeReceiver::receive` returns a `Delivery`. This contains the envelope and
an optional `Box<dyn Acknowledgement>`. The graph holds the acknowledgement
separately while passing the data to the sink.

### 4.2 Acceptance and handling are separate

Consider a source that successfully sends an envelope while the sink is busy
with an earlier one. Acceptance succeeded; handling has not happened yet.

The sink's completion contract makes the distinction explicit:

| `SinkCompletion` | What a successful sink call establishes |
|---|---|
| `Accepted` | The sink accepted or queued the data at its own boundary |
| `Handled` | Processing finished at the sink's declared handling boundary |

For a pipe with explicit handling acknowledgements, the graph completes the
acknowledgement only after the required handling succeeds. It will not connect
an acceptance-only sink as though that sink confirms handling.

`Acknowledgement::complete(Handled)` advances the pipe's handled position.
Dropping the handle does not acknowledge the data. A failed outcome leaves
retained input unacknowledged rather than erasing the obligation.

`Delivery` is neither cloneable nor serializable. The envelope is ordinary
shareable data; the one-shot acknowledgement is a local capability, not a field
to encode into that data.

### 4.3 Buffering and retention on one connection

The same source and sink can use different transport behavior:

| Transport | Data held between source and sink | Full/slow behavior |
|---|---|---|
| `BoundedPipe` | Volatile FIFO queue | Waits for capacity; no persistent replay or handling acknowledgement |
| Byte-bounded pipe | Volatile FIFO with envelope-count and byte reservations | Waits for capacity under both bounds |
| `BroadcastPipe` | Bounded volatile history | Does not wait for a slow receiver; missing history is reported or explicitly skipped with notification |
| `RetainedPipe` | Journal plus consumer handled position | Retains unacknowledged data; backpressures or explicitly prunes according to its retention policy |

A retained store can be in memory or persistent. **Retention is not the same
as crash durability.** Retained history in memory can support another read while
that store exists, but not recovery after losing the process.

With `RetentionPolicy::Backpressure`, the source must wait when required data
cannot be safely reclaimed. With `PruneOldest`, older entries can be removed,
and the receiver must detect when its requested position is no longer available.
An explicit skip is different from handling the missing records successfully.

Capabilities describe these properties separately. `FifoPerStream` concerns
order; `Backpressure` concerns capacity; `DurableAcceptance` concerns survival of
accepted data; `ExplicitAcknowledgement` concerns handling; `Replay` concerns
reading saved data again. The graph checks required capabilities rather than
inferring all of them from the word "persistent."

### 4.4 Ordering and failures

One output stream belongs to one source/output port. New emissions have
increasing sequences. A FIFO pipe preserves that stream's emission order;
wall-clock timestamps cannot reorder it.

An idle pipe suspends its receiver. `receive()` returning `None` means closed
and drained, not "no data available right now."

A send failure returns the envelope and identifies definite nonacceptance or
uncertain durable acceptance. If a send was cancelled without a result, the
caller cannot assume that nothing was stored. Replay or receipt lookup must
resolve an uncertain outcome before it is treated as new input.

### 4.5 Batching does not change the data contract

Even with one source and one sink, there are two different batches:

- A `ChangeSet` groups operations in **one envelope**.
- Pipe/sink batching groups **several complete envelopes** for one invocation.

`send_batch` reports the accepted prefix, the failed envelope and the unattempted
remainder. It is not an all-or-nothing transaction.

For sink batching, `max_inputs` is a hard envelope count. The operation count is
a flush target; the host does not split an envelope to meet it. Each envelope
retains its identity and operation ordinals. Batching alone neither combines
their schemas nor makes their effects atomic.

### 4.6 Count and byte limits

Queue capacity counts envelopes, not records or bytes. An envelope containing
one operation and an envelope containing a thousand operations each occupy one
envelope slot.

Byte accounting uses the complete encoded binary envelope, including metadata,
context and lineage. Some paths allow one oversized envelope alone rather than
wait forever for a reservation that could never fit. That behavior is not a
hard maximum on an individual payload.

Buffered-byte quotas do not include every pending sender, already received
message, component-private allocation or stored business value. Paged history
can limit payload reads and caches while still retaining accounting metadata
per stored entry. A pipe capacity is not a total process-memory limit.

Sources: [pipe interfaces][pipes], [bounded transports][bounded],
[broadcast][broadcast], [retained delivery][retained-pipe],
[retained storage][retained-store], [sink processing contract][component-dataflow].

## 5. Identity, sequence, time and context

These distinctions already matter in the direct source/sink case. They do not
require an intervening transformation.

### 5.1 Which record, operation and delivery?

| Identity or position | Question it answers |
|---|---|
| `RecordId` | Which record does this change address? |
| `ChangeSetId` plus operation ordinal | Which operation in this batch is it? |
| `EnvelopeId` | Which emitted event is it? |
| `StreamId` plus emission sequence | Where does it occur on this source output? |
| Pipe journal position | Where did the pipe store it? |
| Consumer handled position | How much of that stored data has the sink finished? |

`RecordId`, `ChangeSetId` and `EnvelopeId` contain a namespace and nonempty opaque
`Bytes`. They are supplied identities, not content hashes. Named IDs such as
`PortId` and `StreamId` wrap shared strings and reject empty names, whitespace
and control characters.

A source can update the same record in several events. Its `RecordId` remains
the same; the events and individual operations have different identities.
A pipe may store an event at a journal position unrelated to the source's
sequence. Neither number should be substituted for the other.

The general emission rule allows gaps in increasing stream sequences. Specific
admission or recovery contracts can impose stricter continuity requirements.

### 5.2 Time and source positions

`SystemMetadata` holds the stream, emission sequence, optional
`DateTime<Utc>` timestamp and optional source-position bytes.

The timestamp describes time associated with the event. FIFO transport is
ordered by emission, not by assuming every later event has a later timestamp.
Application-specific times inside records are interpreted by their payload
schema.

A source position is an opaque resume token. Its presence alone proves neither
persistence nor completed handling. The source defines how the bytes are
interpreted; the transport must not invent an ordering by comparing them as
strings.

### 5.3 Context accompanies the event

`ProcessingContext` is an append-only sequence of `ContextEntry` contributions.
Each entry contains a contributor ID, key and a value: `Bool`, `Signed`,
`Unsigned`, shared `String` or shared `Bytes`.

Appending an entry does not alter the event or overwrite earlier entries.
Iteration is newest first, and duplicate keys remain visible. Code must use
the applicable reader's lookup rules rather than assume context is a mutable
last-write-wins map.

The immutable event and record bytes can be shared through `Arc` and `Bytes`.
Cloning an envelope preserves that sharing; appending context to the clone
changes only its annotation chain.

Acknowledgements, transactions and index handles are not context values.
They remain local capabilities outside the serializable envelope.

Sources: [identity types][records], [metadata and context][envelopes].

## 6. Transformers and schema changes

A **transformer** is a component between source and sink. It consumes input
envelopes and produces output envelopes.

```mermaid
flowchart LR
    source["Source"] -->|"Schema A through a pipe"| transform["Transformer"]
    transform -->|"Schema B through a pipe"| sink["Sink"]
```

All boxes are components and all arrows carry data through pipes. Schema A and
schema B can be different. The source output must match the transformer's
input; the transformer's output must separately match the sink's input.
The pipe on neither side performs the conversion.

### 6.1 Each side has its own data contract

For example, an application might define:

| Boundary | Illustrative schema | Record payload |
|---|---|---|
| Source output / transformer input | `example.reading-text` | A measurement encoded as text |
| Transformer output / sink input | `example.reading-number` | A validated numeric representation |

These example names are not built-in Drasi schemas. The transformer decodes
according to the first schema, converts and validates the value, then constructs
a record using the second schema. It can preserve the record's business identity
if that identity is valid under the output schema, but it must emit a new event
describing the transformed data.

A transformer can also keep the same schema. Filtering, normalization and
expansion need not change the representation contract.

### 6.2 Zero, one or many outputs

`Transformer::transform(InputEnvelope)` returns `Vec<OutputEnvelope>`.
The input-port name identifies where data arrived; each output names its
destination port within the transformer.

A transformer can:

- Produce no output because it filtered the input.
- Produce one transformed envelope.
- Expand the input into multiple ordered operations or multiple envelopes.
- Retain input for later output, if its processing contract supports that.

Output events use the transformer's own stream and increasing emission
sequences. Reusing an input's sequence for several new emissions is not valid.
An empty output change set can explicitly carry progress when filtering removes
all records; no output at all and an empty progress-bearing envelope are different.

### 6.3 Derivation preserves provenance, not mutable input

`ChangeEnvelope::derive` creates a new event with the transformer's output
identity, schema-consistent change set and system metadata. It shares the
existing context and adds the input's identity/system metadata to lineage.

Lineage does not retain the parent's record payload. It follows a single chain;
it is not a complete record of every input that a component has processed.
The transformer cannot mutate an input record merely because it received an
owned envelope value: the underlying data is immutable and may be shared.

For recoverable stateful transformations, `GraphProducerIdentity` and
`GraphProducerProgress` identify logical output separately from transport
emissions. Replaying already committed output can assign a new emission number
without making that output new business work. Inherited source metadata is
provenance, not automatically this transformer's own checkpoint.

### 6.4 When is the input considered handled?

With one downstream sink, the graph:

1. Receives the input and keeps its pipe acknowledgement separately.
2. Invokes the transformation.
3. Validates and forwards the returned emissions.
4. Calls `delivery_completed` after the outgoing pipe accepted that batch.
5. Finishes any bounded continuations before processing the next input.
6. Confirms applicable handled-input progress and completes the input's
   acknowledgement.

`delivery_completed` means completed **outgoing handoff**, not that the
downstream sink finished its own processing. If recovery depends on that
handoff surviving a crash, the outgoing pipe must actually provide durable
acceptance.

Stateful transformations also have to preserve the relationship between their
input position, changed state and outgoing data. Returning an output vector
does not establish that those pieces committed together; the persistence
section explains the transaction boundaries.

Sources: [transformer contract][component-dataflow], [derivation][envelopes],
[producer identities][producer-progress], [forwarding][graph-routing].

## 7. Fan-out and fan-in

The previous rules apply to every connection when there is more than one
producer or consumer. Multiple connections do not introduce a different
envelope format.
The processing graph allows branching and merging, but data paths cannot
contain cycles.

### 7.1 Fan-out: one output, several consumers

```mermaid
flowchart LR
    source["Source"] -->|"Schema S"| left["Sink A"]
    source -->|"Schema S"| right["Sink B"]
```

When these branches use the same source output port, both sinks must accept
its schema. The graph forwards the emitted envelope to each required branch.
A transformer output can fan out in exactly the same way.

Envelope clones share the immutable event, records and payload bytes. Each
branch can append its own context without modifying its sibling's context.
Sharing payload memory does not merge delivery obligations: each sink still
has its own handling progress.

Independent pipes are sent to separately. If one branch accepts before another
fails, the first acceptance is not rolled back. Recoverable producers retain
their output and use destination-aware replay; they cannot assume fan-out was
an atomic all-or-nothing send.

A slow required branch can backpressure the producer and therefore delay later
output on other branches. Independent branches are not a promise that the
producer can always ignore the slowest one.

### 7.2 Shared retained fan-out

`QosChannel` stores one producer stream with independent named subscriber
cursors. `QosPipe` connects one of those subscribers to a consumer.
The graph recognizes branches using that same channel and appends once rather
than store one copy per reader.

Each subscriber has an independent position and acknowledgement. A disconnected
required subscriber retains its obligation. Safe reclamation follows the
required consumers' progress, not the fastest one.

`SubscriptionStart` defines a new reader's initial cut: earliest retained data,
latest, or after a position. Reopening an existing subscriber retains its saved
cursor instead of silently applying a new start point.

For independent byte-bounded pipes, each branch charges the complete envelope
even when its Rust allocations are shared. A shared journal has its own
single-copy accounting and per-reader progress.

### 7.3 Fan-in: several outputs, one consumer

```mermaid
flowchart LR
    first["Source A"] -->|"Schema S"| sink["Sink"]
    second["Source B"] -->|"Schema S"| sink
```

Two source outputs may connect to the same sink input when they satisfy that
port's schema and delivery requirements. Each output retains its own stream
identity and order.

If the sources use different schemas, they cannot both feed that one
single-schema input. The consumer needs separate appropriately typed input
ports, or explicit transformations that bring the data into a common schema.

Fan-in does not automatically combine records by key or produce a relationship
between them. The receiving component defines any such processing. The
transport only determines which available input is supplied next.

### 7.4 Choosing among inputs

The default `InputMergePolicy::Arrival` uses available arrival order.
`EventTimeAcrossStreams` compares the currently available stream heads without
reordering the admitted sequence within a producer. It does not wait for an
idle source; untimed heads preserve their arrival boundary.

This is not proof that an earlier timestamp cannot arrive later. A
`SourceTimeMergeTransformer` supplies explicit bounded reordering:

- It retains data, orders buffered events by source time and emits them on its
  own output stream.
- Reorder windows, maximum residence time and optional idle-source treatment
  determine when events become eligible.
- Envelope-count and full-envelope-byte bounds limit buffered and pending data.
- Late input is failed-and-retained, routed separately, or explicitly discarded.
- Its persistent form retains the buffered/pending data, identities and progress
  needed to recover that ordering work.

Reordering cannot undo arbitrary processing already performed downstream.
The distinction between merging available heads and deliberately waiting for
more data matters even when both components use the same payload schema.

### 7.5 Retained admission uses the same channel

`SourceAdmission` can publish into an actual QoS channel through a bounded,
graph-driven handle rather than through the source's `next()` method.
It does not create another journal; all of that source's outputs use the bound
channel.

Admission identifies a request using `ProducerSession { incarnation, producer,
epoch }` and a producer request sequence. Accepted data and a retry receipt are
saved together. `AdmissionReceipt` contains the session, request sequence and
journal position, not the downstream handled positions.

The same session/sequence and content can resolve to the prior receipt.
Conflicting payloads, expired sessions/receipts and unexpected sequences are
rejected. Admission receipts and replay receipts for an already identified
persistent producer output are separate modes; neither creates an end-to-end
processing transaction.

Sources: [forwarding][graph-routing], [shared envelopes][envelopes],
[QoS channels][qos], [pipe merge policy][pipes], [time merger][time-merge],
[admission handle][admission], [admission types][admission-data].

## 8. Queries

A continuous query specializes the transformer model: it consumes changes to
elements, maintains indexed state, and produces changes to result rows.

```mermaid
flowchart LR
    source["Source"] -->|"drasi.graph-change"| query["Query"]
    query -->|"drasi.query-row"| sink["Sink"]
```

This is a concrete instance of the schema-A-to-schema-B transformation. Here,
"graph data" means the nodes and relationships evaluated inside the query.
Those are data records, not the processing components or their pipe connections.

### 8.1 Representations along the query path

The implementation deliberately uses different types for stored graph data,
messages between components, evaluator values and ordinary API results.

| Layer | Principal types | Purpose |
|---|---|---|
| Business-data graph | `Element`, `ElementMetadata`, `ElementReference`, `ElementPropertyMap`, `ElementValue` | Represent the nodes and relationships a query indexes |
| Source mutation | `SourceChange` | Describe an insert, patch update, delete or scheduled reevaluation |
| Ordinary source exchange | `SourceEvent`, `SourceEventWrapper` | Pair a change/control message with source sequence, time, position and profiling |
| Computation data | `Record`, `ChangeOperation`, `ChangeSet`, `ChangeEvent`, `ChangeEnvelope` | Carry immutable, schema-checked data between components |
| Port-addressed data | `InputEnvelope`, `OutputEnvelope` | Associate an envelope with a component's input or output port |
| Pipe delivery | `Delivery`, `EnqueueReceipt`, `Acknowledgement` | Distinguish queued data from the local capability to confirm handling |
| Query evaluation | `VariableValue`, `QueryVariables`, `QueryPartEvaluationContext` | Maintain typed bindings and before/after changes while evaluating a query |
| Native query output | `DecodedQueryRow`, `QueryResultSnapshot`, `QueryRecoveryView` | Describe typed result rows, their current set and retained change history |
| Ordinary query exchange | `ResultDiff`, `QueryResult`, `SnapshotResponse`, `OutboxResponse` | Project query data into the ordinary result-reading and reaction interfaces |
| Recovery progress | `SourceCheckpoint`, `SourceProgressSnapshot`, `ConsumerCheckpoint`, `DeliveryProgress` | Record distinct input, query-output and consumer completion positions |

The model types are defined in `drasi-core-types`. Shared envelope types and
codecs are defined in `drasi-computation-contracts` and re-exported through
`drasi_lib::computation::v1`. Query execution, pipes and state ownership reside in
DrasiLib and Core.

The codec implementation also uses private typed-change intermediates. Those are
conversion helpers, **not another representation carried on graph edges**.
For example, the private `drasi.internal.graph-change` descriptor is not the
public `drasi.graph-change` port schema.

### 8.2 Graph elements

`ElementReference` consists of two `Arc<str>` fields:

```text
ElementReference
  source_id
  element_id
```

Both fields participate in identity. Two sources can use the same `element_id`
without referring to the same element. Conversely, changing a label or property
does not give an element a new identity.

`Element` has two variants:

```text
Node
  metadata: ElementMetadata
  properties: ElementPropertyMap

Relation
  metadata: ElementMetadata
  in_node: ElementReference
  out_node: ElementReference
  properties: ElementPropertyMap
```

`in_node` is the reference at the incoming end of the relationship and
`out_node` is the reference at its outgoing end. Endpoints are full
source-qualified references, not unqualified names. A relationship has its own
identity and properties independently of its endpoints.
For `(a)-[r]->(b)`, `r.in_node` addresses `a` and `r.out_node` addresses `b`.

`ElementMetadata` contains the reference, an `Arc<[Arc<str>]>` of labels and
`effective_from: u64`. `effective_from` is **milliseconds since the Unix epoch**,
not the time when a pipe accepted the envelope. Mutation admission checks its
range; zero is allowed as an unknown-time value used by initial loading.

These elements are indexed per query. A source serving two queries supplies
changes to two processing owners; it does not create a shared mutable business
graph that all evaluators modify.

### 8.3 Property values and evaluator values

`ElementValue` supports:

- Null, Boolean, signed 64-bit integer and floating-point value.
- Shared string, list and nested property object.
- Local date-time and fixed-offset zoned date-time.

`VariableValue`, used during evaluation, is broader. It also represents unsigned
integer values, dates, local/zoned times, durations, ranges, expressions,
references, complete elements, metadata and the `Awaiting` marker used by
time-dependent evaluation.

`QueryVariables` is a `BTreeMap<Box<str>, VariableValue>`. Its keys are bindings
or projected column names, not element-property identities.

This distinction matters when writing a transformer. An evaluator value is not
automatically a valid source property. In particular, a query value can retain
temporal or unsigned information that `ElementValue` cannot represent in the
same form. Do not use a JSON round trip as a lossless conversion between them.

### 8.4 Source mutation semantics

| `SourceChange` variant | Evaluator meaning |
|---|---|
| `Insert { element }` | Supply a full element. If that identity already exists, replace its prior contribution rather than count another copy |
| `Update { element }` | Patch an element's properties: supplied properties take precedence, missing properties are filled from the current element |
| `Delete { metadata }` | Remove the addressed element using its identity and effective time; a full property image is unnecessary |
| `Future { future_ref }` | Reevaluate scheduled work for an element, retaining its original time and due time |

Patch merging is defined by the graph model, not by the pipe. It is not a
generic JSON Patch program. Labels and relationship endpoints are fields of the
incoming element; `merge_missing_properties` specifically merges missing
properties.

Before changing an index, the evaluator reads the current element and any saved
deletion-time marker:

- A change older than the current element/deletion marker is skipped.
- An identical current element contributes no new result.
- A node and relationship cannot replace each other under one live identity.
- A deletion leaves a tombstone recording its effective time, preventing an
  older insert from resurrecting it.

A skipped stale mutation can still count as **handled input**. Its source
checkpoint must not remain behind merely because it produced no result change.

Sources: [model types][elements], [property values][element-values],
[mutations][source-changes], [mutation admission and evaluation][evaluator].

### 8.5 Standard query-path schemas

There is a common **envelope structure**, not one universal payload schema.
These are the three standard data schemas relevant to continuous queries:

| Schema ID | Version | Record encoding | Record meaning |
|---|---:|---|---|
| `drasi.graph-change` | 1 | `drasi-source-change-bincode1` | One `SourceChange` per operation image |
| `drasi.query-row` | 1 | `drasi-query-row-bincode1` | Typed query row, signature and row/aggregation metadata |
| `drasi.source-transaction` | 1 | `drasi-source-transaction-bincode1` | One complete transaction frame containing ordered source changes |

Custom schemas can use the same record/envelope/pipe machinery. A pipe does not
translate between a custom schema and these standard schemas.


### 8.6 Graph-change records

`GraphChangeCodec` uses the record identity:

```text
namespace = "drasi.graph-element"
value     = bincode-encoded ElementReference
```

The payload is the bincode encoding of `SourceChange`, not just a property map.
The normal mapping is:

| Source value | Envelope operation | Image |
|---|---|---|
| Insert | `Added` | Full after-image |
| Update | `Updated` with `Patch` semantics | Patch after-image |
| Delete | `Deleted` | Partial before-image containing delete metadata |
| Future reevaluation | `Updated` with `Patch` semantics | Patch after-image containing the future reference |

A graph-schema full replacement can also be represented as a generic
`Updated { semantics: Replace, ... }` carrying a full insert image.
`decode_changes` converts that into a delete followed by an insert at the new
effective time. It is not silently interpreted as a patch.

An identity-only delete is permitted. When no delete metadata supplies its time,
the decoder requires a nonnegative envelope timestamp and derives the effective
milliseconds from it. It does not invent a current timestamp.

The graph validator decodes the record and checks that its embedded reference
and variant agree with the external `RecordId` and `RecordImage`.
The bincode decoders use fixed-integer encoding, reject trailing bytes and apply
a read limit based on the supplied payload.

### 8.7 Query input processing

All continuous queries are implemented by `ContinuousQueryTransformer`, using
`ComputationQuery` and `QueryEvaluator`. The wrapper owns input/output progress;
the evaluator works with graph changes and query indexes.

Ordinary query inputs use `RankedInputQueue`. It chooses among available source
heads using timestamp and source rank, without moving a later admitted source
sequence ahead of an earlier one. Source dispatch mode determines whether this
input path waits for capacity or uses the explicitly lossy full-queue behavior.
Query output dispatch is separate. This is the query-specific use of the fan-in
ordering described above, not a new data representation.

For one input envelope, the query:

1. Determines its logical producer/source position and validates it against
   the saved input owner.
2. Checks whether the input was already handled. A configured position comparator
   can interpret an opaque source cursor; otherwise the appropriate sequence
   watermark is used.
3. Decodes the graph changes, or the complete source transaction.
4. Evaluates changes and produces `QueryPartEvaluationContext` values.
5. Encodes result differences and stages the input checkpoint and output state.
6. Commits through its processing owner.
7. Updates the public result view, then confirms input progress and forwards
   resulting envelopes.

A duplicate can advance a consumed **transport** position without reevaluating
the logical input. A new but filtered input can commit with no outgoing row
envelope. Query output sequence advances for actual result output, not once per
source message.

### 8.8 From source mutations to row differences

The compiled query has MATCH slots and a path solver. The element index maintains
which stored elements can occupy those slots and their relationships.

For an insert/update, evaluation determines the old solutions involving the
element, merges patch properties if appropriate, updates indexed data and
determines the new solutions. Deletes compare the old contributions against
their absence. These changes are projected through the query parts, filters
and expressions.

The result is not a fresh full query result set for every input. It is a
collection of typed differences:

```text
Adding      { after, row_signature }
Updating    { before, after, row_signature }
Removing    { before, row_signature }
Aggregation { before?, after, grouping_keys,
              default_before, default_after, row_signature }
Noop
```

The evaluator suppresses unchanged projected updates. Aggregation results for
one mutation are collapsed before output. Internal grouping transitions still
need their default/current state even when terminal visible values do not change.

### 8.9 Query working indexes

The query's resource bundle contains several distinct indexes:

| Resource | Data held |
|---|---|
| `ElementIndex` | Current graph elements, slot membership, relationship lookups and supported synthetic joins |
| `ElementArchiveIndex` | Historical element versions where supplied by the provider |
| `ResultIndex` | Aggregation accumulators, current/default intermediate parts, time state and element tombstones |
| `FutureQueue` | Deferred reevaluation records |
| `CheckpointStore` | Handled input positions and query/recovery markers |
| `OutboxWriter` | Retained emitted result batches and relevant metadata records |
| `LiveResultsWriter` | Current materialized output rows |

`ResultIndex` is **not** the same thing as `LiveResultsWriter`. The former is
working state needed to evaluate subsequent changes; the latter is the current
answer exposed to readers.

`ResultKey` distinguishes grouping values, input hashes and element references.
`ResultOwner` separates function accumulators, current/default parts, transaction
time, real time and tombstones. Grouping equality/hashing uses its own
`eq_for_groupby`/`hash_for_groupby` semantics; arbitrary JSON equality is not an
equivalent index key.

Synthetic joins are maintained through element-index join support. They provide
query relationships between matching elements; they are not computation-graph
edges.

### 8.10 Query-row records

`QueryChangeCodec` gives each row a query-scoped record identity:

```text
namespace = "drasi.query-row/" + lowercase hex of the query ID's UTF-8 bytes
value     = the row signature as eight big-endian bytes
```

The encoded row contains:

```text
query_id
signature
values: BTreeMap<String, typed value>
kind: Row | Aggregation {
    grouping_keys,
    default_before,
    default_after
}
```

Normal row additions use full after-images. Updates use full replacement
after-images and preserve the before-image when supplied. Removals retain a
before-image and its key. Query-row patches are not the graph-property patch
mechanism.

Aggregation flags describe the evaluator's default/current group handling.
They are not general instructions to delete a row. In particular, a default
before value need not be an already-applied contribution, and a default after
value does not independently prove that the group has no contributors.

The validator checks the signature and query scope against the encoded payload.
Snapshot projection additionally requires full rows belonging to the requested
query.

### 8.11 Lossless typed query values

The query-row value codec uses a tagged representation, rather than serializing
`VariableValue` through JSON:

- Integers are encoded as signed or unsigned values; nonnegative values may
  use the unsigned representation.
- Floating-point values are represented using their `f64` bits, including
  nonfinite values and signed zero.
- Lists and objects recursively encode typed values.
- Temporal values retain their specific kind. Zoned date-time can retain its
  optional timezone name; duration retains calendar and elapsed-time parts.
- Elements, metadata, references, expressions, ranges and `Awaiting` have
  explicit variants.

`QueryChangeCodec::row_values_to_json` is a **projection for readers**, not the
stored row format. Dates and other typed values can become strings or other
JSON representations. Code that needs type fidelity should decode the typed
row instead of round-tripping its rendered JSON.

### 8.12 Row identity and output generation

Query row signatures are `u64` values produced by the evaluator:

- Nonaggregating rows use the MATCH binding identity.
- Grouped rows use the grouping identity.

Two rows with equal projected values can therefore have different signatures.
Comparing only the visible column values loses that distinction.
Conversely, changing a value under one signature normally updates that row
rather than introducing an unrelated row.

`QueryRecoveryIdentity` combines the construction/instance scope, graph ID,
query ID, configuration hash and optional volatile incarnation. Persistent
queries omit the volatile incarnation and can retain identity when the same
stored query is reopened. Output generation separately distinguishes a reset
result history from the previous one.

These are scoped identities used by the implementation, not cryptographic
proofs of data uniqueness. Row signatures and configuration hashes must not be
replaced with hashes of rendered JSON.

### 8.13 Complete source transactions

A source-transaction envelope has exactly one `Added` operation at ordinal zero,
with a full record. Its payload contains:

```text
version
source_id
transaction_id: bytes
position: bytes
changes: Vec<SourceChange>
```

The record's namespace is the source ID; its identity bytes are the transaction
ID. The frame's commit position must agree with the envelope's source position.
A matching `drasi.source-transaction.v1` annotation identifies the group.

All changes must belong to that source. Scheduled `Future` changes cannot appear
inside the group. Assembly and decoding enforce change-count, byte and time
bounds. An incomplete or rejected assembly cannot publish its successful prefix
as a complete transaction.

This schema is not ordinary batching. A vector of graph-change operations does
not become a complete source transaction merely because it arrived together.

Ordinary graph-change batches are evaluated in order. They may contain
intermediate row differences even when their storage changes are committed in
one query operation.

Complete-source-transaction processing additionally collapses the transaction's
result transitions by row signature. It keeps the first before-state and final
after-state, removes insert-then-delete/no-net-change outcomes, and rejects
discontinuous row transitions or incompatible grouping metadata.

This gives the query's downstream reader the transaction's final result
changes, rather than each intermediate update. It does not make two different
queries or arbitrary sinks share the source transaction.

### 8.14 Logical data and replay transport

A replay can transport previously committed output under a **new emission
sequence**. `ChangeEnvelope::reemit` creates the new envelope identity and
transport sequence while retaining its change set, annotations and lineage.

For query output, the logical output position is retained in
`drasi.query-output-sequence.v1`. For stateful graph-change producers,
`GraphProducerProgress` carries a producer identity and logical sequence.
Consumers that perform durable deduplication use this identity and logical
sequence, not whichever transport number was assigned to the retry.

An ordinary source adapter also has two positions: the source's original
sequence and the adapter's output-stream sequence. Original source metadata
is preserved in `drasi.legacy-source.v1`. That name is an internal annotation
key; it does not imply another runtime.

After a transformer creates a new logical output, inherited source metadata is
provenance. It is **not automatically the transformer's own recovery cursor**.
Progress resolution considers the immediate producer marker and any validated
identity-preserving path before using an ancestor's position.

### 8.15 Time and scheduled reevaluation

| Time | Meaning |
|---|---|
| `ElementMetadata.effective_from` | Business mutation time, in Unix milliseconds |
| `SourceEventWrapper.timestamp` | Source-exchange event time carried into adapter metadata |
| `SystemMetadata.timestamp` | Optional envelope time; used by explicit ordering paths |
| Query output metadata timestamp | Time the query result is produced |
| `FutureElementRef.original_time` / `due_time` | Original transaction time and scheduled reevaluation time |
| Profiling nanoseconds | Measurements of elapsed pipeline stages |

Sequence controls per-stream emission order. A timestamp cannot move a later
source sequence ahead of an earlier admitted sequence and thereby justify
skipping the earlier input.

Profiling is observational. Immediate-query completion timings are attached
after its transaction commits; those fields cannot already be in the committed
outbox bytes. Replay identity normalization excludes those immediate completion
timings but preserves inherited context.

`FutureElementRef` stores an element reference, original time, due time and group
signature. Query functions can arrange reevaluation by writing to the query's
future queue.

`QueryScheduledSource` reads a committed scheduling view and emits a marked
empty graph-change envelope when work is due. It does not remove the future
record itself. The query pops/processes due work under its own transaction and
publishes resulting row differences through its ordinary output path.

The due-work marker is distinct from a business source ID. Scheduled
reevaluation also distinguishes original transaction time from due/realtime.
Reaching the final ordinary input sequence therefore does not prove that all
future work has finished.

### 8.16 Graph-change middleware

`MiddlewareTransformer` decodes graph changes, runs the selected
`SourceMiddlewarePipeline`, and encodes the resulting changes back into
`drasi.graph-change`. A pipeline can filter, rewrite or expand changes.

For a newly processed input, this transformer produces one logical output
change-set batch, including an empty batch for a fully filtered input.
Its producer-progress marker lets downstream owners advance through filtered
input without pretending that a data record was emitted. In persistent mode,
it saves its processing state, logical input/output progress and unconfirmed
output under its transaction owner.

Query-local source middleware follows the evaluator's source pipeline instead.
Shared preprocessing outside queries and query-local preprocessing therefore
run at different positions in the data path; shared preprocessing can avoid
doing the same transformation separately for each query.

### 8.17 Standard source and query metadata

Payload schema and metadata answer different questions. The following
annotations retain source provenance, identify produced results or distinguish
control data. They are not application columns.

| Annotation key | Data and interpretation |
|---|---|
| `drasi.legacy-source.v1` | Original source ID, optional source sequence/position, source timestamp, profiling and optional source-description metadata |
| `drasi.graph-producer-progress.v1` | Immediate graph producer identity and logical sequence, separate from emission sequence |
| `drasi.query-output.v1` | Query ID, optional contributing source ID, production timestamp, metadata map and profiling |
| `drasi.query-output-sequence.v1` | Logical query result sequence, preserved when output is reemitted |
| `drasi.query-generation.v1` | Query output generation |
| `drasi.query-recovery-identity.v1` | Scoped query identity and persistent/volatile incarnation information |
| `drasi.query-snapshot.v1` | Whether the query-row envelope is a recovery snapshot |
| `drasi.query-progress-only.v1` | An explicit recovery-progress decision rather than a row replacement |
| `drasi.query.futures-due` | Scheduled-work notification on an empty graph-change batch |
| `drasi.source-transaction.v1` | Complete-transaction source and transaction identity |
| `drasi.source-transaction-replay.v1` | Stable source-transaction replay sequence where supplied |

Readers do not use one universal dictionary rule for these keys. For example,
source metadata preserves the original source contribution, while query-output
readers locate the appropriate current query metadata and verify its identity.
Producer-progress resolution checks which output stream a marker belongs to;
it cannot treat any matching annotation found in an ancestor as current progress.

Application/context annotations and the advisory change hashes can travel with
the same event without becoming checkpoint authority. Append-only context
preserves their history; schema-specific readers establish their meaning.

### 8.18 Query-path identity reference

| Identity/position | Identifies |
|---|---|
| `ElementReference` or `RecordId` | The business element or query row being changed |
| `ChangeSetId` + operation ordinal | One operation within a logical batch |
| `EnvelopeId` | One emitted event |
| `SystemMetadata.stream` + `sequence` | Emission order on a producer's output stream |
| Source checkpoint sequence + optional position bytes | The source input that a processing owner has committed |
| `GraphProducerIdentity` + logical sequence | A transformer's stable output across transport replay |
| `QueryRecoveryIdentity` + output generation + query output sequence | A particular query result history |
| Pipe journal position | Where the pipe stored an accepted envelope |
| Consumer checkpoint or delivery-ledger position | What that consumer actually finished |

The general identities retain the meanings established for Source/Sink.
Queries add input checkpoints, typed row identities and an independently
numbered result history; none replaces the transport's own position.

Sources: [graph codec][graph-codec], [query codec][query-codec],
[typed values][value-codec], [source transactions][source-transactions],
[ranked input][ranked], [middleware][middleware], [query wrapper][query],
[Core query adapter][query-adapter], [evaluator][evaluator],
[transaction-result collapse][transaction-results], [working indexes][result-index],
[scheduled inputs][scheduling], [input progress][input-progress],
[query identity][query-identity].

## 9. Initial data and the live-data boundary

Initial loading uses the same graph-change representation as live evaluation,
but has a separate completion boundary.

`ComputationBootstrapSnapshot` contains:

- An asynchronous stream of `Result<ChangeEnvelope>`.
- A list of `BootstrapWatermark` values: stream, optional source ID, sequence
  and optional position bytes.

The query establishes source preparation/recovered progress before consuming
initial rows. Live input may be admitted and buffered while initial loading
finishes, but it must not be evaluated as if that loading were complete.

Initial rows update the query indexes and materialized result rows. They do not
become an ordinary live outbox batch for each initial record. After the snapshot
stream finishes, the provider can return final watermarks and bounded handover
state that were not available earlier.

The final input watermarks, completed-bootstrap marker and applicable completion
state are committed together. Only then is the initial result view ready for
normal readers and live processing.

`BootstrapState` is bounded opaque initialization metadata kept in the query's
transaction domain. It is not another source stream and is not the snapshot
rows themselves. The current bound is 65,536 bytes.

Several initial-data providers can be composed. Their watermarks and
initialization state remain separate within the containing query's preparation.
A query-level "bootstrap complete" marker does not invent a checkpoint for an
individual source that never supplied one.

If loading is interrupted, existing partial data is not declared a completed
snapshot. Recovery examines the incomplete marker; it does not infer completion
from a nonempty index.

Sources: [bootstrap data contracts][bootstrap-data],
[query bootstrap interface][query-bootstrap], [bootstrap processing][query-recovery].

## 10. Result snapshots, retained output and readers

### 10.1 One current result view

`QueryResults` exposes one query-owned `QueryOutputState`. The important fields
are:

```text
rows: im::HashMap<RecordId, Record>
sequence: u64
outbox: VecDeque<ChangeEnvelope>
capacity: NonZeroUsize
generation: u64
identity: Option<QueryRecoveryIdentity>
```

Applying committed output changes the keyed rows and appends the result envelope
to retained output. Add/replace installs the full after-image; delete removes
its `RecordId`. Equal-valued rows with different identities remain separate.

`QueryResultSnapshot` contains shared immutable rows, `as_of_sequence` and
generation. Cloning the persistent map shares structure; it is not a deep copy
of every record payload. A snapshot is a current set of rows, not a replay
log and not a promise that every source input has been processed.

Retained output is an ordered suffix of emitted batches. Its capacity is
independent of the number of current result rows.

### 10.2 Reading ordinary results

`QueryApi` and `QueryResultsCatalog` are read access to that same query state;
they do not evaluate the query again. `QueryResultsOutlet` forwards query-row
envelopes into catalog subscriptions.

At the ordinary boundary, typed rows become `ResultDiff` and `QueryResult`.
The row signature accompanies the projected JSON data. `SnapshotResponse`
retains keyed rows and exposes both ordinary row iteration and keyed iteration,
together with sequence, configuration hash and output generation.

The JSON projection is deferred to this boundary where possible. A consumer
reading native typed records need not construct an ordinary `QueryResult`.

### 10.3 Joining a snapshot to live output

Reading a snapshot and subscribing later can lose changes that occur between
those actions. `subscribe_query_results` instead establishes the live reader
before taking the snapshot, then filters out output already represented by that
snapshot's sequence.

The reader keeps the output generation with its position. A reset/replacement
cannot be interpreted as the continuation of an old sequence space. If a live
broadcast reader lags, retained history can fill the gap; unavailable history is
an explicit error.

This subscription is connection-local. It is not itself a durable consumer
checkpoint.

### 10.4 Snapshot and progress-only envelopes

Recovery-aware query-row paths distinguish:

- **Snapshot envelope:** a complete set of full row additions plus explicit
  snapshot metadata and its query output boundary.
- **Ordinary change envelope:** differences to apply after the current boundary.
- **Progress-only recovery envelope:** an explicit authorized skip decision,
  without claiming to replace rows or execute missing operations.

`QueryReplayTransformer` and `CheckpointedSink` combine a source result view
with a `ConsumerProgressStore`. `ConsumerCheckpoint` retains sequence,
generation and actual query recovery identity. A normal checkpoint advance and
an explicit snapshot/skip recovery decision are different store operations.

A consumer must not identify a snapshot simply by seeing many additions, or a
skip by seeing an empty batch. The markers and recovery decision establish the
meaning.

Sources: [query output state][query-state], [catalog/outlet][catalog],
[ordinary result exchange][query-exchange], [live reader][native-query],
[consumer recovery][consumer-recovery].

## 11. Persistence and transaction boundaries

### 11.1 The stores answer different questions

| Stored data | Question it answers |
|---|---|
| Source WAL/journal | Which accepted source changes remain available for replay? |
| Query element/working indexes | What state is needed to evaluate the next mutation? |
| Query input checkpoints | Which logical input has this query committed? |
| Query live-result store | What rows currently make up the answer? |
| Query outbox | Which produced result batches can be read or handed off again? |
| Pipe journal and subscriber cursors | Which accepted envelopes still have transport/handling obligations? |
| Consumer checkpoint or delivery ledger | Which query outputs or individual operations did this consumer complete? |

A generic byte state store is not automatically a transaction spanning these
objects. Saving input progress independently before saving query state would
permit recovery to skip work that was never committed.

**Query data flow with storage**

This diagram expands the ordinary source/query/sink path introduced earlier.
Square boxes are producers/consumers at its boundary, rounded boxes are
operations on data, and cylinders are logical stores. Operations need not be
separate components or tasks. Stores may be in memory or persistent; their
shape does not promise durability. Every arrow names data being transferred.

```mermaid
flowchart TB
    source["Source component"]
    sink["Sink or reaction"]
    reader["Result-reading application"]

    encode("1. Encode source input")
    evaluate("2. Evaluate graph changes")
    deliver("3. Deliver result changes")
    read("4. Read query results")

    working[("Query working state<br/>Elements, accumulators,<br/>scheduled work and input checkpoints")]
    results[("Query output state<br/>Current rows, output sequence<br/>and retained result batches")]

    source -->|"SourceEventWrapper containing SourceChange"| encode
    encode -->|"ChangeEnvelope: drasi.graph-change<br/>through the input pipe"| evaluate
    working -->|"Existing elements, intermediate values<br/>and committed input positions"| evaluate
    evaluate -->|"Changed index entries, scheduled work<br/>and new input checkpoints"| working
    results -->|"Current output sequence<br/>and result-history metadata"| evaluate
    evaluate -->|"Result-row mutations, output sequence<br/>and retained query-row batch"| results
    evaluate -->|"ChangeEnvelope: drasi.query-row<br/>through output pipes"| deliver
    deliver -->|"ChangeEnvelope to a native sink<br/>or QueryResult to an ordinary reaction"| sink
    reader -->|"Snapshot request or replay position"| read
    results -->|"Current typed rows<br/>or retained query-row batches"| read
    read -->|"Result snapshot<br/>or ordered result changes"| reader
```

Processes 1 and 3 include ordinary API data adaptation. Native sources already
produce checked envelopes and do not need the `SourceEventWrapper` conversion.
Process 2 is `ContinuousQueryTransformer` using `ComputationQuery` and
`QueryEvaluator`. Process 4 reads existing output state rather than evaluating
the query again.

In the atomic persistent path, the relevant working-state mutations, output
mutations and input checkpoint commit together before output is published.
Arrows describe data dependencies, not a separate transaction per arrow.
Handling acknowledgements and replay are separate flows, described in
[pipe delivery](#41-following-one-envelope) and
[recovery](#12-recovery-and-duplicate-handling).

### 11.2 Atomic query publication

`ComputationIndexes` supplies the evaluator's `IndexSet` and optional checkpoint,
outbox and live-result resources. An `AtomicResultTransaction` exists only when
those resources participate in the same actual `TransactionDomain` and session
control. Matching filenames or a "durable" label do not prove that participation.

The persistent atomic path stages:

```text
one query transaction
  element / archive / aggregation / future-queue changes
  input checkpoint and transport progress, where required
  encoded output batch, if the answer changed
  materialized output-row mutations
  query output sequence and applicable handoff metadata
commit
  publish in-memory result view
  publish committed input progress
  hand output to downstream pipes
```

The query outbox stores the versioned envelope representation. Live result rows
store the typed query-row bytes keyed by row signature. Recovery validates them
as records again instead of trusting an untyped JSON object.
The query's retained-output `EnvelopeCodec` has a 64 MiB encoded-frame limit.

The in-memory result view is updated **after** commit and before reporting
committed input progress. A read of input progress followed by the result
position therefore does not intentionally expose committed input ahead of its
published answer. It is still not an atomic snapshot of every component in the
graph.

### 11.3 Memory-only and non-atomic processing

In-memory providers do not gain crash recovery by exposing a checkpoint
interface. Ordinary memory queries have volatile checkpoints for coordination;
a fresh independent memory store does not retain them.

The explicit non-atomic publication path separates evaluation from output
publication. When output storage is persistent, a pending-publication marker
records the unsafe interval. Recovery rejects an unfinished publication rather
than presenting the old output as current.

Failure or cancellation is not proof of rollback. An interrupted computation
with uncertain storage state cannot simply continue as a healthy evaluator.
Persistent recovery must reestablish what actually committed. In-memory
processing does not promise transactional rollback merely because it uses the
same wrapper types.

### 11.4 Saving processing and outgoing queues together

`SharedStorageGroup` can bind one processor and its QoS journals to one actual
`ComputationTransactionGroup`.

The producer reserves outgoing capacity before processing, then stages the
query/transformer state and participating journal appends in the shared
transaction. Committed journal state becomes visible after the common commit.
This closes the gap between committing processing and persisting those outgoing
handoffs.

It does not create a transaction around the entire computation graph. A
downstream query's later evaluation, an unrelated store and a remote effect do
not join simply because they consume that output.

### 11.5 Several transformations in one transaction

`TransactionTransformer` runs an ordered, linear set of
`TransactionalTransformer` steps using a borrowed `TransactionContext`.
Each step returns one envelope: filtering yields an empty change set, and
expansion yields more ordered operations.

Intermediate results stay inside the container. The common transaction commits
the steps' state and final output together. Mutable business state must use the
borrowed transaction; an ordinary transformer with independent writes cannot
become transactional merely by being placed between two such steps.

This is distinct from a source-transaction envelope. One groups internal
processing steps; the other preserves the boundary of already grouped input.

### 11.6 Durability is a stated failure boundary

`StorageDurability` records separate guarantees for process restart, power loss
and storage loss. Each can be guaranteed, not guaranteed or unknown.
"Persistent," "transactional" and "replicated" are not interchangeable.

When a guarantee depends on several stores, it is limited by their common
guarantee. A local atomic commit does not make the data survive permanent loss
of its storage.

Sources: [resource/transaction bundle][indexes], [transaction owner][transactions],
[query publication][query], [shared storage][shared-storage],
[linear transformations][transaction-transformer], [durability types][durability].

## 12. Recovery and duplicate handling

### 12.1 Recovering the source input

The query records `SourceCheckpoint { sequence, source_position }` under either
`SourceProgressKey::Source` or `SourceProgressKey::Stream`. The optional position
is bounded opaque bytes, not a string to compare lexicographically by default.

`InputPositionPolicy` can supply a source-specific comparator. A partitioned
position may compare only the relevant partition; it need not have one total
order across all partitions.

`QuerySourceProgress` publishes committed progress through a coalescing watch
snapshot. The watch is not persistence; its values come from the processing
owner's real checkpoint state. `transport_sequences` remains separate from
logical source checkpoints because replay transport can advance without new
business input.

For one source with several required consumers, `ConsumerProgressSet` retains
the actual owners and their individual snapshots. A fast consumer's checkpoint
does not replace the slower consumers' positions.

For ordinary WAL-backed input, replay preserves original source sequences:

1. Read the committed WAL head.
2. Establish the subscriber's resume position and register it for live data.
3. Read the retained suffix after its checkpoint through that captured head.
4. Deliver that suffix before subsequent live data.

The redb WAL head comes from committed metadata, not an in-memory sequence
reservation. A positive head with an empty requested suffix can mean data was
pruned; it cannot be treated as successful empty replay. The ordinary source
replay path also checks that the required suffix reaches the captured head
without gaps.

WAL cleanup waits for the initial required subscriptions on a retained log.
Their actual positions are established before a faster restored query can
authorize pruning. Replay registration and pruning share exclusion around the
position handles and actual prune operation.

This source-WAL recovery path is separate from reconstructing arbitrary
undrained native persistent input-pipe frames. Having saved consumer checkpoints
alone is not a universal description of every queued transport frame.

### 12.2 Reconstructing query state

Before exposing recovered output, the query checks:

- Input ownership and stored query identity.
- Completed initial loading and absence of an unfinished reset/publication.
- Output generation and committed result sequence.
- Valid live-row encodings and identities.
- Consistency of retained output, including its tail against the committed
  output sequence.
- Saved output destinations and outstanding handoff progress, where tracked.

Rows without an identifying committed sequence, malformed records, a missing
required suffix or a conflicting identity are errors. A storage read error is
not evidence that the store is empty.

Recovered output is loaded into `QueryOutputState`, and unconfirmed handoffs
are queued for replay before new input is processed.

### 12.3 Replaying committed output

A persistent query can commit result sequence 8 and fail before confirming all
outgoing acceptance. Recovery must resend that result, **not reevaluate its
source mutation to invent result 9**.

Tracked query output keeps a delivered-output position and a separate emission
counter. Replay uses `reemit` to produce a valid new transport emission while
preserving logical query sequence, output generation, typed rows and provenance.
Retention cannot evict output still required for unconfirmed handoff.

`OutputBindings` records the actual receipt-tracked destinations and their
identities. Pending output cannot silently be considered delivered to a changed
set of destinations.

QoS output-replay receipts identify the immediate persistent producer and
canonical content. A matching retry resolves to its previous acceptance;
conflicting data or an expired receipt fails. This output-replay mode is
different from client admission receipts and does not redefine original source
identity.

### 12.4 Consumer completion inside Drasi

For a `Handled` sink, successful handling states that its declared processing
boundary finished. An `Accepted` sink only states that it queued/accepted work.
The graph will not treat an acceptance-only sink as a completed-handling
acknowledger.

`DeliveryRunner` adds persistent operation-level progress where required:

1. Identify the persistent producer and logical input batch.
2. Save/validate its content digest and operation count.
3. Invoke the handler for unfinished operations in order.
4. Advance the completed prefix only after the actual operation succeeds.
5. Retain the batch until every operation finishes.

`DeliveryId` combines consumer scope/port, producer identity, logical sequence
and operation ordinal. It does not depend on retry transport identity.
The ledger also stores a content digest to reject different data presented as
the same logical operation.

With `TransactionalDeliveryHandler`, one operation's local business state and
completion record can commit together under the runner's transaction.
That atomicity is **per operation**, not automatically the complete input batch.

For nontransactional handling, an effect can succeed before its completion record
is saved. The same operation ID may be delivered again. The internal ledger
does not remove that uncertainty by itself.

### 12.5 Reset is a new output history

A reset/removal of processing state is represented with an explicit generation
and an in-progress marker. Recovery must reject partly cleared state.
Completed reconstruction has a newer output generation so old consumer
positions cannot match a fresh sequence space accidentally.

This document does not prescribe when to reset. At the data level, the important
rule is that resetting rows, indexes, progress and output inconsistently cannot
be made safe by changing only a counter or clearing a result cache.

Sources: [source progress][source-progress], [position comparison][position-policy],
[consumer sets][consumer-progress], [ordinary WAL replay][source-base],
[WAL provider][wal-provider], [query recovery][query-recovery],
[handoff tracking][query-delivery], [replay identity][output-identity],
[delivery ledger][delivery-runner].

## 13. An input followed through processing and replay

Consider the query:

```cypher
MATCH (p:Product)
WHERE p.quantity < 10
RETURN elementId(p) AS id, p.quantity AS quantity
```

The following positions are illustrative independent counters, not required
numeric relationships between stages.

### 13.1 Before the change

The query's element index contains product `inventory/P-7` with quantity 12.
It does not appear in the current low-stock result set. The query has committed
source input 41 and produced output sequence 7.

### 13.2 Input becomes an envelope

The next source change is a patch setting quantity to 8:

```text
SourceChange::Update
  Element::Node
    reference: ("inventory", "P-7")
    labels: ["Product"]
    effective_from: a valid millisecond time
    properties: {"quantity": Integer(8)}
```

Other existing properties remain because this is an update patch. The ordinary
source wrapper records source sequence 42 and its optional replay position.
The source adapter emits transport sequence 15 on its own stream.

The graph-change envelope contains an `Updated/Patch` operation, a
`drasi.graph-element` record key for `inventory/P-7`, and the encoded
`SourceChange`. Its source annotation still identifies source sequence 42.
Transport sequence 15 is not written as source checkpoint 42.

### 13.3 Evaluation changes the answer

The query decodes the patch, finds the prior element, updates its indexed state
and compares the old/new MATCH contributions. Quantity 12 did not satisfy the
filter; quantity 8 does.

The evaluator produces an `Adding` result for a row signature, say `R`.
`R` is an opaque evaluator identity, not the string `P-7` and not a hash the
consumer should recompute from the JSON.

The query-row operation is a full addition containing:

```text
query_id: low-stock
signature: R
values:
  id: String("P-7")
  quantity: typed integer 8
kind: Row
```

Its record key is scoped to `low-stock` and `R`. The query atomically commits
source checkpoint 42, the updated query indexes, this materialized row and
output sequence 8. It then publishes the result view and committed input
progress.

### 13.4 Handoff and consumer progress

Tracked output might use transport emission 21 for logical query output 8.
A durable pipe might store it at journal position 103.

These are now four separate numbers:

```text
source committed input     42
query produced output       8
query transport emission   21
pipe journal position     103
```

The sink handles the operation, records its completion where applicable, and
the graph completes the pipe acknowledgement. Another branch has its own
handling obligation even if it received the same immutable envelope.

### 13.5 Failure after query commit

If the process fails after output 8 commits but before its handoff is confirmed:

- Input recovery does not apply source change 42 as new business work.
- The current row is reconstructed from committed query state.
- Output recovery replays logical result 8 under a new transport emission.
- A receipt-aware destination compares logical identity and content, not the
  new envelope ID, to recognize a previous acceptance.

If a consumer previously completed the effect but its completion write was
uncertain, the operation may be retried with the same delivery ID. The query
transaction cannot decide that consumer's outcome.

### 13.6 An input that changes no result

A later patch changing an unprojected description while quantity stays 8 can
advance source input to 43 without advancing output beyond 8. A duplicate
replay of input 42 similarly must not manufacture another visible addition.
This is why "number of source changes" and "latest query result sequence" are
not interchangeable completion measures.

## 14. Reading progress correctly

`DrasiLib::get_query_progress_metrics` reads actual query input/output
publications without storage or network I/O. It returns source/stream input
sequences, output sequence, persistence information and component/output
identity. Missing/unready observations are not fabricated zeros.

The same values can feed telemetry, but telemetry is not a recovery store.
A collector's old sample does not prove the current process reached that
position.

To establish completion for a finite, time-independent workload:

1. Know the actual final accepted input boundary.
2. Observe every required query committed that input under the correct
   producer/query identities.
3. Identify the output boundaries resulting from that completed input.
4. Verify each required consumer completed its data through those boundaries.
5. Read final state at a boundary where further input cannot invalidate the
   comparison.

An empty pipe, zero recent output, a completed source sender or a flushed logger
does not independently establish those facts. Time-dependent queries additionally
need an explicit boundary for future work.

Useful invariants when reading the implementation:

- A property key, row signature, operation ID and stream position solve
  different identity problems.
- A port validates the data contract; a pipe does not change the payload schema.
- An envelope is a batch of operations, not necessarily one business event or
  one transaction.
- An inherited source annotation is not automatically the immediate producer's
  checkpoint.
- Full result rows and incremental output history are different stored data.
- Committed input may advance without output.
- An acceptance receipt is not a handling acknowledgement.
- Successful query commit does not imply successful destination work.
- Retention must account for every required consumer, not only the fastest one.
- Missing or uncertain data must not become an empty, apparently complete view.

## 15. Implementation map

These links follow the internal data path rather than the component-management
or plugin-authoring structure.

| Topic | Main implementation |
|---|---|
| Element identity, metadata, properties and mutations | [Element][elements], [ElementValue][element-values], [SourceChange][source-changes] |
| Source/query exchange wrappers | [exchange.rs][exchange] |
| Schema, record images and ordered changes | [data.rs][records] |
| Envelope metadata, derivation and context | [envelope.rs][envelopes] |
| Standard payload codecs | [graph_codec.rs][graph-codec], [query_codec.rs][query-codec], [source_transaction.rs][source-transactions] |
| Typed query-value storage | [value_codec.rs][value-codec] |
| Outer envelope serialization | [codec.rs][json-codec], [binary_codec.rs][binary-codec] |
| Port contracts and forwarding | [ports.rs][ports], [component.rs][component-dataflow], [graph.rs][graph-routing] |
| Pipe delivery and acknowledgement | [pipe.rs][pipes], [retained_pipe.rs][retained-pipe], [qos_pipe.rs][qos] |
| Graph-driven retained admission | [SourceAdmission][admission], [session and receipt types][admission-data] |
| Input ordering and preprocessing | [ranked_pipe.rs][ranked], [middleware.rs][middleware], [time_merge.rs][time-merge] |
| Query processing and result transitions | [query.rs][query], [query_adapter.rs][query-adapter], [evaluator.rs][evaluator], [query_results.rs][transaction-results] |
| Query working state | [element_index.rs][element-index], [result_index.rs][result-index], [indexes.rs][indexes] |
| Initial data and scheduled data | [bootstrap.rs][bootstrap-data], [query_recovery.rs][query-recovery], [query_scheduling.rs][scheduling] |
| Current rows, output history and read access | [query_state.rs][query-state], [query_catalog.rs][catalog], [native_query.rs][native-query] |
| Logical producer and input identities | [producer_identity.rs][producer-progress], [producer_progress.rs][input-progress], [query_identity.rs][query-identity] |
| Input/consumer progress | [source_progress.rs][source-progress], [consumer_progress.rs][consumer-progress], [input_position.rs][position-policy] |
| Query handoff, consumer recovery and operation completion | [query_delivery.rs][query-delivery], [consumer_recovery.rs][consumer-recovery], [delivery.rs][delivery-runner] |
| Source WAL storage and replay | [SourceBase][source-base], [redb WAL][wal-provider] |
| Shared transaction state | [transaction.rs][transactions], [shared_storage.rs][shared-storage], [transaction_transformer.rs][transaction-transformer] |

[elements]: ../../core-types/src/models/element.rs
[element-values]: ../../core-types/src/models/element_value.rs
[source-changes]: ../../core-types/src/models/source_change.rs
[exchange]: ../../components/computation-contracts/src/exchange.rs
[records]: ../../components/computation-contracts/src/data.rs
[envelopes]: ../../components/computation-contracts/src/envelope.rs
[graph-codec]: ../../components/computation-contracts/src/graph_codec.rs
[query-codec]: ../../components/computation-contracts/src/query_codec.rs
[value-codec]: ../../components/computation-contracts/src/internal/typed_change/value_codec.rs
[source-transactions]: ../../components/computation-contracts/src/source_transaction.rs
[json-codec]: ../../components/computation-contracts/src/codec.rs
[binary-codec]: ../../components/computation-contracts/src/binary_codec.rs
[ports]: ../../components/computation-contracts/src/ports.rs
[component-dataflow]: ../src/computation/v1/component.rs
[graph-routing]: ../src/computation/v1/graph.rs
[pipes]: ../src/computation/v1/pipe.rs
[bounded]: ../src/computation/v1/bounded_pipe.rs
[broadcast]: ../src/computation/v1/broadcast_pipe.rs
[ranked]: ../src/computation/v1/ranked_pipe.rs
[retained-store]: ../src/computation/v1/retained_store.rs
[retained-pipe]: ../src/computation/v1/retained_pipe.rs
[qos]: ../src/computation/v1/qos_pipe.rs
[admission]: ../src/computation/v1/qos_pipe/ingress.rs
[admission-data]: ../../components/computation-contracts/src/admission_messages.rs
[time-merge]: ../src/computation/v1/time_merge.rs
[middleware]: ../src/computation/v1/middleware.rs
[query]: ../src/computation/v1/query.rs
[query-adapter]: ../../core/src/computation/query_adapter.rs
[evaluator]: ../../core/src/query/evaluator.rs
[transaction-results]: ../../core/src/computation/query_results.rs
[element-index]: ../../core/src/interface/element_index.rs
[result-index]: ../../core/src/interface/result_index.rs
[scheduling]: ../src/computation/v1/query_scheduling.rs
[bootstrap-data]: ../../components/computation-contracts/src/bootstrap.rs
[query-bootstrap]: ../src/computation/v1/query_bootstrap.rs
[query-recovery]: ../src/computation/v1/query_recovery.rs
[query-state]: ../src/computation/internal/query_state.rs
[catalog]: ../src/computation/v1/query_catalog.rs
[query-exchange]: ../src/queries/output_state.rs
[native-query]: ../src/computation/runtime/native_query.rs
[consumer-recovery]: ../src/computation/v1/consumer_recovery.rs
[indexes]: ../../core/src/computation/indexes.rs
[transactions]: ../../core/src/computation/transaction.rs
[shared-storage]: ../src/computation/v1/shared_storage.rs
[transaction-transformer]: ../src/computation/v1/transaction_transformer.rs
[durability]: ../../core-types/src/interface/durability.rs
[producer-progress]: ../../components/computation-contracts/src/producer_identity.rs
[input-progress]: ../src/computation/v1/producer_progress.rs
[query-identity]: ../../components/computation-contracts/src/query_identity.rs
[source-progress]: ../src/computation/v1/source_progress.rs
[position-policy]: ../../components/computation-contracts/src/input_position.rs
[consumer-progress]: ../src/computation/v1/consumer_progress.rs
[source-base]: ../src/sources/base.rs
[wal-provider]: ../../components/wals/redb/src/provider.rs
[query-delivery]: ../src/computation/v1/query_delivery.rs
[output-identity]: ../../components/computation-contracts/src/output_identity.rs
[delivery-runner]: ../src/computation/v1/delivery.rs
