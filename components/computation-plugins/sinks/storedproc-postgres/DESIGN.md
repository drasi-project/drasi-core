# Native PostgreSQL stored-procedure sink: design

`drasi-computation-sink-storedproc-postgres` provides one native
ComputationGraph factory, `drasi.postgres/storedproc` (implementation `1`,
configuration version `1`, native ABI 5.0). This document describes how it
works. [CONFIGURATION.md](CONFIGURATION.md) lists every setting and how to
operate the sink.

| Factory | Role | Completion profile |
|---|---|---|
| `drasi.postgres/storedproc` | `Component::Consumer` (`ConsumerMode::External`) | `Consumer/External` plus a destination cursor |

It consumes the standard `GraphChangeCodec` schema on its `in` port from any
producer: a source, a transformer or a query
([sink input contract](../../SINKS.md)).

## Purpose and scope

Call a PostgreSQL stored procedure (or any parameterized statement) for every
graph change, with per-operation exactly-once effects when the procedure keeps
its work inside the sink's transaction. Each change selects a command template
by output alias and operation; row values are always bound as parameters.

The sink owns one piece of destination state: its rows in the progress table
(the destination cursor). It owns one PostgreSQL connection per activation and
the task that drives it. It runs no other worker and opens no listener.

## Architecture

```
                 ┌──────────────────── PostgresStoredProcSink ─────────────────┐
 DeliveryRunner ►│ handle(item)  (one operation of one input, one attempt)     │
 (host: local    │   Delivery::deliver (storedproc-common, shared protocol)    │
  progress,      │     session healthy and idle? else discard + reconnect      │
  retention)     │     one READ COMMITTED transaction over PgSession:          │
                 │        read/lock cursor row (owner, stream_key) FOR UPDATE  │
                 │        admit stream (per-owner maxStreams) / roll over      │
                 │        mark pending ─► route ─► render ─► CALL $1..$n       │
                 │        advance cursor; COMMIT (synchronous_commit = on)     │
                 │     classify: transient / permanent / skipped               │
                 └─────────────────────────────────────────────────────────────┘
```

| Piece | Location | Responsibility |
|---|---|---|
| Factory | `src/lib.rs` | Metadata, published configuration details, describe (guarantee and `onPermanentFailure`), assess, activation credentials, construction without I/O |
| `PostgresStoredProcConfig`, `Commands` | `src/config.rs` | Fields, defaults, validation, template compilation and routing |
| `PostgresStoredProcSink` | `src/component.rs` | Lifecycle hooks, management apply, `DeliveryHandler`, command rendering |
| `PgConnector` | `src/connection.rs` | One connection attempt (TLS, timeouts) producing a `PgSession` |
| Planning | `src/management.rs` | Field-by-field update classification |
| `PgSession`, `ProgressTable`, SQLSTATE classification | `postgres-common/src/delivery/progress.rs` | The PostgreSQL `Session` of the shared protocol: format-4 cursor and dead-letter SQL, table preparation, Erase, commit-outcome and transaction-control detection |
| `Delivery`, `decide`, `OperationContext` | `computation-plugins/common/storedproc` | The cursor protocol, failure rules and atomic skip shared by the three stored-procedure sinks ([README](../../common/storedproc/README.md)) |
| `CompiledTemplate`, `SqlParam` | `postgres-common/src/sql.rs` | Parameter-only rendering and typed binding |
| Parameter-only rule | `reaction-common/src/sql_template.rs` | The single validator shared by the stored-procedure sinks |

## Input decoding and change projection

The host `DeliveryRunner` splits every input into operations and calls `handle`
once per operation with a `DeliveryItem` (stable identity, position and the
canonical batch digest). The sink checks the `in` port, maps the envelope's
stream to its configured output alias and builds the truthful change context
(`OperationContext`, shared by the stored-procedure sinks): `operation`
(`ADD`, `REPLACE`, `PATCH`, `DELETE`), `output` (`alias`, `component`,
`port`), `sequence`, `timestamp` (only when the envelope has one), `identity`
(`namespace`, `id`), only the `element`, `before`, `after`,
`removedProperties` and `effectiveFrom` that the record carries, and
`deliveryId`: the host's stable operation identity, the same across retries
and replays, for procedures that must deduplicate an external effect. Routing uses `added` for `ADD`, `updated` for `REPLACE` and
`PATCH`, and `deleted` for `DELETE`. A scheduling control or recovery frame
fails the input.

## Delivery profile: `Consumer/External` with a destination cursor

The host `DeliveryRunner` records the input's identity and exact content in
local storage before any destination work and advances only the completed
operation prefix. For each operation the sink opens one PostgreSQL transaction
that

1. reads (and row-locks) this owner's cursor row for the stream, or, for a new
   stream, takes the owner's advisory lock, checks `maxStreams` and inserts the
   row;
2. validates the operation against the cursor (see the cursor protocol);
3. marks the operation pending, renders and executes the command, and checks
   that the transaction is still the delivery transaction;
4. advances the cursor's completed count (clearing the pending mark), with
   `synchronous_commit = on` for this transaction;
5. commits.

The procedure's effect and the cursor advance therefore commit or roll back
together. `handle` returns `Ok` only after the commit succeeded, or when the
cursor shows that the operation was committed earlier.

### Exactly-once conditions

An operation's effect happens exactly once when all of these hold:

- the procedure (and everything it calls) uses only the sink's transaction: no
  `COMMIT`/`ROLLBACK` inside a procedure, no `dblink`/foreign-data writes, no
  external side effects (HTTP calls, `NOTIFY` consumers that act, file
  writes);
- nothing else modifies or deletes this owner's progress rows;
- the component keeps its identity (graph and component ID) and its
  `progressTable`.

**Procedures that control the transaction are refused.** PostgreSQL forbids
`COMMIT` or `ROLLBACK` in a procedure `CALL`ed inside a transaction block
(SQLSTATE `2D000`), so such a procedure fails every attempt with a permanent
error and nothing is committed; a template that issues `COMMIT` itself is
caught by the transaction-ID check and refused before the cursor advance
(see the shared protocol's transaction-control rule). Effects outside
PostgreSQL remain at-least-once; bind `deliveryId` to deduplicate them.

### Exact duplicate windows

With the conditions above, there is none: a retry after an unknown outcome
(timeout, lost connection, failed commit reply, process crash after the commit,
a lost local receipt) finds the advanced cursor and completes without an
effect. Without them (external effects), every retried
attempt of an operation whose first attempt reached the effect can repeat it:

1. a retry after a rolled-back attempt (serialization failure, deadlock,
   lock timeout, statement timeout);
2. a retry after a lost connection or an unknown commit outcome;
3. a replay of the input after a crash or after the host dropped the
   in-flight `handle`.

### Exact loss windows

- **None.** An operation without a configured command (no template, an empty
  template, or a template whose condition renders an empty command) completes
  deliberately and advances the cursor; that is configured policy, logged at
  debug level, not loss.
- **After Erase.** Erasing deletes this owner's cursor rows. A component
  re-created with the same identity starts without destination receipts, so a
  replayed input is applied again (a duplicate, not a loss).

### Cursor protocol (format 4)

The cursor and dead-letter tables, the per-operation decision, the failure
classes and the atomic skip are the shared protocol of the stored-procedure
sinks, documented once in the
[storedproc-common README](../../common/storedproc/README.md). In brief: one
row per `(owner, stream_key)` holds the current batch (`sequence`, `digest`,
`operations`), its completed prefix, the at-least-once `epoch`, the durable
query output `generation` and the `pending` mark; a duplicate completes
without an effect, a gap, conflict, expired receipt, unfinished prefix or
superseded generation is refused, and a newer generation (a query reset) or
epoch overwrites the row instead of adding one. Earlier formats are refused
at Start, not migrated.

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


Cursor rejections are permanent and never skipped, even under
`onPermanentFailure: skip`. A damaged row (wrong version, invalid sequence or
digest) is rejected.

The `owner` column lets many consumers share one table: each owner's stream
limit counts only its own rows, and Erase deletes only its own rows. The
legacy reaction keeps its own format-1 table (`public.drasi_delivery_stream_v1`)
through `PostgresDeliveryHandler`; the formats never share a table.

## Retry and classification

Classification is by SQLSTATE and error type, never by message text
(`postgres-common::delivery::progress::classify`), and the failure rules are
the shared protocol's:

| Failure | Class |
|---|---|
| `40001` serialization, `40P01` deadlock, `55P03` lock not available, `57014` statement timeout, class `53` (except `53300`) | transient; rolled back, the connection is kept |
| Class `08`, `57P01` (`pg_terminate_backend`), `57P02`, `57P03`, `53300`, closed connection, I/O, client-side command timeout (`commandTimeoutMs` + 5 s) | transient; the connection is discarded |
| COMMIT reply lost or timed out | transient, outcome unknown; the next attempt resolves it from the cursor |
| COMMIT answered with a SQLSTATE on a live connection (a deferred constraint, `40001` at commit) | the SQLSTATE's class (a constraint is permanent): a definite rollback, never "outcome unknown" |
| `2D000` (a procedure ran `COMMIT`/`ROLLBACK`), a template that committed | permanent |
| Connection refused, unreachable, TLS I/O, connect timeout | transient |
| Authentication (`28xxx`), unknown database (`3D000`), `42501` at connect | permanent |
| Any other SQLSTATE (procedure exception, constraint, type, privilege), parameter conversion, render failure | permanent |
| Cursor rejection, stream capacity | permanent, never skipped |

**One retry model.** `handle` makes one attempt and never sleeps: a transient
failure is returned as a transient `DeliveryFailure` and the graph retries
the same input with its bounded backoff (200 ms doubling to 30 s); the
destination cursor makes every retry idempotent. The only immediate retry is a
reconnect when the idle connection turned out to be dead before its
transaction began. There is no in-plugin retry budget.

A permanent failure follows `onPermanentFailure`: `stall` (default) returns it
and the graph stalls the node; `skip` settles a content failure (procedure,
commit, template or transaction control) by recording it in
`<progressTable>_dead_letter` and advancing the cursor in one transaction,
and reports it as skipped; the host's `DeliveryRunner` records the operation
as completed and the graph counts the skip. A replay never repeats it. The
sink keeps no latched state: after a fix (procedure, template, grant, a freed
stream slot) the next `handle` runs normally without Stop and Start.

If the host drops an in-flight `handle` (cancellation), the session is left
marked busy. The next `handle` discards it (aborting its driver) and
reconnects, because its transaction state is unknown; PostgreSQL rolls the
abandoned transaction back.

## Idempotency key

The destination cursor is the idempotency mechanism: the row keyed by
`(owner, stream_key)` with the batch `sequence`, `digest` and completed prefix
identifies every operation by its producer stream, sequence and ordinal. It
contains no retry, transport or configuration identity, so retries and replays
of the same operation are recognised across reconnects, restarts and processes.
Templates may also bind `sequence` and `identity` from the change context for
their own deduplication.

## Batching and large inputs

Host batching is not used; the runner hands the sink one operation at a time.
No input is rejected for its size: a large input becomes many operations, each
in its own short transaction. Timeouts apply to each command
(`commandTimeoutMs` server-side, plus a 5 s client grace), never to a whole
input.

## Owned and borrowed state

| State | Owner | Erase |
|---|---|---|
| Rows `WHERE owner = <this owner>` in `progressTable` and its dead-letter table | this sink | deleted |
| The progress and dead-letter tables themselves | shared (created by the first sink if permitted) | never dropped |
| Rows of other owners | other sinks | untouched |
| Business tables and procedures | the operator | untouched |
| Local delivery progress (`DeliveryRunner`) | the host | the host's own retirement |

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing to open: templates, routes, TLS material and the progress-table name were validated at construction. |
| `start` | Takes activation credentials if present, connects (one attempt bounded by `connectTimeoutMs` and `ctx.deadline()`), prepares the progress and dead-letter tables (see below) and the cursor statements (each command bounded by `commandTimeoutMs` + 5 s). Observes `ctx.cancellation()` throughout. A cancelled or failed start discards the connection and leaves the sink startable; connection failures are retryable (the host's lifecycle retry policy decides), privilege and configuration failures terminal. A second start is refused. |
| `pause` | Host-driven (D-6): the runner calls no `handle` while paused and awaits or drops the in-flight one. The connection driver only answers the sink's own commands and produces no work, so nothing needs gating. In-place updates require a completed pause. |
| `resume` | Clears the pause flag. |
| `stop` | `Drain`: closes the idle connection gracefully within `min(ctx.deadline(), shutdownTimeoutMs)`. `Abort` or an expired close drops the socket (PostgreSQL rolls back anything open). Nothing outlives Stop. An earlier connection failure is never reported as a Stop failure. |
| `dispose` | `Erase` first deletes `WHERE owner = <this owner>` from the progress and dead-letter tables over a fresh connection (idempotent: missing tables or no rows succeed), using the last activation's login if one was supplied; failures are retryable and the host retries with the same intent. Then (both intents) the connection is released. |

The connection driver task belongs to the session: it is created by start or
by a reconnect inside `handle` and aborted when the session is discarded or
dropped.

### Progress-table preparation

At start, in one transaction under advisory lock `(1146241363, 2)`:

1. the schema must exist and the role must have `USAGE` on it;
2. for each of the progress and dead-letter tables that exists, the role must
   have `SELECT`, `INSERT`, `UPDATE` and `DELETE` on it, and it must have the
   format-4 columns;
3. if either does not exist, the role must have `CREATE` on the schema; the
   sink then runs the documented `CREATE TABLE IF NOT EXISTS` statements.

Every failure names the missing object or privilege, for example
`role "sink" lacks CREATE on schema "public"; the progress table does not
exist: pre-create it ...`. PostgreSQL 15 and later no longer grant `CREATE` on
`public` to every role, so least-privilege roles use a pre-created table
(CONFIGURATION.md has the statement and grants). The step is safe to repeat and
to run concurrently.

## Management

`Factory::assess` and `NativeManagement` classify every field
(`src/management.rs`):

| Boundary | Fields | Why |
|---|---|---|
| Pause (in place) | `routes`, `defaultTemplate`, `maxStreams`, `commandTimeoutMs`, `connectTimeoutMs`, `shutdownTimeoutMs` | read per delivery or per connection; a new `commandTimeoutMs` closes the idle connection so the next operation reconnects with it |
| Stop (restart) | `user`, `password`, `sslMode`, `allowRemotePlaintext`, `tlsCaPem`, `tlsClientCertificatePem`, `tlsClientPrivateKeyPem` | login and transport |
| Replace (recreate, preserve) | `outputs`, the incoming edges, `deliveryGuarantee`, `onPermanentFailure` | output aliases are bound at construction; the guarantee and policy are declared in the description the graph checks; the cursor owner is unchanged, so receipts remain valid |
| Unsupported | `host`, `port`, `database`, `progressTable`, the graph identity | they locate or own the destination cursor: changing them would move every stream's cursor, so committed operations would no longer be recognised and an unfinished batch could never resume ("missing its first operation"). Remove the component (Erase) and add a new one. |

Apply re-assesses, refuses a stale current configuration or a weaker boundary,
requires a completed pause for in-place updates and a stopped sink for restart
updates. Reconstruction accepts only `Recreate` with `Preserve`.

## Security model

- **SQL injection.** Only `{{param …}}` may emit a value; it renders `$n` and
  binds the value. Plain, raw and helper interpolation, partials, decorators and
  block helpers other than `if`, `unless`, `each`, `with` are rejected at
  construction by the shared validator. Procedure names are literal text.
  The progress-table name is restricted to lowercase identifiers and quoted.
- **Secrets.** `password` and `tlsClientPrivateKeyPem` are top-level secret
  fields (the host accepts only secret references). `configuration()` returns
  them so the host can persist the references; `Debug` output redacts them.
  Templates and routes are not secret and are inspectable.
- **Activation credentials.** The factory declares activation credentials:
  username/password and token kinds replace `user`/`password` for one activation
  (and its reconnects and the following Erase), in memory only. Certificate
  identity providers are not supported by the SDK; client certificates are
  static PEM configuration. (The host does not yet bind an identity resource to
  consumer factories; see Known limitations.)
- **Transport.** `sslMode: require` (the default) verifies the certificate chain
  (system roots plus `tlsCaPem`) and the host name. `sslMode: disable` to a host
  other than `localhost` or a loopback address is refused at construction unless
  `allowRemotePlaintext` is set. There is no `prefer` mode, which would permit a
  silent downgrade.
- **Logging.** Errors carry SQLSTATE-bearing database messages, never parameter
  values; the context and rendered SQL are not logged.

## Observability

`application_name` is `drasi-native-storedproc-postgres`. Retries log a warning
with the delivery ID and the error; Erase logs the number of rows deleted. Host
delivery progress is available through the consumer proxy (`progress()`).
`consumer_progress_reader` is not exposed: the SDK offers it only to producers
(sources and transformers) that publish consumer progress, not to External
consumers; the delivered position of this sink is the host runner's progress
plus its destination cursor rows. Metrics (X-11) are not implemented.

## Known limitations

- The host-sdk refuses an `identity` resource for consumer factories, so
  activation credentials declared by this factory are not yet delivered by the
  graph-factory path; until then use `password` (a secret reference).
- A failed delivery is retried without Stop/Start, through the host as well.
  A delivery the host cancels mid-flight still requires Stop before more
  input, because its batch may not have settled.
- Effects outside the transaction are at-least-once; bind `deliveryId` to
  deduplicate them.
- Lowering `maxStreams` below the number of existing rows blocks only new
  streams.
- Producers with a volatile identity (in-memory queries, volatile graph
  producers) are accepted only with `deliveryGuarantee: atLeastOnce`, at least
  once and without resume across a restart; without opt-in the graph fails
  the edge when it binds it.
- The SDK conformance kit (`SinkConformance`) targets plain `NativeSink`s; this
  consumer's lifecycle is covered by its unit and live tests instead.

## Differences from the legacy reaction

| Legacy `reactions/storedproc-postgres` | Native sink |
|---|---|
| Whole-command retry with fixed backoff; log-and-skip errors | One classified attempt per operation, retried by the graph; permanent failures stall, or with `onPermanentFailure: skip` are recorded in the dead-letter table atomically with the cursor |
| Optional transactional delivery on `public.drasi_delivery_stream_v1` (format 1, one global 4,096-stream limit, no owner) | Always transactional; configurable format-4 table with an owner column, per-owner `maxStreams`, real Erase |
| Template context `query_id`, `query_name`, `operation` (`ADD`/`UPDATE`/`DELETE`), `before`/`after`/`data` | Truthful change context (`REPLACE`, `PATCH`; a patch's `after` holds only the properties it sets; no `data`) |
| Identity providers including certificate mTLS | Activation credentials (password/token) and static PEM client certificates |
| `ConfigValue` per field | Flat configuration with top-level secret fields |
| Any block helper allowed in templates | Only `if`, `unless`, `each`, `with` |

## Delivery failures in the graph

The sink follows the graph's
[delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures) through
the shared protocol: one attempt per `handle`, a transient failure (rolled
back, connection lost, commit outcome unknown, deadlock, lock wait) retried
by the graph in place, and a permanent one (a procedure, commit, template or
cursor rejection) following `onPermanentFailure`. Under `skip`, the skip is
recorded in the destination's dead-letter table in the same transaction that
advances the cursor, so it is exactly once; a cursor rejection still stalls.
