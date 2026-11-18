# SQL Server stored-procedure sink: design

`drasi.mssql/storedproc` (crate `drasi-computation-sink-storedproc-mssql`,
computation ABI 5.0.0) calls a SQL Server stored procedure, or any
parameterized T-SQL batch, for each graph change it receives.

> **Live qualification is deferred (CW-78).** Everything below is verified by
> unit tests, construction tests and an in-memory session that models the
> cursor protocol. No SQL Server was used. The live scenarios in
> `components/host-sdk/tests/native_mssql_components.rs` compile but are
> ignored until an authorized environment exists.

## Architecture

| Part | File | Role |
|---|---|---|
| Configuration | `src/config.rs` | Flat camelCase settings, published defaults, validation, template rule |
| Factory | `src/lib.rs` | Metadata, `ConsumerMode::External`, `assess`, activation credentials |
| Component | `src/component.rs` | Lifecycle hooks, `DeliveryHandler`, `NativeManagement`, rendering |
| Protocol | `src/delivery.rs` | Cursor decisions, transaction per operation, retry and classification |
| Session | `src/session.rs` | The protocol's statements over one Tiberius connection |
| Planning | `src/management.rs` | Update boundaries for every field |
| Shared | `mssql-common::sql` | `CommandTemplate` (compiled once), parameter binding, `connect` |

The sink is an **external consumer**. The host `DeliveryRunner` records each
input before any effect and calls `handle` once per operation (one change).
The sink owns **one persistent connection** for its activation. It opens the
connection at Start and reopens it on demand after a failure. It starts no
workers and holds no state outside the running call, apart from that
connection.

## Input decoding and change projection

Each operation is turned into one change notification with
`GraphChangeBatch::operation_notification`, for the output bound to the
envelope's stream. The output alias selects the template: the full alias,
then the alias's last dotted segment, then `defaultTemplate`. The route is
`added` (ADD), `updated` (REPLACE and PATCH) or `deleted` (DELETE).

The template context is the truthful change object: `operation`, `output`
(`alias`, `component`, `port`), `sequence`, `timestamp` (only when the
envelope carries one), `identity`, and only the `element`, `before`,
`after`, `removedProperties` and `effectiveFrom` that the record carries. It
also carries `deliveryId`, the host's stable operation identity (see
[Idempotency key](#idempotency-key)). A PATCH's `after` holds only the
properties it sets, and an unknown `before` is absent rather than null.

## Templates and the injection rule

Templates are **parameter-only** (SMS-01). At construction every template is
checked with `drasi_reaction_common::sql_template::validate_parameter_only`
and compiled once into a `CommandTemplate`. The following are rejected:

* plain interpolation `{{x}}`, anywhere in the SQL text;
* raw output `{{{x}}}` and `{{& x}}`;
* inline value helpers such as `{{json x}}` (the legacy reaction allowed
  `EXEC p '{{json after}}'`, which wrote row data into the SQL text);
* partials, decorators and block helpers other than `if`, `unless`, `each`
  and `with`;
* `{{param}}` with no value.

`{{param path}}` binds the value as `@P1`, `@P2`, … and `{{param (json
path)}}` binds a JSON document. Procedure names and other structure are
literal text. Rendering is strict: a field missing from the change fails the
render. A render failure is a permanent failure of that operation (D-5).

One rendered command may bind at most **2 098** values. SQL Server accepts
2 100 parameters per request and `sp_executesql` uses two. A command over
the limit fails before any effect with a permanent `ParameterLimitExceeded`
error.

## Delivery profile: Consumer/External with a destination cursor

Each operation runs in its own SQL Server transaction (`XACT_ABORT ON`)
through the protocol shared by the three stored-procedure sinks
(`drasi-computation-storedproc-common`,
[README](../../common/storedproc/README.md)); `session.rs` implements its
SQL Server `Session`:

1. `BEGIN TRANSACTION`.
2. Read this sink's cursor row for the stream `WITH (UPDLOCK, HOLDLOCK)`.
3. If there is no row, take the per-owner admission lock (`sp_getapplock`,
   transaction-owned) and read again. This serialises only a stream's first
   admission; existing streams use their own row lock.
4. Decide (the shared decision table). A duplicate rolls back and returns
   success.
5. For a first batch, check the per-owner stream count against `maxStreams`
   and insert the row; for a new batch, a newer query generation or a new
   producer epoch, replace the batch fields and reset `completed` to 0.
6. Mark the operation `pending`, render the command and run it, draining
   every result set.
7. Check that the transaction depth is still 1, then increment `completed`
   (clearing `pending`). Exactly one row must change.
8. `COMMIT TRANSACTION`.

### Exactly-once conditions

An operation's effect and its cursor advance commit atomically, so an
operation takes effect **exactly once** when the procedure keeps its work in
the sink's transaction and causes no effects outside SQL Server's transaction
(linked servers without distributed transactions, `xp_cmdshell`, Service
Broker sends to other instances, external REST calls, and similar).

**Procedures that control the transaction are refused, never repeated.** A
procedure that commits, rolls back or leaves a transaction open changes the
transaction depth (error 266 or `@@TRANCOUNT <> 1` at step 7): the operation
fails permanently and the cursor is not advanced. With `XACT_ABORT ON` its
own `COMMIT` may already have committed the effect together with the
`pending` mark; every later attempt then finds the mark and refuses to run the
procedure again, so a replay cannot duplicate the effect.

### Duplicate and loss windows

* **No loss.** The host records the input before any effect and advances only
  the completed prefix. A failed operation is retried, never skipped, unless
  `onPermanentFailure: skip` settles it (recorded in the dead-letter table).
* **No duplicate** for a transactional procedure. A crash or connection loss
  before `COMMIT` rolls the effect and the cursor back together. A lost
  `COMMIT` reply leaves the outcome unknown; the next attempt reads the cursor
  and finds the operation committed.
* **Duplicates** only for effects outside the transaction; bind `deliveryId`
  to deduplicate them.
* A stream whose cursor row is erased (Erase, manual deletion) starts again:
  an in-flight batch then fails with a permanent error rather than repeating.

### Cursor table (shared format 4)

The table is identical across the three stored-procedure sinks:

| Column | Type | Meaning |
|---|---|---|
| `owner` | `NVARCHAR(64)` | Lowercase hex SHA-256 of `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id` |
| `stream_key` | `NVARCHAR(128)` | The host's ordered delivery stream (`drasi-delivery-stream-v1-…`) |
| `version` | `SMALLINT` | Always 4 |
| `sequence` | `NVARCHAR(20)` | Decimal producer sequence of the batch in progress |
| `digest` | `VARBINARY(32)` | Content digest of that batch |
| `operations` | `BIGINT` | Operations in that batch |
| `completed` | `BIGINT` | Completed operation prefix |
| `epoch` | `NVARCHAR(64)` | Empty for exactly-once streams; otherwise the at-least-once producer epoch |
| `generation` | `BIGINT` | The durable query's output generation (0 when none) |
| `pending` | `BIGINT` | The operation whose effect is running, or -1; durable only if the effect escaped the transaction |

The primary key is `(owner, stream_key)`. Text columns are `NVARCHAR` so that
the bound string parameters seek the key without implicit conversion. The
table holds one row per stream, not one per operation; a query reset rolls
its row over instead of adding one. The dead-letter table
`<progressTable>_dead_letter` records skipped operations with their error.

`owner` makes rows belong to exactly one sink in one graph, so several sinks
(and the other dialects' sinks, in their own databases) can share a table.
Each owner may keep at most `maxStreams` streams; other owners do not count.

At Start the sink creates the tables if they are missing. Creation runs in a
transaction under an exclusive application lock, so concurrent starts do not
race. Start then checks the login's SELECT, INSERT, UPDATE and DELETE
permissions and the column names of both tables, and fails with a message that
names the missing grant. A least-privilege login can use tables that an
administrator created in advance (see `CONFIGURATION.md`). Earlier formats are
refused, not migrated.

## Batching

The host's batching is the only batching. There is no limit on the number of
changes in an input and no input is rejected for its size (SMS-02; the old
100 000-command cap is gone). Rendering happens per operation, so memory is
bounded by one rendered command. `commandTimeoutMs` applies to each server
round trip (Tiberius's per-response timer), never to a whole input.

## Retry and classification

Each `handle` is one attempt; the graph retries a transient failure with its
bounded backoff, so there is no in-plugin retry budget. The only immediate
retry is a reconnect when the idle connection turned out to be dead before its
transaction began. Failures are classified by type and SQL Server error
number, never by message text (`delivery::classify`):

| Class | Errors |
|---|---|
| Transient, keep the connection | 1205 deadlock victim, 1222 lock timeout, 3960/3961 snapshot conflicts, 41301/41302/41305/41325 in-memory OLTP conflicts, 59101 admission lock not granted |
| Transient, reconnect | I/O and protocol errors, connect timeout, a lost `COMMIT` reply (outcome unknown, resolved from the cursor by the next attempt), a failed rollback, Azure SQL transient errors 233, 64, 4221, 10053, 10054, 10060, 10928, 10929, 40197, 40501, 40613, 49918–49920 |
| Definite commit error | a server error answering `COMMIT` on a live connection is classified by its number (a definite rollback), never as "outcome unknown" |
| Permanent | every other server error (procedure `THROW`/`RAISERROR`, constraint violations, permissions, login failure, syntax), TLS errors, render failures, the parameter limit, transaction control (266 or a changed depth) |
| Permanent, never skipped | every cursor protocol violation, stream capacity |

A permanent content failure (procedure, commit, template, transaction
control) follows `onPermanentFailure`: `stall` (default) or `skip`, which
records it in the dead-letter table in the same transaction that advances the
cursor.

**Tiberius cannot send a TDS ATTENTION**, so a running statement cannot be
cancelled in place. When the host drops a call (Abort, a deadline) or a round
trip exceeds `commandTimeoutMs`, the connection is dropped. SQL Server rolls
the transaction back when it notices the closed session, which may be after
the procedure's current wait finishes. The next operation reconnects. No
"interrupted" flag is latched (SP-03).

## Idempotency key

The template context carries `deliveryId`, the host's `DeliveryItem::id`. It
is derived from the consumer scope, port, stream, producer and the change's
ordinal, and excludes retry and configuration identity, so a procedure can
record it for its own deduplication (for example when it cannot stay inside
the sink's transaction).

## Owned and borrowed destination state

The sink owns only its rows (`owner`) in the progress and dead-letter tables.
It never drops or truncates the tables, and it never touches business tables
except through the configured procedures. `dispose` with
`StateIntent::Erase` deletes this owner's rows from both; a missing table
counts as success, and a second Erase deletes nothing. The factory declares `erases_owned_state`.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `create` | Parses and validates the configuration, compiles templates, resolves routes against the bound inputs, derives `owner` from the graph scope. No I/O. |
| `initialize` | Nothing (construction did the checks). |
| `start` | Resolves authentication (activation credentials, never falling back to literals when an identity is bound), connects (one attempt bounded by `connectTimeoutMs`), creates or checks the progress and dead-letter tables, and keeps the connection. Bounded by `ctx.cancellation()` and `ctx.deadline()`. A cancelled or failed start leaves nothing open; transient failures are reported as retryable. |
| `pause` / `resume` | Host-driven (D-6): the runner calls no `handle` while paused and settles the in-flight one. The idle connection stays open. |
| `handle` | One operation, as described above. |
| `stop` | `Drain` closes the idle connection gracefully within the deadline (at most `shutdownTimeoutMs`); `Abort` drops it. The host never runs `handle` during Stop, so there is no in-flight statement and nothing is retained. An earlier delivery failure is never reported here. |
| `dispose` | Drops any connection. `Erase` deletes this owner's rows on a short-lived connection, using the last activation's credentials (kept in memory only) or the literal credentials. |

## Authentication

* **SQL authentication** with literal `user` and `password` (secret).
* **Activation credentials** (`supports_activation_credentials`): a host
  identity binding supplies credentials at each start, overriding literals
  for that activation only. `UsernamePassword` is SQL authentication.
  `Token` is a **Microsoft Entra ID access token** sent with Tiberius
  `AuthMethod::AADToken` (the username is ignored). This replaces the old
  behaviour that turned a token into a SQL password.
* Certificates are rejected by the host.

> **Host gap.** The Host SDK does not yet bind identity resources to
> external-consumer factories (`"identity"` requires a non-consumer factory
> in `host-sdk/src/computation/factory.rs`). Until it does, only literal SQL
> authentication works at runtime; the sink side of activation credentials and
> Entra ID tokens is implemented and unit tested.

## Security

* TLS is required by default (`encryption: required`) and verifies the server
  certificate against the system roots plus `trustedCaPem`.
  `trustServerCertificate`, `encryption: loginOnly` and `encryption:
  disabled` are explicit opt-ins and log a warning at start.
* The password is the only secret field. It is redacted from `Debug`;
  `configuration()` returns it because it is the persistence hook.
* Templates are parameter-only (see above). The progress table name is
  validated to lowercase identifiers and is bracket-quoted.
* Errors and logs never include credentials or parameter values.

## Delivery guarantee and producer epochs

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


## Known limitations

* In-memory (volatile) queries are accepted only with
  `deliveryGuarantee: atLeastOnce` (at least once, no resume across a
  restart); without opt-in the edge fails when it is bound.
* Live SQL Server qualification is deferred (CW-78).
* Identity bindings for external consumers need a Host change (see above).
* A statement cannot be cancelled in place (no ATTENTION); see above.
* No metrics yet (X-11).

## Differences from the legacy reaction

| Legacy (`reactions/storedproc-mssql`) | Native |
|---|---|
| Inline `{{json}}` and `{{x}}` in SQL text allowed | Rejected: parameter-only templates (intentional tightening) |
| Command retry with backoff, no transaction | Per-operation transaction with cursor; one classified attempt, retried by the graph |
| Shared client | One persistent connection per activation, reconnected on demand |
| `trust_cert()` always | Certificate verification by default |
| Identity providers (password; token as SQL password) | Activation credentials; token is an Entra ID access token |
| Per-command timeout | Per-round-trip timeout |
| Log-and-continue on errors | `onPermanentFailure: stall` (default) or `skip`, which records the operation in the dead-letter table atomically with the cursor; nothing is silently dropped |
| Context `query_id`, `query_name`, `data` | Truthful change context (see `CONFIGURATION.md`) |

The legacy reaction is unchanged and still accepts inline `{{json}}`.

## Delivery failures in the graph

The sink follows the graph's
[delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures) through
the shared protocol: one attempt per `handle`, a transient failure (rolled
back, connection lost, commit outcome unknown, deadlock, lock timeout)
retried by the graph in place, and a permanent one (a procedure, commit,
template or cursor rejection, transaction control) following
`onPermanentFailure`. Under `skip` the skip is recorded in the dead-letter
table in the same transaction that advances the cursor, so it is exactly
once; a cursor rejection still stalls.
