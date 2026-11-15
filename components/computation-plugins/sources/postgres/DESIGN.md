# Native PostgreSQL source: design

`drasi-computation-source-postgres` streams a PostgreSQL logical-replication
publication (pgoutput, protocol 1) into a ComputationGraph. It is a native
plugin (ABI 4.0) with four factories, each at implementation and
configuration version `1`. Connection handling, SCRAM, TLS, pgoutput
decoding, key mapping, bounded transaction assembly, slot management and the
source binding live in `drasi-postgres-common`, shared with the PostgreSQL
bootstrap providers. Neither depends on `drasi-lib`.

Field reference, prerequisites and runbook: [CONFIGURATION.md](CONFIGURATION.md).

## Modes

| Factory | Emits | Progress | Guarantee |
|---|---|---|---|
| `drasi/postgres-transactions` | One `SourceTransactionCodec` envelope per committed transaction | One atomic persistent query (`source_progress`) | Whole commits; a rejected or incomplete commit never publishes a prefix |
| `drasi.postgres/changes` | One `GraphChangeCodec` envelope per row change | One query, memory or persistent (`source_progress`) | Intermediate changes visible; partial transactions resume at the unhandled suffix |
| `drasi.postgres/shared-changes` | Row changes | A fixed consumer set (`consumer_progress`) | One slot feeds many queries; each filters covered input independently |
| `drasi.postgres/shared-transactions` | Whole commits | A fixed set of atomic persistent queries | As above, per commit |

Factory identity selects the contract; no option changes another factory's
schema, cursor format or guarantee.

### Cursors and feedback

Feedback to the server follows **committed host progress**, never plugin-queue
acceptance or the read position. The worker sends standby status every
`feedbackIntervalMs`, on server request, and whenever progress changes.

- **Transactions:** the cursor is the commit LSN plus the source binding.
  Logical sequence is the commit LSN.
- **Rows:** each row carries its commit LSN, offset and count within the
  commit, the previous complete commit and a stable logical sequence. Replay
  after a partial transaction emits only the unhandled suffix; feedback never
  crosses a transaction until all its rows are handled.
- **Shared rows:** replay starts at the members' minimum *WAL position*
  (commit LSN, then offset), not their logical counters. Feedback is the
  minimum complete-commit floor of every member. Snapshot boundaries between
  visible commits carry into later row cursors. One input-position policy lets
  each query skip covered replay. Emitted counters stay above every member's
  counters and this source's prior allocations, across reconnects and reader
  replacement.
- **Shared transactions:** the position has `version`, `mode`
  (`whole_commit` or `configured_snapshot`), a domain-separated `binding`,
  `commit_lsn` and a logical `sequence` (zero only for a snapshot). Replay
  and feedback use the minimum physical boundary of every member.

A fresh group starts at its configured or slot boundary; a group mixing
handled and missing cursors fails rather than starting the missing member at
the read position. Failed, reset, removed or rebound members fence progress.
Membership is immutable per binding: late enrollment uses Core's ordered
stop, drain, group replacement and restart.

### Binding

The binding is a SHA-256 over the facts a cursor depends on: the server's
system identifier and database, the slot, the publication and its operations,
the selected tables with their key columns, key column types and replica
identity, row filters, the source and the stream (format version 2). Every
committed cursor carries it; a cursor whose binding no longer matches is a
`SchemaChanged` failure.

Decision D-7 narrowed the binding to key columns: adding, dropping or retyping
a non-key column does not invalidate cursors, and the decoder follows
pgoutput's relation messages. Key, replica-identity, publication and slot
changes do invalidate them. A table joining an all-tables publication
(`tables` empty) is a `SchemaChanged` failure at runtime; with explicit
`tables`, other tables are ignored.

### Element identity

Snapshot and live rows share `drasi-postgres-common`'s key encoding
(`pg:v1:[namespace, table, sorted key pairs]`) and type mapping. Labels are
`table` in `public`, otherwise `schema.table`. Key-changing updates emit a
delete and an insert; unchanged TOAST values are omitted (or taken from a
`REPLICA IDENTITY FULL` old tuple for key changes).

## Slots

Managed ownership derives a prefix from SHA-256 over the graph and component
identifiers (`drasi_` + 24 hex digits). The live slot is the prefix plus `_`
and 32 more hex digits; coordinated providers create `prefix_<uuid>` slots.
Every slot this source owns therefore matches `^prefix_[0-9a-f]{32}$` in its
database, and nothing else does. That makes erasure exact without persisted
markers, and lets a later process, or the provider in another library,
derive the same names.

- A managed live slot is created only when no checkpoint exists and the slot
  is absent (by catalog lookup, not error text; a concurrent create of the
  same derived name, SQLSTATE 42710, is the same slot). A missing slot with a
  checkpoint is `PositionUnavailable`.
- A coordinated provider records its slot in the committed cursor; the source
  streams only from a slot whose name it owns.
- External slots are never created or dropped (decision PG-02 replaced
  text-matched adoption).
- Erasing disposal lists slots with the prefix, refuses if any is active
  (`SlotActive`), and drops each with `DROP_REPLICATION_SLOT`, treating
  SQLSTATE 42704 (already gone) as success. It is idempotent and safe to retry.

## Worker

One owned worker per activation reads CopyData, assembles complete commits
with bounded `SourceTransactionBuilder`s, and pushes envelopes into a
capacity-one channel that `next` drains. Under backpressure it keeps sending
feedback and enforces the transaction deadline.

### Reconnection

A transport failure (I/O errors, timeouts, server shutdown and connection
SQLSTATEs 57P0x, 08xxx, 53xxx, and 55006 for a slot held by a terminating
walsender) reconnects in **every** mode with full-jitter backoff (250 ms
doubling to 30 s) until `reconnectBudgetMs` is spent. A reconnect revalidates
retention and the binding, then resumes at the committed cursor:

- transactions resume at the committed commit LSN;
- rows rewind to the owner's handled row cursor;
- shared modes resume at the members' minimum position and raise allocation
  floors to the observed maximum.

Output after the cursor is replayed, exactly as after a restart; consumers
deduplicate by committed position (single owner) or input-position policy
(shared). Permanent failures (`PostgresCdcError`, decoding errors, fenced or
reset owners, exceeded limits) never reconnect.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Validates the resolved configuration. No I/O. |
| `start` | Takes activation credentials if supplied, connects, ensures the slot, validates retention and the binding, checks the cursor is still retained, and starts streaming, all within the smaller of `ioTimeoutMs` and the host deadline, and abandoned on cancellation. After a failed start the source requires a stop before it starts again. |
| `pause` | Asks the worker to stop at the next transaction boundary: an open commit is assembled first and held, nothing more is read or emitted, and feedback continues so `wal_sender_timeout` does not disconnect. Returns once the worker acknowledges, within the deadline; on timeout or cancellation the pause is withdrawn. A reconnecting worker counts as paused. |
| `resume` | Lifts the gate; held output is emitted. |
| `stop` | `Drain`: final feedback at the committed cursor, then CopyDone and Terminate. `Abort`: the worker is cancelled. Both honour the host deadline and keep the worker owned on timeout, so a later stop can finish cleanup. A replication failure already reported through `next` does not make stop fail. |
| `dispose` | Requires a stopped source. `Preserve` does nothing. `Erase` drops this source's managed slots (see above). |

## Management

`Factory::assess` and `NativeManagement` classify every field (table in
[CONFIGURATION.md](CONFIGURATION.md#settings)). InPlace (`reconnectBudgetMs`)
applies to the running worker after a completed pause. Restart fields apply
while stopped. Fields the cursor is bound to are unsupported: a replacement
would need every downstream query rebuilt from a new snapshot, which this
source cannot coordinate, so it does not claim a Recreate plan. A `startLsn`
change is a Restart only while the live object sees no committed cursor (the
assessment's fingerprint names that fact, and apply re-assesses).

## Security

- TLS through `native-tls` always verifies the certificate chain and host
  name; `tlsCaPem` adds trust roots. `prefer` falls back to plaintext only
  when the server answers the SSLRequest with `N`, never after a failed
  handshake. SCRAM-SHA-256, MD5 and cleartext password authentication are
  supported; use TLS with the latter two.
- `password` is a top-level secret field; `Debug` output redacts it.
  Activation credentials replace `user` and `password` per activation.
- Catalog queries quote every literal and identifier; slot names are
  restricted to lowercase letters, digits and underscores.

## Schema discovery

`source_schema_reader` opens a short-lived ordinary connection and describes
the selected publication tables: labels as emitted, property types matching
the decoder's values (integers, floats and numerics as numbers, booleans,
timestamps; dates, times, JSON, UUID and bytea as strings; arrays untyped).

## Unsupported

- TRUNCATE (refused at start; decision D-9). A typed reset that forces a
  rebuild is the planned follow-up.
- Tables without a usable key (decision D-10).
- PostgreSQL 12 and older; coordinated snapshots need 15.
- Two-phase and streamed in-progress transactions, failover slots and
  timeline changes.
- Automatic recovery from a lost slot or a schema change: both need removal
  with erase and re-addition.

## Differences from the legacy source

| Legacy | Native |
|---|---|
| Per-field `ConfigValue` references | One secret field (`password`) plus activation credentials |
| Create-or-reuse slot by name, default `drasi_slot` | Explicit managed or external ownership; managed names are derived |
| Reconnect on any error, fixed delay | Transport failures only, jittered backoff within a budget, every mode |
| TRUNCATE logged and ignored | Refused at start |
| DDL tolerated | Non-key changes tolerated; key, identity and publication changes fail typed |
| Keyless tables got random IDs | Rejected |
| Element ID `table:k1_k2` | `pg:v1:[…]` |
| Plaintext replication stream | Verified TLS |
| PostgreSQL 10+ | 13+ |

## Consumers still on the legacy source

These configurations outside this crate still use the ordinary PostgreSQL
source and need converting to a native factory and its progress binding:

- Server `examples/configs/02-sources/postgres-cdc-complete.yaml` (one source
  feeding three queries: use `drasi.postgres/shared-changes`).
- Server Getting Started steps 3 to 6.
- Server Trading `trading-sources-only.yaml`, including queries created after
  the source starts (use Core's explicit group reconciliation).
- Server `order-exception-handling.yaml` (mixed families).
