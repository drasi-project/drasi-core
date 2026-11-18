# Native SQL Server stored-procedure sink

**Live SQL Server qualification is deferred by the user (CW-78).** The sink is
verified by unit tests, construction tests and an in-memory model of its
cursor protocol only. The live scenarios in
`components/host-sdk/tests/native_mssql_components.rs` compile and stay
ignored; do not seek or run a database without renewed approval.

`drasi.mssql/storedproc` is a native ComputationGraph sink for the
computation plugin ABI 5.0.0. `plugin()` and the gated dynamic entry point
(`dynamic-plugin` feature) export the same factory.

It is an external consumer of the host `DeliveryRunner`. Each change runs in
its own SQL Server transaction that executes the rendered procedure call and
advances this sink's row in a destination cursor table, so a change takes
effect exactly once when the procedure does not commit on its own.

```json
{
  "host": "sql.example.internal",
  "database": "orders",
  "user": "drasi_sink",
  "password": {"secretRef": "orders-sink-password"},
  "outputs": {"orders": {"component": "bc769198-8a46-426c-acb5-435b084520d0", "port": "out"}},
  "defaultTemplate": {
    "added":   {"template": "EXEC dbo.UpsertOrder {{param after.id}}, {{param (json after)}}"},
    "updated": {"template": "EXEC dbo.UpsertOrder {{param after.id}}, {{param (json after)}}"},
    "deleted": {"template": "EXEC dbo.DeleteOrder {{param before.id}}"}
  }
}
```

* [`DESIGN.md`](DESIGN.md): delivery profile, cursor protocol, exactly-once
  conditions, retry classification, lifecycle, security.
* [`CONFIGURATION.md`](CONFIGURATION.md): every field, templates, grants,
  authentication, update matrix, Erase, runbook and the legacy mapping.

## Capabilities

| Capability | Native behaviour |
|---|---|
| Delivery profile | `ConsumerMode::External` plus a destination cursor (`progressTable`, format 4) committed in the same transaction as each change's procedure call, through the protocol shared with the PostgreSQL and MySQL sinks ([storedproc-common](../../common/storedproc/README.md)). Exactly once per operation. A procedure that commits or rolls back is refused permanently and never run again (no duplicate on replay). `onPermanentFailure: skip` records an undeliverable operation in the dead-letter table atomically with the cursor advance. |
| Templates | Parameter-only: every value goes through `{{param …}}` and is bound as `@P1…@Pn`. Inline `{{json}}`, `{{x}}`, `{{{x}}}`, partials, decorators and other block helpers are rejected at construction. Compiled once; strict rendering. |
| Routing | Full output alias, then its last dotted segment, then `defaultTemplate`, per operation. |
| Context | Truthful change: `operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`), `output`, `sequence`, `timestamp`, `identity`, and only the `element`, `before`, `after`, `removedProperties`, `effectiveFrom` the record carries; plus `deliveryId`. |
| Values | Exact positional binding; unsigned values beyond `BIGINT` bind as decimal text; objects and arrays as JSON text; at most 2 098 values per command. |
| Large inputs | No input size limit; rendering per change; timeouts per server round trip. |
| Retry | Classified by SQL Server error number and error type; one attempt per operation, retried by the graph; a lost commit reply is resolved through the cursor; a definite commit error is classified by its number. No latched failure state. |
| Connection | One persistent connection per activation, reconnected on demand. |
| Lifecycle | Start honours cancellation and the deadline and creates or checks the progress and dead-letter tables; host-driven pause; Drain closes gracefully within `shutdownTimeoutMs`, Abort drops; Erase deletes only this sink's rows. |
| Authentication | SQL authentication; activation credentials (SQL login or Microsoft Entra ID access token as Tiberius `AADToken`). The Host does not yet bind identities to consumers. |
| TLS | Required by default with certificate verification; extra CA via `trustedCaPem`; plaintext and `trustServerCertificate` are explicit opt-ins with warnings. |
| Management | `Factory::assess` and `NativeManagement`; every field classified (InPlace, Restart, Recreate, or Unsupported for `host`, `port`, `database` and `progressTable`). |
| Configuration | Flat camelCase, unknown fields rejected, only `password` secret, published defaults, redacting `Debug`. |
| Cancellation | Tiberius cannot send ATTENTION; an interrupted statement drops the connection and SQL Server rolls the transaction back. |

## Features

* `native-tls` (default) or `rustls`: TLS backend for Tiberius.
* `dynamic-plugin`: export the C ABI entry point for the cdylib.
