# Native MySQL stored-procedure sink: design

`drasi.mysql/storedproc` (crate `drasi-computation-sink-storedproc-mysql`,
implementation `1`, configuration version `1`, native ABI 5.0) calls MySQL
stored procedures for standard graph changes from any source, transformer or
query. It is an **external consumer** of the host `DeliveryRunner`
(`ConsumerMode::External`) with a **destination cursor**: each operation's
procedure effect and its receipt commit in one MySQL transaction.

The runtime lives in `drasi-mysql-common` (`storedproc::{config, delivery,
render, session, runtime}`) so the plugin crate stays a thin factory,
lifecycle and management layer. The cursor protocol, its failure rules and the
atomic skip are shared with the PostgreSQL and SQL Server sinks
(`drasi-computation-storedproc-common`,
[README](../../common/storedproc/README.md)); `storedproc::delivery`
implements only its MySQL `Session` (SQL, the KILL proof, error codes).

## Purpose and scope

- Turn each graph change into one rendered `CALL` with positional `?`
  parameters, bound with MySQL prepared statements.
- Deliver each operation exactly once when the procedure keeps to the
  delivery transaction (see [Delivery profile](#delivery-profile)).
- Out of scope: MariaDB (the owned protocol layer refuses it), client
  certificates, batching several operations into one transaction.

## Architecture

```text
host DeliveryRunner ──item──▶ MySqlStoredProcSink (plugin, DeliveryHandler)
   records input,              │ render (compiled templates, strict)
   advances completed prefix   ▼
                         activation session (one persistent MySQL connection)
                               │ START TRANSACTION
                               │ SELECT … FROM progress FOR UPDATE   (row lock)
                               │ [GET_LOCK + COUNT + INSERT]          (first stream)
                               │ UPDATE progress SET pending=k        (prepared)
                               │ CALL proc(?, ?, …)  + in-transaction status check
                               │ UPDATE progress SET completed=completed+1, pending=-1
                               │ COMMIT
                         control session (only to KILL a retired session)
```

- **One activation-owned session** (SP-06/SMY-03). Start opens it; every
  operation reuses it. The legacy two-connections-per-input design is gone.
- **Session retirement with ownership proof.** Every session is opened with a
  random connection-attribute nonce and proves it owns its connection ID. A
  session whose command did not finish (command timeout, cancelled `handle`,
  transport failure, unknown commit) is *retired*: a short-lived control
  session verifies the nonce, `KILL`s the connection and waits until the server
  no longer lists it. Only then may a replacement session run, so an abandoned
  transaction can neither commit later nor hold the cursor row lock behind a
  retry. If the proof cannot finish, the session stays owned and the next
  `handle`, Stop or Start retries it first.
- **One attempt.** A connection attempt is bounded by `connectTimeoutMs`
  (and, at Start, the lifecycle deadline and cancellation). A transport
  failure or server overload (1040, 1053, 1203) is transient: the graph
  retries the delivery (or the host lifecycle policy the Start).
  Authentication and authorization failures are permanent.
- **Timeouts per command.** The protocol layer runs with a one-day I/O bound;
  the sink bounds every statement with `commandTimeoutMs` and every attempt
  with `connectTimeoutMs`, so those settings change in place.

## Input decoding and change projection

The host hands the consumer one `DeliveryItem` per operation of an input. The
sink maps the item's stream to its bound output (`SinkOutputs`), decodes the
operation with `GraphChangeBatch::operation_notification`, and renders the
truthful change context (`operation`, `output`, `sequence`, `timestamp`,
`identity`, only the `element`, `before`, `after`, `removedProperties` and
`effectiveFrom` the record carries, and `deliveryId`, the host's stable
operation identity; see [SINKS.md](../../SINKS.md)). `ADD`
selects `added`, `REPLACE` and `PATCH` select `updated`, `DELETE` selects
`deleted`. A scheduling control or recovery frame fails.

Route precedence: the full output alias, then its last dotted segment, then
`defaultTemplate`. No template, or a condition that renders empty SQL, still
advances the cursor (the operation is complete with no effect).

## Delivery profile

**`Consumer/External + destination cursor`.**

| Window | Behaviour |
|---|---|
| Loss | None. The runner records the input before any effect, keeps the envelope until the whole input completes and advances only the completed prefix. A failed operation fails the input; nothing is skipped unless `onPermanentFailure: skip` settles it (recorded in the dead-letter table). |
| Duplicate, procedure keeps the transaction | None. A replay of an operation inside the cursor's completed prefix commits nothing and succeeds. This covers a crash or cancellation after `COMMIT` but before the host recorded completion, an unknown commit outcome, and a host that lost its local ledger. |
| Procedure ends the transaction | Refused. Every cursor write and the `CALL` are prepared statements whose reply carries the server's in-transaction status; a procedure that runs `COMMIT`/`ROLLBACK` or causes an implicit commit (DDL, `LOCK TABLES`, …) leaves the transaction closed, so the operation fails permanently and the cursor is never advanced in autocommit. If the effect committed with the `pending` mark, every later attempt refuses to run it again (no duplicate). A procedure that commits and immediately reopens a transaction is not detectable, but its whole effect still commits with the cursor or, on failure, the `pending` mark prevents a re-run. Non-transactional tables (MyISAM) remain outside the guarantee. |
| External side effects | At-least-once (anything the procedure does outside MySQL is not rolled back). |

Exactly-once conditions: InnoDB (or another transactional engine) for every
table the procedure writes, no transaction control or implicit commit inside
the procedure, and the cursor table in the same server.

Completion: `handle` returns `Ok` only after `COMMIT` succeeded.

## Destination cursor protocol (format version 4)

Shared by the three stored-procedure sinks and documented once in the
[storedproc-common README](../../common/storedproc/README.md). The tables
live in the configured `database` unless `progressTable` names
`schema.table`:

| Column | Type | Meaning |
|---|---|---|
| `owner` | `CHAR(64)` ascii | lowercase hex SHA-256 of `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id` |
| `stream_key` | `VARCHAR(128)` ascii | the host `DeliveryPosition` stream |
| `version` | `SMALLINT` | `4` |
| `sequence` | `VARCHAR(20)` | decimal producer sequence of the current batch |
| `digest` | `BINARY(32)` | canonical batch digest |
| `operations` | `BIGINT` | operations in the current batch |
| `completed` | `BIGINT` | completed operation prefix |
| `epoch` | `VARCHAR(64)` ascii | empty for exactly-once streams; otherwise the at-least-once producer epoch |
| `generation` | `BIGINT` | the durable query's output generation (0 when none); a newer one rolls the row over, an older one is refused |
| `pending` | `BIGINT` | the operation whose effect is running, or -1; durable only if the effect escaped the transaction |

`PRIMARY KEY (owner, stream_key)`, InnoDB. The dead-letter table
`<progressTable>_dead_letter` (`PRIMARY KEY (owner, delivery_id)`) records
skipped operations with their error. First-stream admission for an owner is
serialised with `GET_LOCK`, released after `COMMIT`/`ROLLBACK`. Earlier
formats are refused at Start, not migrated.

### Delivery guarantee and producer epochs

The factory's pure describer (`Factory::describe`) reads the sink's own
`deliveryGuarantee` and declares it as the input's `InputDelivery` in the
sink's `ComponentDescription`; the Host forwards the description unchanged.
The graph compares it with the producer's declared output when it binds the
edge (`check_edge`). `deliveryGuarantee: exactlyOnce` (the default) accepts
only a `Durable`, replayable producer; a `Volatile` one fails the edge with
`Incompatibility::ProducerIdentity`, and an `Untracked` or undeclared producer
fails under either guarantee. The failure is recorded on the edge
(`BindingState::Failed`) and on the sink node (terminal `Binding` failure);
the graph change itself is accepted. The plugin's own check at delivery is an
internal invariant guard, not the compatibility check.

`deliveryGuarantee: atLeastOnce` additionally accepts a producer with a
volatile identity: an in-memory query or a volatile graph producer.
Its output has no persistent progress, so the host derives a weaker identity
(`Candidate::new_volatile`): the stream key excludes the producer's per-run
incarnation, and the item's `DeliveryPosition` (version 2) carries an
`epoch`, a SHA-256 of the producer incarnation (and, for a query, its
generation and configuration hash). The host delivers such a batch without its `DeliveryRunner` ledger
(nothing can resume across a restart), calling the plugin per operation and
stopping at the first failure. The cursor applies one extra rule: an item
whose epoch differs from the row's treats the row as absent and overwrites
it, so a new producer run starts a fresh cursor on the same row; the same
epoch deduplicates exactly as before. Exactly-once items (empty epoch) never
match an at-least-once row, nor the reverse (a permanent receipt conflict).


Rows are validated on every read (version 4, canonical decimal sequence,
32-byte digest, `0 <= completed <= operations`, a consistent `pending`).

## Retry and classification

Classification is by MySQL error code and typed error, never by message text
(`storedproc::delivery::classify`); the rules are the shared protocol's:

| Failure | Class | Notes |
|---|---|---|
| Deadlock (1213), lock wait timeout (1205) | transient | InnoDB rolled back; the session is kept and the graph retries the input |
| Session lost, `commandTimeoutMs` expired, fatal server error (1053, 1927, 3169, 4031) | transient | the session is KILLed and proven gone before any replacement runs |
| `COMMIT` reply lost | transient, outcome unknown | the session is retired; the next attempt resolves it from the cursor |
| `COMMIT` answered with a server error on a live session | the error's class | a definite rollback |
| Admission lock busy | transient | |
| Retired session not yet proven gone | transient | the next attempt retries the cleanup first |
| Connection refused, overload (1040, 1053, 1203) | transient | |
| Authentication or authorization at connect | permanent | |
| Procedure error (`SIGNAL`, constraint, missing procedure, privilege), template render failure (D-5) | permanent | rolled back; settled as skipped under `onPermanentFailure: skip` |
| The procedure ended the transaction | permanent | settled as skipped under `skip` |
| Receipt expired / conflict / gap / superseded generation, stream capacity | permanent | never skipped |

**One retry model.** Each `handle` is one attempt; the graph retries a
transient failure with its bounded backoff. The only immediate retry is a
reconnect when the idle session turned out to be dead before its transaction
started (restart, `wait_timeout`, `KILL`), because nothing executed.

No error latches the sink: after any failure the same input can be retried
without Stop and Start (SP-03).

## Templates

Compiled once at construction with the shared parameter-only rule
(`drasi_reaction_common::sql_template::validate_parameter_only`): every value
is a `{{param <path>}}` rendered as `?`; plain `{{x}}`, raw `{{{x}}}`, other
value helpers (`{{json x}}`), partials and decorators are rejected. `if`,
`unless`, `each` and `with` may select or exclude SQL text. Rendering is
strict: a path the change does not carry fails the change permanently.
Parameters map JSON to MySQL values: null → `NULL`, booleans → `0/1`,
integers (signed or unsigned) → integers, other numbers → `DOUBLE`, strings →
bytes, objects and arrays → their JSON text.

## Batching

The host's input is the batch; the sink never rejects an input for its size
(SMY-01 removed the 4 096-change and whole-input byte caps). Each operation is
its own transaction, bounded by `commandTimeoutMs` per statement and
`maxProtocolBytes` per packet. Host batching (`handle_batch`) is not used.

## Idempotency key

The cursor *is* the idempotency mechanism: `(owner, stream_key, sequence,
operation)` derived from the host's content-bound `DeliveryPosition` and
digest. No transport or configuration identity is part of it.

## Owned and borrowed state

Owned: this owner's rows in the cursor and dead-letter tables, the
activation session and any retired session awaiting proof. Borrowed: the
procedures, the business tables and the tables themselves (created when
absent, never dropped).

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing: configuration, templates and routes were validated and compiled at construction. |
| `start` | Requires completed cleanup. Opens the session (one attempt) and creates/verifies the cursor and dead-letter tables, within `ctx.deadline()` and `ctx.cancellation()`. A failed or cancelled start leaves nothing open and the object startable. Activation credentials, when bound, are used instead of literals. |
| `pause` / `resume` | Host-driven (D-6): the runner calls no `handle` while paused and settles the in-flight one first. Marks the in-place update boundary. |
| `stop` | `Drain`: the host awaited the in-flight operation; the idle session closes. `Abort`: the host dropped the in-flight `handle`; the session left mid-command is KILLed (rolling its transaction back). Bounded by `ctx.deadline()` (or `shutdownTimeoutMs`); on expiry Stop returns a retryable error and keeps the session owned. An earlier delivery failure is never a Stop failure. |
| `dispose` | `Preserve`: retire/close sessions. `Erase`: delete only this owner's cursor and dead-letter rows (idempotent; absent tables are fine), then close. |
| `handle` | One operation per call, as above. |

## Management

`Factory::assess` and `NativeManagement` classify every field (see
[CONFIGURATION.md](CONFIGURATION.md#updates)). In-place updates require a
completed pause; restart updates require a stopped sink; Replace is never an
instance update. `host`, `port`, `database` and `progressTable` are
Unsupported: they locate the receipts, so moving them would drop the receipt
of an in-flight operation and orphan this owner's rows. Remove the component
(Erase) and add it again.

## Security model

- `password` is the only secret field and is never printed by `Debug`;
  templates and routes are inspectable.
- Values reach SQL only as bound parameters; procedure names are literal text.
- **Secure by default.** `sslMode: require` (the default) requires TLS and
  verifies the certificate chain and host name; `tlsCaPem` adds a trusted CA.
  `sslMode: disable` is refused at construction for a host other than
  `localhost` or a loopback address unless `allowRemotePlaintext` is set, as
  in the PostgreSQL sink.
- KILL is issued only after the per-connection nonce proves ownership; a
  reused connection ID is never killed.
- The cursor table name is a validated identifier; every cursor write is a
  prepared statement with bound values; the owner and stream key in the two
  cursor reads are validated literals.

## Known limitations

- The host consumer path does not yet bind activation credentials
  (identity providers) to external consumers; the plugin supports them, but
  a Host must forward them before they take effect. Until then use literal
  credentials.
- In-memory (volatile) queries are accepted only with
  `deliveryGuarantee: atLeastOnce`, and are then delivered at least once:
  deduplicated within one producer run, never resumed across a restart.
  Without opt-in the edge fails when it is bound (see Delivery guarantee and
  producer epochs).
- A failed delivery is retried without Stop/Start, through the host as well.
  A delivery the host cancels mid-flight still requires Stop before more
  input, because its batch may not have settled.
- Erase after an activation-credential start uses the last activation's
  account, kept in memory only; a never-started object with bound credentials
  cannot erase and reports the owner so rows can be deleted manually.
- No metrics yet (X-11); failures are logged.

## Differences from the legacy reaction

| Legacy (`reactions/storedproc-mysql`) | Native |
|---|---|
| Connection pool | One persistent activation session plus a control session only for KILL |
| Command retry with backoff | Deadlocks, lock waits and lost connections are transient and the graph retries the input in place with backoff; the cursor makes retries idempotent |
| Log and skip failed commands | `onPermanentFailure: stall` (default) stalls the node; `skip` records the operation in the dead-letter table atomically with the cursor advance |
| `query_id`, `query_name`, `data` context | Truthful change context (`output.alias`, `operation`, `before`/`after`, …) |
| `{{json …}}` inline | Rejected; bind with `{{param …}}` |
| Nested `ConfigValue` settings | Flat camelCase fields, `password` secret |
| Unbounded input, no receipts | Per-operation transactions with a destination cursor |

## Delivery failures in the graph

The sink follows the graph's
[delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures) through
the shared protocol: one attempt per `handle`, a transient failure (rolled
back, session lost, commit outcome unknown, deadlock, lock wait) retried by
the graph in place, and a permanent one (a procedure, commit, template or
cursor rejection, transaction control) following `onPermanentFailure`. Under
`skip` the skip is recorded in the destination's dead-letter table in the
same transaction that advances the cursor, so it is exactly once; a cursor
rejection still stalls.
