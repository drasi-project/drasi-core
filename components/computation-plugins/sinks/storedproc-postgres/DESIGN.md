# Native PostgreSQL stored-procedure sink: design

`drasi-computation-sink-storedproc-postgres` provides one native
ComputationGraph factory, `drasi.postgres/storedproc` (implementation `1`,
configuration version `1`, native ABI 4.0). This document describes how it
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
 DeliveryRunner ►│ handle(item)  (one operation of one input)                  │
 (host: local    │   loop within reconnectBudgetMs (jittered backoff):         │
  progress,      │     connection usable? else discard + Connection::connect   │
  retention)     │     Connection::ready (prepared cursor statements,          │
                 │                        statement_timeout sync)              │
                 │     ProgressStore::deliver ── one READ COMMITTED transaction│
                 │        read/lock cursor row (owner, stream_key) FOR UPDATE  │
                 │        admit stream (per-owner maxStreams) / roll over      │
                 │        ProcedureEffect: route ─► render ─► CALL $1..$n      │
                 │        SET LOCAL synchronous_commit = on; advance cursor    │
                 │        COMMIT                                               │
                 │     classify: Retry / Reconnect / Permanent                 │
                 └─────────────────────────────────────────────────────────────┘
```

| Piece | Location | Responsibility |
|---|---|---|
| Factory | `src/lib.rs` | Metadata, published configuration details, assess, activation credentials, construction without I/O |
| `PostgresStoredProcConfig`, `Commands` | `src/config.rs` | Fields, defaults, validation, template compilation and routing |
| `PostgresStoredProcSink` | `src/component.rs` | Lifecycle hooks, management apply, `DeliveryHandler`, the procedure effect |
| `Connection` | `src/connection.rs` | One connection attempt (TLS, timeouts), the owned driver task, graceful close and discard |
| Planning | `src/management.rs` | Field-by-field update classification |
| `Backoff` | `src/retry.rs` | Full-jitter exponential backoff (100 ms doubling to 5 s) within a budget |
| `ProgressStore`, `DeliveryOwner`, `ProgressTable`, `DeliveryError` | `postgres-common/src/delivery/progress.rs` | Format-3 cursor protocol, table preparation, Erase, typed error classification |
| `CompiledTemplate`, `build_native_render_input`, `SqlParam` | `postgres-common/src/sql.rs` | Parameter-only rendering, the change context, typed binding |
| Parameter-only rule | `reaction-common/src/sql_template.rs` | The single validator shared by the stored-procedure sinks |

## Input decoding and change projection

The host `DeliveryRunner` splits every input into operations and calls `handle`
once per operation with a `DeliveryItem` (stable identity, position and the
canonical batch digest). The sink checks the `in` port, maps the envelope's
stream to its configured output alias and builds the truthful change context
(`build_native_render_input`): `operation` (`ADD`, `REPLACE`, `PATCH`,
`DELETE`), `output` (`alias`, `component`, `port`), `sequence`, `timestamp`
(only when the envelope has one), `identity` (`namespace`, `id`) and only the
`element`, `before`, `after`, `removedProperties` and `effectiveFrom` that the
record carries. Routing uses `added` for `ADD`, `updated` for `REPLACE` and
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
3. renders and executes the command;
4. sets `synchronous_commit = on` for this transaction and advances the cursor's
   completed count;
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

**Procedures that commit internally downgrade to at-least-once.** A procedure
that issues `COMMIT` (PostgreSQL 11+ procedures may) makes its own effect
durable before the cursor advance. If the attempt then fails, the retry
repeats the effect. The same applies to effects outside PostgreSQL.

### Exact duplicate windows

With the conditions above, there is none: a retry after an unknown outcome
(timeout, lost connection, failed commit reply, process crash after the commit,
a lost local receipt) finds the advanced cursor and completes without an
effect. Without them (internal commits, external effects), every retried
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

### Cursor protocol (format 3)

One row per (owner, producer/consumer stream):

| Column | Meaning |
|---|---|
| `owner` | Lowercase hex SHA-256 of `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id` (64 characters) |
| `stream_key` | The runner's stable stream key (`drasi-delivery-stream-v1-<64 hex>`) |
| `version` | `3` |
| `sequence` | Decimal producer sequence of the latest batch (`[1-9][0-9]{0,19}`) |
| `digest` | The batch's canonical 32-byte content digest |
| `operations` | Operations in that batch (> 0) |
| `completed` | Completed prefix (0..=`operations`) |
| `epoch` | Empty for exactly-once streams; otherwise the at-least-once producer epoch (64 hex) |

Primary key `(owner, stream_key)`. For an operation at index `i` of batch
`(sequence, digest, operations)`:

- same sequence: the digest and operation count must match ("payload conflicts
  with its receipt" otherwise); `i < completed` is an already committed
  operation and completes without an effect; `i == completed` is applied;
  `i > completed` is an operation gap;
- higher sequence: the previous batch must be complete and `i` must be `0`
  (otherwise "unfinished operations or a missing batch prefix"); the row rolls
  over to the new batch;
- lower sequence: the receipt expired; rejected;
- no row: `i` must be `0`; the owner's row count must be below `maxStreams`;
- different non-empty epoch: the row is treated as absent and overwritten
  (`i` must be `0`); an empty epoch on exactly one side is rejected.

Format-2 tables are refused at Start, not migrated.

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


Rejections are permanent (`DeliveryError::Rejected`), never silently skipped.
A damaged row (wrong version, invalid sequence or digest) is rejected.

The `owner` column lets many consumers share one table: each owner's stream
limit counts only its own rows, and Erase deletes only its own rows. The
legacy reaction keeps its own format-1 table (`public.drasi_delivery_stream_v1`)
through `PostgresDeliveryHandler`; the formats never share a table.

## Retry and classification

Classification is by SQLSTATE and error type, never by message text
(`postgres-common::delivery::progress`):

| Failure | Disposition | Action |
|---|---|---|
| `40001` serialization, `40P01` deadlock, `55P03` lock not available, `57014` statement timeout, class `53` (except `53300`) | Retry | rolled back; retry on the same connection |
| Class `08`, `57P01` (`pg_terminate_backend`), `57P02`, `57P03`, `53300`, closed connection, I/O | Reconnect | discard the connection, reconnect, retry |
| Client-side command timeout (`commandTimeoutMs` + 5 s) | Reconnect | the connection state is unknown |
| Commit failure or commit timeout (`CommitUnknown`) | Reconnect | **safe to retry**: the cursor recognises a committed operation |
| Rollback failure | Reconnect (Permanent if the original failure was) | the connection is discarded either way |
| Connection refused, unreachable, TLS I/O, connect timeout | Retry (reconnect) | |
| Authentication (`28xxx`), unknown database (`3D000`), `42501` at connect | Permanent | |
| Any other SQLSTATE (procedure exception, constraint, type, privilege), parameter conversion, render failure, cursor rejection, stream capacity | Permanent | |

Inside one `handle`, Retry and Reconnect dispositions are retried with
full-jitter exponential backoff (100 ms doubling to 5 s) until
`reconnectBudgetMs` is spent; `0` makes one attempt. The backoff wait holds no
state outside the `handle` future, so a pause, an Abort or a deadline that
drops the future ends it at once. When the budget is spent the last error is
returned and `retryable` reports its classification, so the host runner's own
`DeliveryOptions.retry` may call `handle` again (each call has a fresh budget).

A permanent failure fails the input. The sink keeps no latched state: the next
`handle` (the host's replay of the same input, or another input) runs normally,
so a fixed procedure or template, a restored grant or a freed stream slot
recovers delivery without Stop and Start.

A render failure (for example a template that reads an absent field under
strict mode) is permanent (decision D-5): it is never retried by the sink or the
runner and never skipped. Fix the template with an in-place update; the host's
replay then succeeds.

If the host drops an in-flight `handle` (cancellation), the connection is left
marked in flight. The next `handle` discards it (aborting its driver) and
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
| Rows `WHERE owner = <this owner>` in `progressTable` | this sink | deleted |
| The progress table itself | shared (created by the first sink if permitted) | never dropped |
| Rows of other owners | other sinks | untouched |
| Business tables and procedures | the operator | untouched |
| Local delivery progress (`DeliveryRunner`) | the host | the host's own retirement |

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing to open: templates, routes, TLS material and the progress-table name were validated at construction. |
| `start` | Takes activation credentials if present, connects (bounded by `connectTimeoutMs` and `ctx.deadline()`), prepares the progress table (see below) and the cursor statements (each command bounded by `commandTimeoutMs` + 5 s and the deadline). Observes `ctx.cancellation()` throughout. A cancelled or failed start discards the connection, joins its driver and leaves the sink startable; connection failures are retryable, privilege and configuration failures terminal. A second start is refused; a start while a previous driver has not finished closing is refused retryably. |
| `pause` | Host-driven (D-6): the runner calls no `handle` while paused and awaits or drops the in-flight one. The connection driver only answers the sink's own commands and produces no work, so nothing needs gating. In-place updates require a completed pause. |
| `resume` | Clears the pause flag. |
| `stop` | `Drain`: closes the client gracefully; the driver finishes after the server answers any command still running (bounded by `statement_timeout`) and the deadline. `Abort`: drops the socket at once (PostgreSQL rolls back). Waits at most `min(ctx.deadline(), shutdownTimeoutMs)`; on expiry it returns a retryable error and keeps the driver, so a later Stop or Dispose joins it. An earlier connection failure is never reported as a Stop failure. |
| `dispose` | `Erase` first deletes `WHERE owner = <this owner>` over a fresh connection (idempotent: a missing table or no rows succeeds), using the last activation's login if one was supplied; failures are retryable and the host retries with the same intent. Then (both intents) any remaining connection and driver are released. |

The connection driver task is owned by the activation: it is created by start
or by a reconnect inside `handle`, discarded (aborted and joined) on reconnect,
and joined by stop or dispose.

### Progress-table preparation

At start, in one transaction under advisory lock `(1146241363, 2)`:

1. the schema must exist and the role must have `USAGE` on it;
2. if the table exists, the role must have `SELECT`, `INSERT`, `UPDATE` and
   `DELETE` on it, and it must have the format-3 columns;
3. if it does not exist, the role must have `CREATE` on the schema; the sink
   then runs `CREATE TABLE IF NOT EXISTS`.

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
| Pause (in place) | `routes`, `defaultTemplate`, `maxStreams`, `commandTimeoutMs`, `reconnectBudgetMs`, `shutdownTimeoutMs` | read per delivery; a new `commandTimeoutMs` is applied to the live session (`SET statement_timeout`) before its next command |
| Stop (restart) | `host`, `port`, `database`, `user`, `password`, `sslMode`, `allowRemotePlaintext`, `tlsCaPem`, `tlsClientCertificatePem`, `tlsClientPrivateKeyPem`, `connectTimeoutMs` | connection settings |
| Replace (recreate, preserve) | `outputs`, the incoming edges | output aliases are bound at construction; the cursor owner is unchanged, so receipts remain valid |
| Unsupported | `progressTable`, the graph identity | they change the cursor identity: committed operations would no longer be recognised and an unfinished batch could never resume ("missing its first operation"). Remove the component (Erase) and add a new one. |

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
- A procedure that commits internally, or effects outside the transaction,
  are at-least-once.
- Lowering `maxStreams` below the number of existing rows blocks only new
  streams.
- In-memory (volatile) queries are accepted only with
  `deliveryGuarantee: atLeastOnce`, at least once and without resume across a
  restart; without opt-in the edge fails when it is bound.
  Volatile producers other than queries are refused at the first delivery.
- The SDK conformance kit (`SinkConformance`) targets plain `NativeSink`s; this
  consumer's lifecycle is covered by its unit and live tests instead.

## Differences from the legacy reaction

| Legacy `reactions/storedproc-postgres` | Native sink |
|---|---|
| Whole-command retry with fixed backoff; log-and-skip errors | Classified per-operation retry within `reconnectBudgetMs`; permanent failures fail the input (no skip) |
| Optional transactional delivery on `public.drasi_delivery_stream_v1` (format 1, one global 4,096-stream limit, no owner) | Always transactional; configurable format-2 table with an owner column, per-owner `maxStreams`, real Erase |
| Template context `query_id`, `query_name`, `operation` (`ADD`/`UPDATE`/`DELETE`), `before`/`after`/`data` | Truthful change context (`REPLACE`, `PATCH`; a patch's `after` holds only the properties it sets; no `data`) |
| Identity providers including certificate mTLS | Activation credentials (password/token) and static PEM client certificates |
| `ConfigValue` per field | Flat configuration with top-level secret fields |
| Any block helper allowed in templates | Only `if`, `unless`, `each`, `with` |
