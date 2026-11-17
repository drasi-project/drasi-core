# Native MySQL stored-procedure sink: design

`drasi.mysql/storedproc` (crate `drasi-computation-sink-storedproc-mysql`,
implementation `1`, configuration version `1`, native ABI 4.0) calls MySQL
stored procedures for standard graph changes from any source, transformer or
query. It is an **external consumer** of the host `DeliveryRunner`
(`ConsumerMode::External`) with a **destination cursor**: each operation's
procedure effect and its receipt commit in one MySQL transaction.

The runtime lives in `drasi-mysql-common` (`storedproc::{config, delivery,
render, session, runtime}`) so the plugin crate stays a thin factory,
lifecycle and management layer.

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
                               │ CALL proc(?, ?, …)                   (effect)
                               │ UPDATE progress SET completed=completed+1
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
- **Bounded reconnect.** Connecting retries transport failures and server
  overload (1040, 1053, 1203) with full-jitter exponential backoff (100 ms to
  5 s), at most `connectRetryAttempts` extra attempts within
  `reconnectBudgetMs` (and, at Start, the lifecycle deadline and
  cancellation). Authentication and authorization failures fail at once.
- **Timeouts per command.** The protocol layer runs with a one-day I/O bound;
  the sink bounds every statement with `commandTimeoutMs` and every attempt
  with `connectTimeoutMs`, so those settings change in place.

## Input decoding and change projection

The host hands the consumer one `DeliveryItem` per operation of an input. The
sink maps the item's stream to its bound output (`SinkOutputs`), decodes the
operation with `GraphChangeBatch::operation_notification`, and renders the
truthful change context (`operation`, `output`, `sequence`, `timestamp`,
`identity`, and only the `element`, `before`, `after`, `removedProperties` and
`effectiveFrom` the record carries; see [SINKS.md](../../SINKS.md)). `ADD`
selects `added`, `REPLACE` and `PATCH` select `updated`, `DELETE` selects
`deleted`. A scheduling control or recovery frame fails.

Route precedence: the full output alias, then its last dotted segment, then
`defaultTemplate`. No template, or a condition that renders empty SQL, still
advances the cursor (the operation is complete with no effect).

## Delivery profile

**`Consumer/External + destination cursor`.**

| Window | Behaviour |
|---|---|
| Loss | None. The runner records the input before any effect, keeps the envelope until the whole input completes and advances only the completed prefix. A failed operation fails the input; nothing is skipped. |
| Duplicate, procedure keeps the transaction | None. A replay of an operation inside the cursor's completed prefix commits nothing and succeeds. This covers a crash or cancellation after `COMMIT` but before the host recorded completion, an unknown commit outcome, and a host that lost its local ledger. |
| Duplicate, procedure ends the transaction | At-least-once. A procedure that runs `COMMIT`/`ROLLBACK`, causes an implicit commit (DDL, `LOCK TABLES`, …) or writes non-transactional tables (MyISAM) commits its effect before the cursor. A failure between that commit and the cursor update repeats the effect on retry. The sink detects an ended transaction (the server status after the `CALL`) and logs a warning per operation. |
| External side effects | At-least-once (anything the procedure does outside MySQL is not rolled back). |

Exactly-once conditions: InnoDB (or another transactional engine) for every
table the procedure writes, no transaction control or implicit commit inside
the procedure, and the cursor table in the same server.

Completion: `handle` returns `Ok` only after `COMMIT` succeeded.

## Destination cursor protocol (format version 3)

Shared by the three stored-procedure sinks. Table `progressTable` (default
`drasi_delivery_progress`) in the configured database, InnoDB:

| Column | Type | Meaning |
|---|---|---|
| `owner` | `CHAR(64)` ascii | lowercase hex SHA-256 of `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id` |
| `stream_key` | `VARCHAR(128)` ascii | the host `DeliveryPosition` stream (producer, consumer scope and port) |
| `version` | `SMALLINT` | `3` |
| `sequence` | `VARCHAR(20)` | decimal producer sequence of the current batch |
| `digest` | `BINARY(32)` | canonical batch digest |
| `operations` | `BIGINT` | operations in the current batch |
| `completed` | `BIGINT` | completed operation prefix |
| `epoch` | `VARCHAR(64)` ascii | empty for exactly-once streams; otherwise the at-least-once producer epoch |

`PRIMARY KEY (owner, stream_key)`. One row per stream replaces an ever-growing
receipt table.

Per operation, in one transaction:

1. Validate the position; lock the stream row with `SELECT … FOR UPDATE`.
2. Epoch: a different non-empty epoch on both sides treats the row as absent
   (operation `0` required; the row is overwritten with the new batch and
   epoch); an empty epoch on exactly one side is a permanent receipt conflict.
   No row: take the per-owner admission lock (`GET_LOCK`, released after
   `COMMIT`/`ROLLBACK`), re-read, require operation `0`, count the owner's rows
   against `maxStreams`, insert the row with `completed = 0`.
3. Older sequence than the row: *receipt expired* (permanent).
4. Same sequence: digest and operation count must match (*payload conflict*,
   permanent). An operation below `completed` is already committed: return
   success without effect.
5. Newer sequence: the previous batch must be complete and the operation must
   be `0`; replace the row's batch.
6. Require `operation == completed` (*gap*, permanent).
7. Run the effect, then `UPDATE … SET completed = completed + 1 WHERE …
   AND completed = <expected>` and check one row changed.
8. `COMMIT`.

Format 2 tables (without `epoch`) are refused at Start, not migrated.

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


Rows are validated on every read (version 2, canonical decimal sequence,
32-byte digest, `0 <= completed <= operations`).

## Retry and classification

`DeliveryHandler::retryable` classifies by MySQL error code and typed error,
never by message text:

| Failure | Retryable | Notes |
|---|---|---|
| Deadlock (1213), lock wait timeout (1205) | yes | after `ROLLBACK` |
| Session lost before `COMMIT` | yes | only after the session was retired (proof complete) |
| `COMMIT` outcome unknown | yes | after retirement; the cursor makes the retry idempotent |
| Admission lock busy | yes | |
| `commandTimeoutMs` expired | no | session KILLed, transaction rolled back; the host retries the input under its own policy |
| Template render failure (D-5) | no | permanent for the change |
| Procedure error (`SIGNAL`, constraint, missing procedure, privilege) | no | rolled back |
| Receipt expired / conflict / gap, stream capacity | no | operator action needed |
| Retired session not yet proven gone | no | the next `handle` retries the cleanup first |

An idle session that the server closed (restart, `wait_timeout`, `KILL`) is
detected at `START TRANSACTION`; the sink retires it, reconnects and runs the
operation once more inside the same `handle`, because nothing executed.

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

Owned: this owner's rows in the cursor table, the activation session and any
retired session awaiting proof. Borrowed: the procedures, the business tables
and the cursor table itself (created when absent, never dropped).

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing: configuration, templates and routes were validated and compiled at construction. |
| `start` | Requires completed cleanup. Opens the session (bounded retries) and creates/verifies the cursor table, within `ctx.deadline()` and `ctx.cancellation()`. A failed or cancelled start leaves nothing open and the object startable. Activation credentials, when bound, are used instead of literals. |
| `pause` / `resume` | Host-driven (D-6): the runner calls no `handle` while paused and settles the in-flight one first. Marks the in-place update boundary. The only in-handle wait is a reconnect bounded by `reconnectBudgetMs`. |
| `stop` | `Drain`: the host awaited the in-flight operation; the idle session closes. `Abort`: the host dropped the in-flight `handle`; the session left mid-command is KILLed (rolling its transaction back). Bounded by `ctx.deadline()` (or `shutdownTimeoutMs`); on expiry Stop returns a retryable error and keeps the session owned. An earlier delivery failure is never a Stop failure. |
| `dispose` | `Preserve`: retire/close sessions. `Erase`: first delete only this owner's cursor rows (idempotent; an absent table is fine), then close. |
| `handle` | One operation per call, as above. |

## Management

`Factory::assess` and `NativeManagement` classify every field (see
[CONFIGURATION.md](CONFIGURATION.md#updates)). In-place updates require a
completed pause; restart updates require a stopped sink; Replace is never an
instance update. `progressTable` is Unsupported: it is the identity of the
receipts, so moving it would drop the receipt of an in-flight operation and
orphan this owner's rows. Remove the component (Erase) and add it again.

## Security model

- `password` is the only secret field and is never printed by `Debug`;
  templates and routes are inspectable.
- Values reach SQL only as bound parameters; procedure names are literal text.
- `ssl: true` requires TLS and verifies the certificate chain and host name;
  `tlsCaPem` adds a trusted CA. Without TLS, traffic is plaintext (a warning
  is logged).
- KILL is issued only after the per-connection nonce proves ownership; a
  reused connection ID is never killed.
- The cursor table name is a validated identifier; all literals in cursor SQL
  are validated hex or decimal.

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
| Command retry with backoff | Host runner retries classified errors; the cursor makes retries idempotent |
| Log and skip failed commands | Fail the operation; the host retries the input |
| `query_id`, `query_name`, `data` context | Truthful change context (`output.alias`, `operation`, `before`/`after`, …) |
| `{{json …}}` inline | Rejected; bind with `{{param …}}` |
| Nested `ConfigValue` settings | Flat camelCase fields, `password` secret |
| Unbounded input, no receipts | Per-operation transactions with a destination cursor |
