# Native PostgreSQL stored-procedure sink: configuration and management

The factory `drasi.postgres/storedproc` takes one flat camelCase object.
Unknown fields are rejected at every level. `password` and
`tlsClientPrivateKeyPem` are top-level **secret** fields: the host accepts only
secret references for them. Everything else, including templates and routes,
is inspectable. The factory publishes every default and range through its
configuration details. [DESIGN.md](DESIGN.md) explains the behaviour behind
each field.

The consumer needs its construction scope (the graph identity), which the host
always supplies for consumers; together with the component ID it names the
destination cursor owner.

## Fields

| Field | Type | Default | Range / values | Secret | Update |
|---|---|---|---|---|---|
| `host` | string | `localhost` | nonempty, no NUL | no | Unsupported |
| `port` | integer | `5432` | 1..=65535 | no | Unsupported |
| `database` | string | required | nonempty | no | Unsupported |
| `user` | string | required | nonempty; replaced by activation credentials | no | Stop |
| `password` | string | empty | ≤ 4096 bytes; replaced by activation credentials | **yes** | Stop |
| `sslMode` | string | `require` | `require` (verified TLS: trusted chain and matching host name, libpq `verify-full`), `disable` | no | Stop |
| `allowRemotePlaintext` | boolean | `false` | permits `sslMode: disable` to a host other than `localhost` or a loopback address | no | Stop |
| `tlsCaPem` | string | none | PEM, ≤ 64 KiB; requires `require`; trusted in addition to the system roots | no | Stop |
| `tlsClientCertificatePem` | string | none | PEM, ≤ 64 KiB; requires `tlsClientPrivateKeyPem` | no | Stop |
| `tlsClientPrivateKeyPem` | string | none | PKCS#8 PEM, ≤ 64 KiB; requires `tlsClientCertificatePem` | **yes** | Stop |
| `outputs` | object | `{}` | alias → `{component, port?}` | no | Replace |
| `routes` | object | `{}` | alias → `{added?, updated?, deleted?: {template}}`; each key must match an output alias or its last dotted segment | no | Pause |
| `defaultTemplate` | object | none | `{added?, updated?, deleted?: {template}}` | no | Pause |
| `progressTable` | string | `drasi_delivery_progress` | `table` (in schema `public`) or `schema.table`, lowercase identifiers (`[a-z_][a-z0-9_]*`; schema ≤ 63 bytes, table ≤ 51 bytes); the dead-letter table `<table>_dead_letter` sits beside it | no | Unsupported |
| `maxStreams` | integer | `4096` | 1..=1,000,000 cursor rows for this consumer | no | Pause |
| `deliveryGuarantee` | string | `exactlyOnce` | `exactlyOnce`, `atLeastOnce` (see Delivery guarantee) | no | Replace |
| `onPermanentFailure` | string | `stall` | `stall`, `skip` (see Permanent failures) | no | Replace |
| `commandTimeoutMs` | integer | `30000` | 1..=2,147,483,647; server `statement_timeout` per command, client waits 5 s longer | no | Pause |
| `connectTimeoutMs` | integer | `10000` | 1..=3,600,000; one attempt: TCP, TLS and authentication | no | Pause |
| `shutdownTimeoutMs` | integer | `10000` | 1..=3,600,000; Stop's and Dispose's bound when the host gives no earlier deadline | no | Pause |

There is no retry budget: each operation is one attempt, and the graph retries
transient failures (rolled back, connection lost, commit outcome unknown) with
its bounded backoff.

At least one nonempty template is required. `routes` keys that match no
output alias are rejected.

## Examples

In the examples, `password` is omitted: supply it as a secret reference in
the component specification (a literal is refused for a secret field).

Minimal:

```json
{
  "host": "db.internal.example",
  "database": "app",
  "user": "drasi_sink",
  "defaultTemplate": {
    "added":   {"template": "CALL app.upsert_person({{param after.id}}, {{param after.name}})"},
    "updated": {"template": "CALL app.upsert_person({{param after.id}}, {{param after.name}})"},
    "deleted": {"template": "CALL app.delete_person({{param identity.id}})"}
  }
}
```

Per-output routes, a private CA, a least-privilege progress table and a lower
stream limit:

```json
{
  "host": "pg.example", "database": "app", "user": "drasi_sink",
  "tlsCaPem": "-----BEGIN CERTIFICATE-----\n...",
  "outputs": {"people": {"component": "6439b52f-06c0-5e90-8905-d736f18e34ee"}},
  "routes": {
    "people": {
      "added":   {"template": "CALL app.person_added({{param after}})"},
      "deleted": {"template": ""}
    }
  },
  "defaultTemplate": {"deleted": {"template": "CALL app.removed({{param identity.id}})"}},
  "progressTable": "drasi.delivery_progress",
  "maxStreams": 64
}
```

Local development without TLS (allowed only for `localhost` or a loopback
address):

```json
{"host": "127.0.0.1", "sslMode": "disable", "database": "postgres", "user": "postgres",
 "defaultTemplate": {"added": {"template": "CALL log_change({{param after}})"}}}
```

## Templates

Templates are Handlebars SQL. The only construct that may emit a value is
`{{param <expression>}}`: it renders the next placeholder (`$1`, `$2`, …) and
binds the value. Objects and arrays bind as JSON (use `jsonb` parameters);
strings bind as text unless the target is numeric; integers narrow only
without overflow. Rejected at construction: `{{after.id}}`, `{{{after.id}}}`,
`{{json after}}` and any other value helper, partials (`{{> x}}`), decorators,
`{{param}}` without exactly one value, and block helpers other than `if`,
`unless`, `each` and `with` (whose bodies follow the same rule). A `param`
value may be a sub-expression of a standard helper (`{{param (json after)}}`
binds the JSON text). Write procedure names and other SQL structure literally.
The engine is the [shared native template engine](../../common/template/README.md),
the same in every native sink.

Context keys: `operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`), `output`
(`alias`, `component`, `port`), `sequence`, `timestamp` (RFC 3339, only when
known), `identity` (`namespace`, `id`), and only the `element`, `before`,
`after`, `removedProperties` and `effectiveFrom` that the change carries. A
`PATCH`'s `after` holds only the properties it sets; an unknown `before` is
absent, not null. Templates render in strict mode: reading an absent field
fails.

Routing, per operation: the route for the full output alias, then the route
for its last dotted segment, then `defaultTemplate`. `added` serves `ADD`,
`updated` serves `REPLACE` and `PATCH`, `deleted` serves `DELETE`. The first
configured spec wins, so an explicit empty `template` means "no command" and
stops the fallback. A template whose condition renders only whitespace runs no
command. Operations without a command still advance the cursor.

Render-failure policy (D-5): a template that fails to render fails the input
permanently. It is not retried by the sink or the host runner and never
skipped. Fix the template with an in-place update; the host's replay then
delivers.

## Destination prerequisites

### Procedures

A procedure must do its work only in the caller's transaction: no `COMMIT` or
`ROLLBACK`, no other connections (`dblink`, foreign-data writes) and no
external effects. Otherwise the effect is at-least-once (DESIGN.md). It must
not touch the progress table. The sink's role needs `EXECUTE` on the procedure
(PUBLIC has it by default) and whatever the procedure body needs unless it is
`SECURITY DEFINER`.

### Progress table and grants

The sink role needs:

- `CONNECT` on the database;
- `USAGE` on the progress table's schema;
- `SELECT`, `INSERT`, `UPDATE`, `DELETE` on the progress table and its
  dead-letter table;
- either `CREATE` on that schema (the sink then creates the tables at first
  start), or pre-created tables.

PostgreSQL 15 and later no longer grant `CREATE` on `public` to every role, so
a least-privilege role needs a pre-created table. As an administrator:

```sql
CREATE SCHEMA IF NOT EXISTS drasi;
CREATE TABLE IF NOT EXISTS "drasi"."delivery_progress" (
    owner TEXT NOT NULL CHECK (owner ~ '^[0-9a-f]{64}$'),
    stream_key TEXT NOT NULL,
    version SMALLINT NOT NULL CHECK (version = 4),
    sequence TEXT NOT NULL CHECK (sequence ~ '^[1-9][0-9]{0,19}$'),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    operations BIGINT NOT NULL CHECK (operations > 0),
    completed BIGINT NOT NULL CHECK (completed >= 0 AND completed <= operations),
    epoch TEXT NOT NULL CHECK (epoch = '' OR epoch ~ '^[0-9a-f]{64}$'),
    generation BIGINT NOT NULL CHECK (generation >= 0),
    pending BIGINT NOT NULL CHECK (pending >= -1 AND pending < operations),
    PRIMARY KEY (owner, stream_key)
);
CREATE TABLE IF NOT EXISTS "drasi"."delivery_progress_dead_letter" (
    owner TEXT NOT NULL CHECK (owner ~ '^[0-9a-f]{64}$'),
    delivery_id TEXT NOT NULL,
    stream_key TEXT NOT NULL,
    sequence TEXT NOT NULL,
    operation BIGINT NOT NULL,
    epoch TEXT NOT NULL,
    generation BIGINT NOT NULL,
    error TEXT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (owner, delivery_id)
);
GRANT USAGE ON SCHEMA drasi TO drasi_sink;
GRANT SELECT, INSERT, UPDATE, DELETE ON drasi.delivery_progress,
    drasi.delivery_progress_dead_letter TO drasi_sink;
```

and configure `"progressTable": "drasi.delivery_progress"`. The statement is
also available as `create_table_sql` in the crate. This is format 4 (it adds
`generation` and `pending` and the dead-letter table); an earlier format is
refused at Start, not migrated. Start fails with a message
naming the missing schema or privilege, for example
`role "drasi_sink" lacks CREATE on schema "public"; ...`.

### Delivery guarantee

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

### Permanent failures

`onPermanentFailure` decides what happens to an operation that can never be
delivered (the procedure or its commit rejected it, the template did not
render, or the procedure ran `COMMIT`/`ROLLBACK`):

- `stall` (default): the sink returns a permanent failure and the graph stalls
  the node until an operator fixes the procedure, template or data (or changes
  this setting) and restarts it.
- `skip`: the sink records the operation in `<progressTable>_dead_letter`
  (delivery ID, stream, sequence, operation, epoch, generation, the error and
  the time) and advances the cursor past it **in one transaction**, then
  reports it as skipped; the graph acknowledges the input, delivers its other
  operations, and counts the skip on the node. A replay never repeats it.
  A destination cursor rejection (gap, conflict, expired receipt, capacity)
  cannot be skipped: it still stalls.

Procedures must keep their work in the sink's transaction: PostgreSQL refuses
`COMMIT` or `ROLLBACK` in a procedure called inside it (SQLSTATE `2D000`),
which is a permanent failure. The template context carries `deliveryId`, the
host's stable operation identity, for deduplicating effects outside
PostgreSQL.

### TLS and credentials

`sslMode: require` verifies the server certificate against the system roots
plus `tlsCaPem` and checks that it names `host`. For mutual TLS supply
`tlsClientCertificatePem` and a PKCS#8 `tlsClientPrivateKeyPem`. When the host
supplies activation credentials (username/password or token), they replace
`user` and `password` for that activation; certificate identity providers are
not supported, use static PEM. The host does not yet bind identity resources
to consumer factories (DESIGN.md, Known limitations).

## Updates

| Boundary | Fields |
|---|---|
| Pause (in place, after a completed pause) | `routes`, `defaultTemplate`, `maxStreams`, `commandTimeoutMs`, `connectTimeoutMs`, `shutdownTimeoutMs` |
| Stop (restart) | `user`, `password`, `sslMode`, `allowRemotePlaintext`, `tlsCaPem`, `tlsClientCertificatePem`, `tlsClientPrivateKeyPem` |
| Replace (recreate, cursor preserved) | `outputs`, incoming edges, `deliveryGuarantee`, `onPermanentFailure` (declared in the description) |
| Unsupported | `host`, `port`, `database`, `progressTable`, the graph identity: they locate the destination cursor; remove the component with Erase and add a new one |

## Lifecycle and removal

Start connects and prepares the progress table; a failure leaves the sink
startable (connection failures are retryable, missing privileges terminal).
Pause is host-driven. Stop drains (waits for a running command) or aborts,
bounded by the deadline and `shutdownTimeoutMs`.

Removal with **Erase** deletes exactly this owner's rows (`WHERE owner =
'<owner>'`) from the progress and dead-letter tables over a fresh connection. It is idempotent (repeat it freely), never
drops the table and never touches other owners' rows, business tables or
procedures. If the database is unreachable, Erase fails retryably and the host
retries it. Dispose without Erase (`Preserve`) leaves the rows, so a component
re-created with the same identity resumes exactly where it stopped.

## Operations runbook

- **Find this consumer's rows.** The owner is
  `encode(sha256(convert_to('drasi.storedproc/owner/v1','UTF8') || '\x00'::bytea
  || convert_to('<graph id>','UTF8') || '\x00'::bytea ||
  convert_to('<component id>','UTF8')), 'hex')`.
  `SELECT * FROM drasi.delivery_progress WHERE owner = '<owner>'`.
- **Manual erase** (component already gone): delete those rows. Never edit or
  delete the rows of a live consumer: committed operations would be applied
  again or unfinished batches rejected.
- **"stream capacity is exhausted"**: the owner holds `maxStreams` rows. Raise
  `maxStreams` (in place) or remove rows of retired streams; delivery resumes on
  the next replay without a restart.
- **"lacks … on …"** at start: grant the named privilege or pre-create the
  table, then start again.
- **"payload conflicts with its receipt" / "receipt expired" / "operation gap"**:
  the progress rows do not match the host's delivery progress (rows edited,
  restored from a backup, or a component recreated with another identity but
  the same owner). Investigate before deleting rows.
- **Connection lost / `pg_terminate_backend`**: no action; the graph retries
  the operation, the sink reconnects and the cursor prevents duplicates.
- **Skipped operations** (`onPermanentFailure: skip`):
  `SELECT * FROM drasi.delivery_progress_dead_letter WHERE owner = '<owner>'
  ORDER BY recorded_at`. Replay the business effect by hand if needed; the rows
  are this sink's and are removed by Erase.
- **"the command ended the delivery transaction"**: the procedure ran
  `COMMIT`/`ROLLBACK`; remove the transaction control from the procedure.
- **Sessions** use `application_name = 'drasi-native-storedproc-postgres'`.
- **Old format**: the legacy table `public.drasi_delivery_stream_v1` is not read
  or migrated by this sink.

## Legacy configuration mapping

| Legacy `reactions/storedproc-postgres` / previous native `settings` | Native field |
|---|---|
| `settings` wrapper object | removed: fields are top level |
| `hostname` | `host` |
| `port`, `database`, `user`, `password` | unchanged names (`password` now a top-level secret) |
| `ssl: true` / `false` | `sslMode: require` / `disable` (plus `allowRemotePlaintext` for a remote host) |
| `tls_ca_pem` | `tlsCaPem` |
| `tls_client_cert_pem`, `tls_client_key_pem` | `tlsClientCertificatePem`, `tlsClientPrivateKeyPem` (PKCS#8) |
| identity provider (password/token) | host activation credentials |
| identity provider (certificate) | static `tlsClientCertificatePem`/`tlsClientPrivateKeyPem` |
| `routes` keyed by query name | `routes` keyed by output alias (or its last dotted segment) |
| `default_template` | `defaultTemplate` |
| `command_timeout_ms` | `commandTimeoutMs` |
| `connect_timeout_ms` | `connectTimeoutMs` (one attempt; no longer applied twice) |
| `shutdown_timeout_ms` | `shutdownTimeoutMs` (only when the host gives no earlier deadline) |
| `retry_attempts`, backoff settings | removed: the graph retries transient failures |
| log-and-skip of failed commands | `onPermanentFailure: skip` (recorded in the dead-letter table) |
| transactional delivery opt-in (`public.drasi_delivery_stream_v1`) | always on; `progressTable`, `maxStreams` (format 4, per owner) |

Template context: legacy `query_id`/`query_name` are replaced by `output`
(`alias`, `component`, `port`); `operation` `UPDATE` becomes `REPLACE` or
`PATCH`; `data` is removed; `metadata` is replaced by `sequence`, `timestamp`
and `identity`. Legacy error handling logged and skipped failed commands; the
native sink fails the input and the host replays it.
