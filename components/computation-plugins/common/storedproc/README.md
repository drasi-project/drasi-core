# drasi-computation-storedproc-common

The destination-cursor delivery protocol of the native stored-procedure sinks
(`drasi.postgres/storedproc`, `drasi.mysql/storedproc`,
`drasi.mssql/storedproc`), implemented once. Each database supplies only a
`driver::Session` (its SQL and error classification); the decisions, the
failure rules and the atomic skip are shared, so the three sinks behave the
same way.

## Cursor (format 4)

One row per `(owner, stream_key)` in the progress table:

| Column | Meaning |
|---|---|
| `owner` | Lowercase hex SHA-256 of `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id`. Several sinks share a table; each owns, counts (`maxStreams`) and erases only its rows. |
| `stream_key` | The host's ordered delivery stream (`DeliveryPosition::stream`). A durable query's key excludes its output generation; a volatile producer's key excludes its incarnation. |
| `version` | `4`. Earlier formats are refused, not migrated. |
| `sequence`, `digest`, `operations` | The stream's current batch: producer sequence, canonical content digest, operation count. |
| `completed` | The completed operation prefix of that batch. |
| `epoch` | Empty for exactly-once streams; otherwise the at-least-once producer epoch. |
| `generation` | The durable query producer's output generation (0 when it has none). |
| `pending` | The operation whose effect is running in an open delivery transaction, or `-1`. It is written in the effect's transaction and cleared by the cursor advance, so it is durable only if the effect escaped that transaction. |

The dead-letter table `<progressTable>_dead_letter` (same schema) holds one
row per skipped operation: `owner`, `delivery_id` (primary key with `owner`),
`stream_key`, `sequence`, `operation`, `epoch`, `generation`, `error` (at
most 4,000 characters) and `recorded_at`.

`progressTable` is `table` or `schema.table`; each part is a lowercase
identifier (`[a-z_][a-z0-9_]*`), the schema at most 63 bytes and the table at
most 51 so that `<table>_dead_letter` fits every backend. An unqualified
table lives in the backend's default schema (PostgreSQL `public`, MySQL the
configured `database`, SQL Server `dbo`).

## One operation

One transaction per operation:

1. Read and lock the stream's row; if there is none, take the owner's
   first-stream admission lock and read again.
2. Decide (`cursor::decide`):
   - no row: operation 0 inserts the row (counted against `maxStreams`);
     another operation is refused (`missing its first operation`);
   - a different epoch: an at-least-once producer's new run; operation 0
     overwrites the row. Exactly-once items never match at-least-once rows,
     nor the reverse;
   - a newer generation: a reset or recreated query; operation 0 overwrites
     the row (no new row). An older generation is refused (`superseded`);
   - an older sequence: refused (`receipt expired`); the same sequence with a
     different digest or operation count: refused (`conflicts`);
   - the same batch: an operation inside the completed prefix is a
     duplicate (nothing runs); the next one applies; any other is a gap;
   - a newer batch: operation 0 after a complete batch overwrites the row;
     anything else is refused (`unfinished operations`).
3. Mark the operation pending, run the rendered command, advance
   `completed` (clearing `pending`) and commit. The effect and the cursor
   commit together.

## Failures

The driver makes **one attempt per call and never sleeps**: the graph retries
a transient failure with its bounded backoff. The only immediate retry is a
reconnect when a reused idle session turned out to be dead before its
transaction began.

| Failure | Class |
|---|---|
| Deadlock, serialization or lock-wait failure (PostgreSQL 40001/40P01/55P03/57014, MySQL 1213/1205, SQL Server 1205/1222/...) | transient (rolled back; the session is kept) |
| A lost or unusable connection, a statement timeout | transient (the session is discarded; MySQL KILLs it and proves it gone first) |
| COMMIT's reply lost | transient, "outcome unknown"; the session is discarded and the next attempt resolves it from the cursor (a committed operation is then a duplicate) |
| COMMIT definitely refused (a deferred constraint, a serialization failure at commit) | classified like the statement error: permanent (or transient) — never "outcome unknown" |
| The procedure or its template rejected the operation | permanent |
| The command ended the delivery transaction (COMMIT, ROLLBACK, an implicit commit) | permanent; the cursor is never advanced outside the transaction. If the effect escaped (committed with the `pending` mark), later attempts refuse to run it again |
| A cursor rejection (gap, conflict, expired, superseded, capacity, a mismatched guarantee, invalid progress) | permanent, never skipped |

Detection of transaction control is per backend: PostgreSQL refuses it inside
the delivery transaction (`2D000`) and checks the transaction ID after the
command; MySQL reads the server's in-transaction status after every
statement; SQL Server checks `@@TRANCOUNT` and error 266.

## `onPermanentFailure`

`stall` (default): the permanent failure is returned and the graph stalls the
node. `skip`: a failure caused by the operation's content (the procedure, its
commit, its template, transaction control) is settled in a second
transaction that records the operation and its error in the dead-letter table
and advances the cursor past it, atomically; the call reports
`DeliveryFailure::skipped`. The host `DeliveryRunner` then records the
operation as completed, delivers the input's other operations, and reports the
input as permanently failed, so the graph acknowledges it and counts the skip.
A replay finds the operation settled, so the skip happens exactly once. A
cursor rejection cannot be settled; the host then fails the node.

## Testing

`conformance::run_protocol_table` drives the driver through every rule above
against a `conformance::Backend`. It runs against the in-memory session in this
crate's unit tests and against PostgreSQL and MySQL in their live suites
(`cargo test -p drasi-computation-sink-storedproc-{postgres,mysql} --test live
-- --ignored`). `conformance::ReplyDropper` is a TCP proxy that loses the reply
to one COMMIT, for live lost-reply tests.
