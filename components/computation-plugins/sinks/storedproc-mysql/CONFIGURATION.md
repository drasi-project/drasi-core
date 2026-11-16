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
| `hostname` | string | `localhost` | nonempty | no | Restart |
| `port` | integer | `3306` | 1..=65535 | no | Restart |
| `database` | string | required | letters, digits, `_`; at most 64 | no | Restart |
| `user` | string | empty | required unless activation credentials are bound | no | Restart |
| `password` | string | empty | ignored with activation credentials | **yes** | Restart |
| `ssl` | boolean | `false` | `true` = TLS with certificate and host-name verification | no | Restart |
| `tlsCaPem` | string | none | PEM, at most 64 KiB; requires `ssl: true` | no | Restart |
| `outputs` | object | `{}` | alias → `{component, port?}` | no | Replace |
| `routes` | object | `{}` | alias → `{added?, updated?, deleted?}`; each `{template}`; keys must match an output | no | In place |
| `defaultTemplate` | object | none | `{added?, updated?, deleted?}` | no | In place |
| `progressTable` | string | `drasi_delivery_progress` | identifier, at most 64; in `database` | no | Unsupported |
| `maxStreams` | integer | `4096` | 1..=1048576 | no | In place |
| `deliveryGuarantee` | string | `exactlyOnce` | `exactlyOnce`, `atLeastOnce` (see Delivery guarantee) | no | Restart |
| `commandTimeoutMs` | integer | `30000` | 1..=86400000 | no | In place |
| `connectTimeoutMs` | integer | `5000` | 1..=86400000 | no | In place |
| `shutdownTimeoutMs` | integer | `10000` | 1..=86400000 | no | In place |
| `connectRetryAttempts` | integer | `5` | 0..=32 | no | In place |
| `reconnectBudgetMs` | integer | `30000` | 1..=86400000 | no | In place |
| `maxProtocolBytes` | integer | `8388608` | 4096..=67108864 | no | Restart |

At least one command template (in `routes` or `defaultTemplate`) is required.

- `commandTimeoutMs` bounds each statement of an operation, including the
  `CALL`. On expiry the session is KILLed and the operation rolls back.
- `connectTimeoutMs` bounds one connection attempt; `connectRetryAttempts`
  and `reconnectBudgetMs` bound a connect sequence at Start and on reconnect.
- `shutdownTimeoutMs` bounds session cleanup in Stop and Dispose when the
  host gives no deadline, and retiring a failed session during delivery.
- `maxProtocolBytes` is the largest MySQL packet: one command with its
  parameters. It does not limit the input size.
- `maxStreams` bounds the cursor rows (one per producer stream) this
  component may hold.

## Delivery guarantee

`deliveryGuarantee` selects which producers the sink accepts:

- `exactlyOnce` (default): only producers with persistent progress, such as
  persistent queries. Each procedure call commits with its cursor row, so a
  retry never repeats a completed call. Wiring an in-memory query to this
  sink is refused when the graph configuration is accepted (the edge becomes
  bindable, or either endpoint is created, replaced or updated), with the
  error "the producer is an in-memory query without persistent progress; use
  a persistent query or set deliveryGuarantee to atLeastOnce". The same
  refusal is applied, permanently, at the first delivery if a volatile input
  still arrives.
- `atLeastOnce`: additionally accepts in-memory (volatile) queries. Their
  outputs carry a per-run epoch; the cursor still deduplicates retries within
  one producer run, but a process restart starts a new run whose outputs are
  delivered against a fresh cursor (the stream row is overwritten, not
  duplicated, so `maxStreams` is not consumed per run). Effects after a
  restart may repeat work the previous run already did; nothing is resumed.
  Persistent producers are still delivered exactly once.

## Examples

Literal credentials (resolve `password` through a secret reference):

```json
{
  "hostname": "mysql.internal",
  "database": "application",
  "user": "drasi_procedures",
  "password": "<secret reference>",
  "ssl": true,
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
-- only if the sink should create the table at Start
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
  PRIMARY KEY (owner, stream_key),
  CHECK (version = 3),
  CHECK (operations > 0),
  CHECK (completed >= 0 AND completed <= operations)
) ENGINE=InnoDB;
```

Start verifies the table: InnoDB, exactly these columns in this order with
these types, and `PRIMARY KEY (owner, stream_key)`. Without `CREATE` and
without the table, Start fails with a message naming the missing privilege.
Many components (and graphs) may share one table; rows are separated by
`owner`, and each owner's stream count is bounded by its `maxStreams`.
This is format version 3 (it adds `epoch`). A format 2 table is refused at
Start, not migrated: drop it (after its streams have drained) or configure
another `progressTable`.

## Updates

| Boundary | Fields |
|---|---|
| In place (Pause) | `routes`, `defaultTemplate`, `maxStreams`, `commandTimeoutMs`, `connectTimeoutMs`, `shutdownTimeoutMs`, `connectRetryAttempts`, `reconnectBudgetMs` |
| Restart (Stop) | `hostname`, `port`, `database`, `user`, `password`, `ssl`, `tlsCaPem`, `maxProtocolBytes`, `deliveryGuarantee` |
| Replace | `outputs`, changed incoming edges |
| Unsupported | `progressTable` (the receipt identity; remove with Erase and add again) |

An unchanged configuration is an in-place no-op. A proposed configuration that
fails validation is rejected.

## Lifecycle and removal

- Start opens the session and creates or verifies the cursor table. A
  database that is unreachable within the start deadline or
  `reconnectBudgetMs` fails Start (retryable by the host).
- Stop closes the session; a session left mid-command (Abort) is KILLed.
- Removal (`Dispose` with `Erase`) deletes only this component's rows:
  `DELETE FROM <progressTable> WHERE owner = '<owner>'`. It is idempotent and
  never drops the table or touches business data. Disposal with `Preserve`
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
- **Stuck Stop:** a retryable Stop error means a retired session could not be
  proven gone in time; retry Stop. Check `information_schema.PROCESSLIST` for
  the account's sessions.

## Legacy configuration mapping

| Legacy reaction field | Native field |
|---|---|
| `hostname`, `port`, `database`, `user`, `password`, `ssl` | same names (`password` is a secret field) |
| `identityProvider` | activation credentials (identity resource) |
| `routes`, `defaultTemplate` | same, keyed by **output alias** instead of query ID |
| `commandTimeoutMs` | `commandTimeoutMs` (per statement) |
| `retryAttempts` (command retries) | none: the host retries classified failures; `connectRetryAttempts` covers connecting only |
| template context `query_id`, `query_name` | `output.alias`, `output.component` |
| template context `data` | `after` / `before` |
| `{{json x}}` inline | not allowed; use `{{param x}}` (objects bind as JSON text) |
