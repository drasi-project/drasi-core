# SQL Server stored-procedure sink: configuration

Implementation `drasi.mssql/storedproc`, configuration version 1. The
configuration is one flat camelCase object; unknown fields are rejected.

> **Live qualification is deferred (CW-78).** These settings are verified by
> unit and construction tests only.

## Fields

| Field | Type | Default | Range / values | Required | Secret | Update |
|---|---|---|---|---|---|---|
| `host` | string | — | host name or IP, ≤ 253 bytes | yes | no | Restart |
| `port` | integer | `1433` | 1–65535 | no | no | Restart |
| `database` | string | — | ≤ 128 characters | yes | no | Unsupported |
| `user` | string | none | ≤ 1024 bytes | without an identity binding | no | Restart |
| `password` | string | none | ≤ 1024 bytes | without an identity binding | **yes** | Restart |
| `encryption` | string | `required` | `required`, `loginOnly`, `disabled` | no | no | Restart |
| `trustServerCertificate` | boolean | `false` | — | no | no | Restart |
| `trustedCaPem` | string | none | PEM certificates, ≤ 256 KiB | no | no | Restart |
| `hostNameInCertificate` | string | `host` | host name | no | no | Restart |
| `progressTable` | string | `dbo.drasi_delivery_progress` | `table` or `schema.table`; letters, digits, `_`; ≤ 128 per part | no | no | Unsupported |
| `maxStreams` | integer | `4096` | 1–1 000 000 | no | no | InPlace |
| `deliveryGuarantee` | string | `exactlyOnce` | `exactlyOnce`, `atLeastOnce` (see Delivery guarantee) | no | no | Recreate |
| `outputs` | object | `{}` | `{alias: {component, port?}}` | no | no | Recreate |
| `routes` | object | `{}` | `{alias: {added?, updated?, deleted?: {template}}}` | no | no | InPlace |
| `defaultTemplate` | object | none | `{added?, updated?, deleted?: {template}}` | no | no | InPlace |
| `commandTimeoutMs` | integer | `30000` | 1–3 600 000 | no | no | InPlace |
| `connectTimeoutMs` | integer | `15000` | 1–300 000 | no | no | InPlace |
| `retryBudgetMs` | integer | `60000` | 0–3 600 000 | no | no | InPlace |

Rules checked at construction:

* At least one non-empty template must be configured in `routes` or
  `defaultTemplate`.
* Every template must be parameter-only (see [Templates](#templates)).
* Every `routes` key must name an output alias, fully or as its last dotted
  segment.
* `trustServerCertificate` and `trustedCaPem` are mutually exclusive.
  Certificate settings require encryption.
* Without an identity binding, `user` and a non-empty `password` are required.
* The sink needs its graph scope (the host always provides it for consumers).

`commandTimeoutMs` bounds each server round trip of a statement, not a whole
input. An expired round trip drops the connection; the operation is retried
after reconnect within `retryBudgetMs` (see `DESIGN.md`). `retryBudgetMs: 0`
returns the first failure to the host.

## Examples

Minimal, with literal SQL authentication and TLS:

```json
{
  "host": "sql.example.internal",
  "database": "orders",
  "user": "drasi_sink",
  "password": {"secretRef": "orders-sink-password"},
  "outputs": {"orders": {"component": "bc769198-8a46-426c-acb5-435b084520d0", "port": "out"}},
  "defaultTemplate": {
    "added":   {"template": "EXEC dbo.UpsertOrder {{param after.id}}, {{param (json after)}}, {{param deliveryId}}"},
    "updated": {"template": "EXEC dbo.UpsertOrder {{param after.id}}, {{param (json after)}}, {{param deliveryId}}"},
    "deleted": {"template": "EXEC dbo.DeleteOrder {{param before.id}}, {{param deliveryId}}"}
  }
}
```

The `password` is a host secret reference; how it is written depends on the
host. Use the producer's real component UUID in `outputs`; the alias is only
a name.

Routes for two outputs, a private CA, a pre-created cursor table:

```json
{
  "host": "10.0.4.12", "port": 1433, "database": "analytics",
  "user": "drasi_sink", "password": {"secretRef": "analytics-sink-password"},
  "trustedCaPem": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n",
  "hostNameInCertificate": "sql.analytics.internal",
  "progressTable": "drasi.delivery_progress",
  "outputs": {
    "rooms":   {"component": "…uuid…", "port": "out"},
    "sensors": {"component": "…uuid…", "port": "out"}
  },
  "routes": {
    "rooms":   {"added": {"template": "EXEC drasi.AddRoom {{param identity.id}}, {{param after.name}}"}},
    "sensors": {"updated": {"template": "{{#if after.active}}EXEC drasi.Reading {{param identity.id}}, {{param after.value}}{{/if}}"}}
  },
  "commandTimeoutMs": 10000
}
```

Entra ID (Azure SQL), once the Host binds identities to consumers (see
[Authentication](#authentication)): omit `user` and `password` and bind an
identity provider that returns a `Token` credential for
`https://database.windows.net/`.

## Templates

### Context

| Key | Present | Meaning |
|---|---|---|
| `operation` | always | `ADD`, `REPLACE`, `PATCH` or `DELETE` |
| `output` | always | `{alias, component, port}` of the producing output |
| `sequence` | always | The producer's sequence of the envelope |
| `timestamp` | when the envelope has one | RFC 3339 event time |
| `identity` | always | `{namespace, id}` of the element |
| `element` | when known | `{labels, kind?, inNode?, outNode?}` |
| `before` | when the record carries it | Properties before the change |
| `after` | ADD, REPLACE, PATCH | All properties (ADD, REPLACE) or only those set (PATCH) |
| `removedProperties` | PATCH | Names of removed properties |
| `effectiveFrom` | when known | Business time of the element |
| `deliveryId` | always | Stable operation identity for application deduplication |

Routing: the full output alias, then the alias's last dotted segment, then
`defaultTemplate`, per operation. `updated` covers REPLACE and PATCH. An
operation without a template, or whose template renders only whitespace,
runs no command but still advances the cursor.

### Helpers and the parameter-only rule

* `{{param path}}` binds the value as the next `@Pn` parameter. Unsigned
  integers beyond `BIGINT` bind as decimal text; objects and arrays bind as
  JSON text; `null` binds `NULL`.
* `{{param (json path)}}` binds the value serialized as JSON.
* `{{#if}}`, `{{#unless}}`, `{{#each}}` and `{{#with}}` may be used for control
  flow; their bodies follow the same rule. Comments are allowed.

Everything else that could write a value into the SQL text is rejected when
the sink is created: `{{x}}`, `{{{x}}}`, `{{& x}}`, `{{json x}}` outside
`param`, `{{lookup …}}` outside `param`, partials (`{{> p}}`), decorators
(`{{* d}}`) and other block helpers. Write procedure names and structure as
literal text.

A command may bind at most 2 098 values (2 100 per request, two used by
`sp_executesql`). Larger commands fail permanently; pass a JSON document with
`{{param (json after)}}` instead.

### Render-failure policy

A render failure (a missing field in strict mode, the parameter limit) is a
permanent failure of that operation. The operation's transaction rolls back,
the cursor does not advance and the host reports the failure. Fix the
template in place (routes and templates are InPlace updates) and the host
retries the same operation.

## Destination prerequisites

### Procedures

Procedures run inside the sink's transaction. For exactly-once delivery they
must **not** `COMMIT`, `ROLLBACK`, open a transaction they leave open, or
produce effects outside SQL Server's transaction. A procedure that changes
the transaction depth fails the operation permanently and may already have
committed its effect, which a redelivery repeats (at-least-once). Result sets
are read and discarded.

### Grants

The sink's login needs:

```sql
GRANT EXECUTE ON OBJECT::dbo.UpsertOrder TO drasi_sink;      -- each procedure
GRANT SELECT, INSERT, UPDATE, DELETE ON OBJECT::dbo.drasi_delivery_progress TO drasi_sink;
```

To let the sink create the table at its first start, also grant:

```sql
GRANT CREATE TABLE TO drasi_sink;
GRANT ALTER ON SCHEMA::dbo TO drasi_sink;
```

For a least-privilege login, create the table in advance instead (format 3,
identical for the three stored-procedure sinks) and grant only DML:

```sql
CREATE TABLE dbo.drasi_delivery_progress (
    owner NVARCHAR(64) NOT NULL,
    stream_key NVARCHAR(128) NOT NULL,
    version SMALLINT NOT NULL CHECK (version = 3),
    sequence NVARCHAR(20) NOT NULL,
    digest VARBINARY(32) NOT NULL CHECK (DATALENGTH(digest) = 32),
    operations BIGINT NOT NULL CHECK (operations > 0),
    completed BIGINT NOT NULL CHECK (completed >= 0 AND completed <= operations),
    epoch NVARCHAR(64) NOT NULL,
    PRIMARY KEY (owner, stream_key)
);
```

A format 2 table (without `epoch`) is refused at Start, not migrated. Start checks the four DML permissions and the columns and names the missing
grant in its error. A missing `CREATE TABLE` permission fails Start with a
message that points here.

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

### Authentication

| Mode | How |
|---|---|
| SQL authentication | `user` and `password` |
| Activation identity, SQL login | Bind an identity provider returning `UsernamePassword`; literals are ignored for that activation and never used as a fallback |
| Activation identity, Entra ID | Bind an identity provider returning `Token` with an access token for Azure SQL; sent as Tiberius `AADToken` |

The identity provider receives `host` and `port` as its endpoint context.
Credentials are refreshed at each start and never written to configuration.
Certificate credentials are rejected.

**Current limitation:** the Host SDK does not yet bind identity resources to
external-consumer factories, so only SQL authentication with literal
credentials works at runtime today.

### Encryption

| `encryption` | Behaviour |
|---|---|
| `required` (default) | TLS for the whole session; the certificate is verified against system roots plus `trustedCaPem` |
| `loginOnly` | TLS for the login only; data is plaintext. Logs a warning |
| `disabled` | No TLS; credentials and data are plaintext. Logs a warning |

`trustServerCertificate: true` disables certificate verification (testing
only) and logs a warning.

## Update matrix

| Field | Boundary | Why |
|---|---|---|
| `routes`, `defaultTemplate` | InPlace (Pause) | Rendered per operation; recompiled on apply |
| `maxStreams`, `retryBudgetMs` | InPlace (Pause) | Read per operation |
| `commandTimeoutMs`, `connectTimeoutMs` | InPlace (Pause) | The idle connection is replaced; the next operation reconnects |
| `host`, `port`, `user`, `password`, `encryption`, `trustServerCertificate`, `trustedCaPem`, `hostNameInCertificate` | Restart (Stop) | The connection and credentials are established at Start |
| `deliveryGuarantee` | Recreate (Replace) | Declared on the input port; the graph re-checks every producer against it |
| `outputs`, bound inputs | Recreate (Replace) | Route aliases and delivery streams change (live rebinding is CW-94) |
| `database`, `progressTable` | **Unsupported** | They locate the cursor. Moving it would lose every stream's completed prefix (duplicates or refused operations) and leave old rows behind. Remove the sink with Erase and create a new one |

A `host` change is meant for the same database reached at another address
(failover, DNS). Pointing `host` at a different server that has a different
copy of the cursor table has the same effect as changing `database`.

## Removal and Erase

Removing the sink with `StateIntent::Erase` deletes its rows:

```sql
DELETE FROM dbo.drasi_delivery_progress WHERE owner = N'<owner>';
```

`owner` is the lowercase hex SHA-256 of `drasi.storedproc/owner/v1`, a NUL
byte, the graph ID, a NUL byte and the component ID. A missing table is
success and a repeated Erase deletes nothing. The table itself and other
owners' rows are never touched. `Preserve` keeps the rows, so a sink
recreated with the same graph and component ID resumes its cursor.

If the sink uses an identity binding and was not started in the current
process, Erase cannot authenticate; its error contains the exact `DELETE`
statement to run manually.

## Operational runbook

| Symptom | Cause | Action |
|---|---|---|
| Start fails: `CREATE TABLE permission denied` | Least-privilege login | Pre-create the table and grant DML (above) |
| Start fails: `lacks INSERT on the progress table` | Missing grant | Grant SELECT, INSERT, UPDATE, DELETE |
| `delivery stream capacity … exhausted` | More producer streams than `maxStreams` | Raise `maxStreams` (InPlace), or erase stale owners' rows |
| `operation gap` / `unfinished batch` / `receipt expired` | The cursor and the host's progress disagree (cursor rows deleted or restored, two sinks sharing an owner) | Stop the sink, inspect the owner's rows, delete the stream's row only if you accept replaying the batch |
| `payload conflicts with its receipt` | The same batch arrived with different content | Investigate the producer; do not delete the row blindly |
| `procedure changed the delivery transaction` | The procedure commits or rolls back | Remove transaction control from the procedure; the effect may already be committed |
| `the rendered command binds N parameters` | Over 2 098 values | Pass a JSON document with `{{param (json …)}}` |
| Repeated retryable errors (1205, 40613, I/O) | Contention or failover | Automatic within `retryBudgetMs`; the host retries afterwards |

Useful query:

```sql
SELECT owner, stream_key, sequence, operations, completed
FROM dbo.drasi_delivery_progress ORDER BY owner, stream_key;
```

## Mapping from the legacy reaction

| Legacy `reactions/storedproc-mssql` | Native |
|---|---|
| `connection.host`, `port`, `database` | `host`, `port`, `database` |
| `connection.user`, `password` | `user`, `password` (secret) |
| `encryption: on / off / not_supported` | `encryption: required / loginOnly / disabled` (default `required`) |
| `trust_server_certificate` (always trusted in legacy) | `trustServerCertificate` (default `false`), or `trustedCaPem` |
| `routes`, `default_template` | `routes`, `defaultTemplate` |
| `command_timeout_ms` (per command) | `commandTimeoutMs` (per round trip) |
| `retry_attempts` (blind command retry) | `retryBudgetMs` (classified, cursor-protected retry) |
| `identity_provider` | Host activation identity binding |
| — | `progressTable`, `maxStreams`, `deliveryGuarantee`, `connectTimeoutMs`, `outputs` |
| Query names in `routes` | Output aliases in `routes` (full alias or last dotted segment) |

Template changes:

* **Inline `{{json x}}` and `{{x}}` in SQL text are rejected.** This is an
  intentional tightening: legacy accepted `EXEC p '{{json after}}'`, which
  wrote row data into the SQL text. Use `{{param (json after)}}`.
* `query_id`, `query_name`, `input_alias` and `metadata` are gone; use
  `output.alias`, `output.component` and `output.port`.
* `data` (legacy update payload) is gone; use `before` and `after`.
* Legacy `operation` was `ADD`, `UPDATE`, `DELETE`. Native uses `ADD`,
  `REPLACE`, `PATCH`, `DELETE`; a PATCH's `after` holds only the properties
  it sets. Templates under `updated` receive both REPLACE and PATCH.
* New: `identity`, `element`, `sequence`, `deliveryId`.
