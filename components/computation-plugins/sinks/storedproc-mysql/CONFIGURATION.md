# Native MySQL stored-procedure sink: configuration and management

Implementation `drasi.mysql/storedproc`, version `1`, configuration version
`1`. The configuration is one flat camelCase object; unknown fields are
rejected. The factory publishes every field with its default through
`configuration_details`. See [DESIGN.md](DESIGN.md) for the delivery model.

This is a breaking change from the earlier native sink: the nested secret
`settings` object, snake_case keys, `retry_attempts` and the whole-input
limits are gone. There is no migration path; re-create the component.

## Fields

| Field | Type | Default | Range / rule | Secret | Update |
|---|---|---|---|---|---|
| `host` | string | `localhost` | nonempty | no | Unsupported |
| `port` | integer | `3306` | 1..=65535 | no | Unsupported |
| `database` | string | required | letters, digits, `_`; at most 64 | no | Unsupported |
| `user` | string | empty | required unless activation credentials are bound | no | Restart |
| `password` | string | empty | ignored with activation credentials | **yes** | Restart |
| `sslMode` | string | `require` | `require` (TLS with certificate chain and host-name verification), `disable` (plaintext; refused for a non-loopback `host` unless `allowRemotePlaintext`) | no | Restart |
| `allowRemotePlaintext` | boolean | `false` | permits `sslMode: disable` to a host other than `localhost` or a loopback address | no | Restart |
| `tlsCaPem` | string | none | PEM, at most 64 KiB; requires `sslMode: require` | no | Restart |
| `outputs` | object | `{}` | alias → `{component, port?}` | no | Replace |
| `routes` | object | `{}` | alias → `{added?, updated?, deleted?}`; each `{template}`; keys must match an output | no | In place |
| `defaultTemplate` | object | none | `{added?, updated?, deleted?}` | no | In place |
| `progressTable` | string | `drasi_delivery_progress` | `table` (in `database`) or `schema.table`, lowercase identifiers (schema ≤ 63 bytes, table ≤ 51 bytes); the dead-letter table `<table>_dead_letter` sits beside it | no | Unsupported |
| `maxStreams` | integer | `4096` | 1..=1000000 | no | In place |
| `deliveryGuarantee` | string | `exactlyOnce` | `exactlyOnce`, `atLeastOnce` (see Delivery guarantee) | no | Replace |
| `onPermanentFailure` | string | `stall` | `stall`, `skip` (see Permanent failures) | no | Replace |
| `commandTimeoutMs` | integer | `30000` | 1..=3600000 | no | In place |
| `connectTimeoutMs` | integer | `10000` | 1..=3600000 | no | In place |
| `shutdownTimeoutMs` | integer | `10000` | 1..=3600000 | no | In place |
| `maxProtocolBytes` | integer | `8388608` | 4096..=67108864 | no | Restart |

At least one command template (in `routes` or `defaultTemplate`) is required.

- `commandTimeoutMs` bounds each statement of an operation, including the
  `CALL`. On expiry the session is KILLed and the operation rolls back.
- `connectTimeoutMs` bounds one connection attempt. There is no retry
  budget: a failed attempt is retried by the graph (delivery) or the host
  lifecycle policy (Start), as are deadlocks, lock waits and lost sessions.
- `shutdownTimeoutMs` bounds session cleanup in Stop and Dispose when the
  host gives no deadline, and retiring a failed session during delivery.
- `maxProtocolBytes` is the largest MySQL packet: one command with its
  parameters. It does not limit the input size.
- `maxStreams` bounds the cursor rows (one per producer stream) this
  component may hold.

## Delivery guarantee

`deliveryGuarantee` selects which producers the sink accepts. The plugin's
description of the sink declares the selected guarantee on its input, and the
graph checks it against the producer's declared output when it binds the edge.
An incompatible pairing never rejects the graph change: both components stay,
the edge is failed and unbound, and the sink carries a terminal binding
failure until either side's configuration changes or the edge is removed. It
is never discovered at delivery. Producers whose outputs carry no producer
identity, or that declare none, are refused under either guarantee.

- `exactlyOnce` (default): only producers that declare a durable output
  identity, such as persistent queries. Each procedure call commits with its
  cursor row, so a retry never repeats a completed call. A producer with a
  volatile identity (for example an in-memory query) is refused: "input
  requires ExactlyOnce delivery keyed on producer identity, but the producer's
  output identity is Volatile". Use a persistent producer or set
  `deliveryGuarantee` to `atLeastOnce`.
- `atLeastOnce`: additionally accepts producers with a volatile identity (for
  example in-memory queries). Their outputs carry a per-run epoch; the cursor
  still deduplicates retries within one producer run, but a process restart
  starts a new run whose outputs are delivered against a fresh cursor (the
  stream row is overwritten, not duplicated, so `maxStreams` is not consumed
  per run). Effects after a restart may repeat work the previous run already
  did; nothing is resumed. Persistent producers are still delivered exactly
  once.

## Examples

Literal credentials (resolve `password` through a secret reference):

```json
{
  "host": "mysql.internal",
  "database": "application",
  "user": "drasi_procedures",
  "password": "<secret reference>",
  "outputs": {"people": {"component": "people-query", "port": "out"}},
  "defaultTemplate": {
    "added":   {"template": "CALL add_person({{param after.id}}, {{param after.name}})"},
    "updated": {"template": "CALL update_person({{param after.id}}, {{param after.name}})"},
    "deleted": {"template": "CALL delete_person({{param before.id}})"}
  }
}
```

Per-output routes and a conditional command:

```json
{
  "database": "application",
  "user": "drasi_procedures",
  "password": "<secret reference>",
  "routes": {
    "orders": {
      "updated": {"template": "{{#if after.shipped}}CALL ship({{param after.id}}){{/if}}"}
    }
  },
  "defaultTemplate": {"added": {"template": "CALL audit_add({{param output.alias}}, {{param after}})"}}
}
```

## Templates

- Context: `operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`), `output`
  (`alias`, `component`, `port`), `sequence`, `timestamp` (when the envelope
  has one), `identity` (`namespace`, `id`) and only the `element`, `before`,
  `after`, `removedProperties` and `effectiveFrom` the record carries. A
  `PATCH` `after` holds only the properties it sets; an unknown `before` is
  absent, not null.
- `added` handles `ADD`; `updated` handles `REPLACE` and `PATCH`; `deleted`
  handles `DELETE`.
- Routing: full output alias, then its last dotted segment, then
  `defaultTemplate`. A missing or empty template, or a condition that renders
  no SQL, completes the operation without a `CALL`.
- Helpers: only `{{param <path>}}` emits a value (as a bound `?`). Plain
  `{{x}}`, raw `{{{x}}}`, `{{json x}}`, partials and decorators are rejected at
  construction. `if`, `unless`, `each`, `with` are allowed.
- Render failure (a path the change does not carry, strict mode) is a
  permanent, non-retryable failure of that change (D-5); the input fails and
  the host does not skip it.

## Destination prerequisites

- MySQL 5.7.9 or newer (not MariaDB), `mysql_native_password` or
  `caching_sha2_password`, `performance_schema` enabled (connection
  attributes prove session ownership before any KILL).
- The procedures, with InnoDB tables, and no `COMMIT`, `ROLLBACK` or
  implicitly committing statements inside them (otherwise those operations are
  at-least-once).

### Grants

```sql
-- procedures
GRANT EXECUTE ON application.* TO 'drasi_procedures'@'%';
-- cursor rows
GRANT SELECT, INSERT, UPDATE, DELETE ON application.drasi_delivery_progress TO 'drasi_procedures'@'%';
GRANT SELECT, INSERT, UPDATE, DELETE ON application.drasi_delivery_progress_dead_letter TO 'drasi_procedures'@'%';
-- only if the sink should create the tables at Start
GRANT CREATE ON application.* TO 'drasi_procedures'@'%';
```

The account kills only its own sessions (no `PROCESS` or
`CONNECTION_ADMIN` needed). `GET_LOCK` needs no privilege.

### Pre-created cursor table (least privilege)

```sql
CREATE TABLE application.drasi_delivery_progress (
  owner CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  stream_key VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  version SMALLINT NOT NULL,
  sequence VARCHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  digest BINARY(32) NOT NULL,
  operations BIGINT NOT NULL,
  completed BIGINT NOT NULL,
  epoch VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  generation BIGINT NOT NULL,
  pending BIGINT NOT NULL,
  PRIMARY KEY (owner, stream_key),
  CHECK (version = 4),
  CHECK (operations > 0),
  CHECK (completed >= 0 AND completed <= operations),
  CHECK (pending >= -1 AND pending < operations)
) ENGINE=InnoDB;
CREATE TABLE application.drasi_delivery_progress_dead_letter (
  owner CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  delivery_id VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  stream_key VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  sequence VARCHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  operation BIGINT NOT NULL,
  epoch VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
  generation BIGINT NOT NULL,
  error TEXT NOT NULL,
  recorded_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
  PRIMARY KEY (owner, delivery_id)
) ENGINE=InnoDB;
```

Start verifies both tables: InnoDB and exactly these columns in this order
with these types. Without `CREATE` and
without the table, Start fails with a message naming the missing privilege.
Many components (and graphs) may share one table; rows are separated by
`owner`, and each owner's stream count is bounded by its `maxStreams`.
This is format version 4 (it adds `generation`, `pending` and the
dead-letter table). An earlier format is refused at Start, not migrated: drop
it (after its streams have drained) or configure another `progressTable`.

### Permanent failures

`onPermanentFailure` decides what happens to an operation that can never be
delivered (the procedure `SIGNAL`s or violates a constraint, the template does
not render, or the procedure ends the transaction):

- `stall` (default): the sink returns a permanent failure and the graph stalls
  the node until an operator acts.
- `skip`: the sink records the operation (delivery ID, stream, sequence,
  operation, the error and the time) in `<progressTable>_dead_letter` and
  advances the cursor past it in one transaction, then reports it as skipped;
  the graph acknowledges the input, delivers its other operations and counts
  the skip. A replay never repeats it. A cursor rejection (gap, conflict,
  expired receipt, capacity) is never skipped.

Procedures must not `COMMIT`, `ROLLBACK` or cause an implicit commit: the sink
detects the ended transaction and fails permanently (the cursor is never
advanced in autocommit). Bind `{{param deliveryId}}` to deduplicate effects
outside MySQL.

## Updates

| Boundary | Fields |
|---|---|
| In place (Pause) | `routes`, `defaultTemplate`, `maxStreams`, `commandTimeoutMs`, `connectTimeoutMs`, `shutdownTimeoutMs` |
| Restart (Stop) | `user`, `password`, `sslMode`, `allowRemotePlaintext`, `tlsCaPem`, `maxProtocolBytes` |
| Replace | `outputs`, changed incoming edges, `deliveryGuarantee`, `onPermanentFailure` (declared in the description) |
| Unsupported | `host`, `port`, `database`, `progressTable` (they locate the receipts; remove with Erase and add again) |

An unchanged configuration is an in-place no-op. A proposed configuration that
fails validation is rejected.

## Lifecycle and removal

- Start opens the session and creates or verifies the cursor and dead-letter
  tables. A database that is unreachable within `connectTimeoutMs` or the
  start deadline fails Start (retryable by the host).
- Stop closes the session; a session left mid-command (Abort) is KILLed.
- Removal (`Dispose` with `Erase`) deletes only this component's rows from
  the cursor and dead-letter tables (`WHERE owner = '<owner>'`). It is
  idempotent and never drops the tables or touches business data. Disposal with `Preserve`
  keeps the rows, so a re-created component with the same graph and
  component ID resumes its receipts.

## Operations

- **Owner of a component:**
  `SELECT LOWER(SHA2(CONCAT('drasi.storedproc/owner/v1', CHAR(0), '<graph_id>', CHAR(0), '<component_id>'), 256));`
  (the plugin also logs it at debug level when constructed).
- **Inspect progress:**
  `SELECT stream_key, sequence, operations, completed FROM drasi_delivery_progress WHERE owner = '<owner>';`
  `completed < operations` means the batch is in progress or failing.
- **Manual Erase** (for example when activation credentials are no longer
  available): stop the component, then
  `DELETE FROM drasi_delivery_progress WHERE owner = '<owner>';`
- **Stream capacity exhausted:** raise `maxStreams` (in place) or delete rows of
  streams that will not return.
- **Receipt expired / payload conflict / gap:** the destination holds a newer
  or different batch for the stream than the host delivers (for example the
  host's state was restored from an older backup). Investigate before deleting
  the stream's row; deleting it makes the next batch re-apply.
- **Skipped operations:**
  `SELECT delivery_id, stream_key, sequence, operation, error, recorded_at FROM drasi_delivery_progress_dead_letter WHERE owner = '<owner>';`
- **"the command ended the delivery transaction":** remove `COMMIT`,
  `ROLLBACK` and statements that commit implicitly from the procedure.
- **Stuck Stop:** a retryable Stop error means a retired session could not be
  proven gone in time; retry Stop. Check `information_schema.PROCESSLIST` for
  the account's sessions.

## Legacy configuration mapping

| Legacy reaction field | Native field |
|---|---|
| `hostname` | `host` |
| `port`, `database`, `user`, `password` | same names (`password` is a secret field) |
| `ssl` | `sslMode` (`require` is the default; `disable` only to loopback unless `allowRemotePlaintext`) |
| `identityProvider` | activation credentials (identity resource) |
| `routes`, `defaultTemplate` | same, keyed by **output alias** instead of query ID |
| `commandTimeoutMs` | `commandTimeoutMs` (per statement) |
| `retryAttempts` (command retries) | none: the graph retries transient failures (including connection failures) in place with backoff |
| log-and-skip of failed commands | `onPermanentFailure: skip` (recorded in the dead-letter table) |
| template context `query_id`, `query_name` | `output.alias`, `output.component` |
| template context `data` | `after` / `before` |
| `{{json x}}` inline | not allowed; use `{{param x}}` (objects bind as JSON text) |
