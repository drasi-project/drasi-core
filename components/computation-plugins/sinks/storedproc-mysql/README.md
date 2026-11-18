# Native MySQL stored procedures

`drasi-computation-sink-storedproc-mysql` provides `drasi.mysql/storedproc`
(implementation `1`, configuration version `1`, native ABI 5.0): it calls a
MySQL stored procedure for each standard graph change from a source,
transformer or query ([sink input contract](../../SINKS.md)).

- **Delivery:** an external consumer of the host `DeliveryRunner`. Each
  operation runs in its own MySQL transaction that commits the procedure
  effect together with this component's row in a destination cursor table
  (`drasi_delivery_progress`, format version 4), through the protocol shared
  with the PostgreSQL and SQL Server sinks
  ([storedproc-common](../../common/storedproc/README.md)). Replays and
  retries do not repeat committed effects, so delivery is exactly-once per
  operation. A procedure that commits or rolls back on its own is detected
  and refused (permanent); the cursor never advances outside the delivery
  transaction. With `onPermanentFailure: skip`, an undeliverable operation is
  recorded in `drasi_delivery_progress_dead_letter` atomically with the
  cursor advance.
- **Templates:** Handlebars with `{{param <path>}}` only, bound as positional
  `?` parameters; compiled at construction.
- **Connection:** one persistent session per activation, verified TLS by
  default (`sslMode: require`; plaintext only to loopback unless
  `allowRemotePlaintext`), and session retirement with a verified KILL before
  any retry. Each operation is one attempt; the graph retries transient
  failures (deadlocks, lock waits, lost sessions).
- **Configuration:** flat camelCase fields; `password` is the only secret.

```sh
cargo build -p drasi-computation-sink-storedproc-mysql --features dynamic-plugin
```

Minimal configuration:

```json
{
  "database": "application",
  "user": "drasi_procedures",
  "password": "<secret reference>",
  "defaultTemplate": {"added": {"template": "CALL add_person({{param after.id}}, {{param after.name}})"}}
}
```

- **[DESIGN.md](DESIGN.md)**: architecture, delivery profile with exact
  duplicate and loss windows, cursor protocol, retry classification,
  lifecycle, management, security model, limitations and legacy differences.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges,
  secrecy and update boundaries, examples, template reference, grants and the
  pre-created table, update matrix, Erase runbook and the legacy mapping.

Live tests (Podman): `make test-native-mysql` and
`cargo test -p drasi-computation-sink-storedproc-mysql --test live -- --ignored --test-threads=1`
(the shared protocol table, a lost COMMIT reply and an InnoDB deadlock).
