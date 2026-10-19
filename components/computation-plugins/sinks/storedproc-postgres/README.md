# Native PostgreSQL stored-procedure sink

`drasi-computation-sink-storedproc-postgres` exports only
`drasi.postgres/storedproc`, implementation/configuration version `1`, through
native ComputationGraph ABI `1.0.0` and the consumer extension. Its `in` port
accepts `QueryChangeCodec` rows. It runs inside the hosting DrasiLib instance's
single graph, statically or as a separately loaded library.

The sink reuses the legacy parameter-only Handlebars renderer, template routing
and typed SQL binding, plus the existing `PostgresDeliveryHandler` destination
transaction/cursor contract. These shared helpers live in
`drasi-postgres-common`; the native library does not depend on `drasi-lib`, the
query engine or the legacy SDK.

## Completion and ownership

The factory declares `ConsumerMode::External`, **not** a host-storage transaction
for SQL effects. The host owns `DeliveryRunner`, persistent local progress,
bounded retry and upstream retention. PostgreSQL atomically commits each
operation's effect and its destination cursor. Successful delivery means that
commit completed, not that a background queue accepted the command.

Construct through `NativeFactory::create_consumer`, supplying the actual index
provider, stable instance/graph/component scope and `DeliveryOptions`.
Graph-factory construction instead requires a `consumer` resource holding
`NativeConsumerResource { provider, options }`. Ordinary unbound construction
rejects. Both static `NativePlugin::from_definition(plugin()?)` and dynamic
`computation::load(path)` expose the same factory.

The destination table `public.drasi_delivery_stream_v1` retains one bounded
cursor per consumer/producer stream, at most 4,096 streams. Retrying committed
operations does not repeat their effects, even if a commit reply or local receipt
was lost. Old batches after rollover, conflicting contents, operation gaps,
unfinished prefixes and damaged receipts reject. Do not manually alter or delete
this table. Replacements must preserve their logical identity and destination
receipts.

Procedures must use only the supplied PostgreSQL transaction. They must not
commit/roll back it, change the reserved receipt table, issue effects through
another connection, or invoke external services. Atomicity is per operation, not
across all operations in a query batch or the complete source transaction.

## Configuration

The factory takes one secret `settings` object. Its configuration getter retains
the full normalized object, including credentials; persist it using a secret
resolver reference.

| Field | Meaning |
|---|---|
| `hostname`, `port` | Defaults: `localhost`, `5432`. |
| `database`, `user`, `password` | Database and user are required; password defaults to empty. Values use the PostgreSQL configuration API, not a connection-string interpolation. |
| `ssl` | Default `false`; `true` requires verified server TLS. |
| `tls_ca_pem` | Optional additional trusted CA; hostname verification remains enabled. |
| `tls_client_cert_pem`, `tls_client_key_pem` | Optional PEM certificate/key pair for mutual TLS; both required together with `ssl=true`. Each PEM field is bounded to 64 KiB. |
| `queries` | Required nonempty, unique list of accepted query IDs. Unexpected query IDs fail. |
| `routes`, `default_template` | Shared `QueryConfig`/`TemplateSpec` command templates. At least one nonempty command is required. |
| `command_timeout_ms` | Positive, default 30,000; also sets PostgreSQL `statement_timeout`. |
| `connect_timeout_ms`, `shutdown_timeout_ms` | Positive, default 30,000 each. |

Routing is full query ID, last dotted segment, then default, resolved separately
for ADD/UPDATE/DELETE. Aggregations use the UPDATE template as in the legacy
reaction. Context keys are `query_id`, `query_name`, `operation`, RFC3339
`timestamp`, `metadata`, and applicable `before`, `after`, `data`.

```json
{
  "added": {"template": "CALL add_user({{param after.id}}, {{param after.name}})"},
  "updated": {"template": "CALL change_user({{param before.id}}, {{param after.name}})"},
  "deleted": {"template": "CALL delete_user({{param before.id}})"}
}
```

Only `{{param ...}}` may emit dynamic SQL. Bare/raw substitutions, other emitting
helpers, partials and decorators reject during construction. A whole object binds
as JSONB; text such as `"007"` remains text unless its actual target is numeric.
Integer narrowing rejects overflow rather than truncating. Render/type/SQL errors
propagate and retain unfinished work; there is no log-and-skip error fallback.
An absent/empty operation template or a false configured command condition is an
explicit no-command policy and is logged at debug level.

Only PostgreSQL SQLSTATE `40001`, `40P01` and `55P03` are retryable when the host's
bounded `DeliveryOptions.retry` permits it. Commit failures are uncertain, not
safe fresh operations. Command cancellation/timeout requires awaited cleanup and
reconstruction using the same identity. The sink owns its connection driver;
cancelled/timed-out cleanup retains that owner and forbids a replacement until
joining completes.

## Build and qualify

```sh
cargo build -p drasi-computation-sink-storedproc-postgres --features dynamic-plugin
make build-native-postgres-plugins
cargo test -p drasi-source-postgres --test native_transactions libraries
cargo test -p drasi-reaction-storedproc-postgres --test delivery
```

The native-library cases use actual PostgreSQL/RocksDB and separately built
libraries. They cover static/dynamic end-to-end delivery, parameter safety, TLS,
restart, partial batches and destination deduplication. The existing destination
handler suite retains its cancellation, concurrent duplicate, receipt-bound,
corruption and database-crash assertions against the same implementation.
