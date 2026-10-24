# Native SQL Server stored-procedure sink

**User-approved deferral: SQL Server runtime qualification remains incomplete.**

`drasi.mssql/storedproc` is a native sink, not a legacy reaction queue or adapter.
`plugin()` and the gated dynamic entrypoint use the same implementation with
computation ABI 1.0.0 / wire 2.

The required secret-bearing `settings` object contains:

* `connection`: the shared SQL Server connection configuration;
* `queries`: explicit accepted query IDs;
* `routes` and/or `default_template`: existing `QueryConfig` / `TemplateSpec`;
* `command_timeout_ms`, `shutdown_timeout_ms` (default 30000, positive/bounded).

For example:

```json
{
  "queries": ["all"],
  "default_template": {
    "added": {"template":"EXEC dbo.ApplyEffect {{param operation}},{{param after.id}}"},
    "updated": {"template":"EXEC dbo.ApplyEffect {{param operation}},{{param after.id}}"},
    "deleted": {"template":"EXEC dbo.ApplyEffect {{param operation}},{{param before.id}}"}
  }
}
```

Add the connection through a Host secret/configuration reference rather than
putting literal credentials in a persisted Server definition.

## Preserved behavior and intentional safety changes

| Capability | Native behavior |
|---|---|
| Rendering/routing | The extracted legacy `mssql-common::sql` implementation, including unchanged original renderer assertions. Full query ID → last dotted segment → per-operation default. |
| Contexts | ADD/UPDATE/DELETE, aggregation before/after, query ID/name, timestamp and metadata. Noop/unconfigured operation/empty rendered command emits nothing. |
| Values | Strict missing fields; exact positional Tiberius binding. Unsigned values beyond i64 bind as decimal text; null, Unicode, JSON objects/arrays preserve renderer semantics. |
| Execution | Pre-render the entire input before any effect; execute commands in order and drain every result set. |
| Errors | Render, connect and procedure errors propagate. No legacy log-and-drop while reporting success. |
| Completion | `Handled` only after the command responses are drained. This is **not** durable deduplication, transactional delivery or once-only effects. Earlier commands may commit before a later command fails. |
| Retry | No blind `retry_attempts`; rejected as unsupported. Legacy retried potentially executed commands whose effects were unknown. Removing that behavior is a safety correction, not a claim to preserve unsafe retry semantics. |
| Stop/cancel | SQL work owns its connection and remains retained when its caller is cancelled or times out. Stop waits; failed/timed-out cleanup remains failure. Tiberius exposes no cancellation method, and socket drop/CLOSED does not prove a server-side procedure stopped. No sysadmin/KILL required. |
| Authentication | Optional real Host `identity` binding uses the activation-credentials extension. The configured hostname/port are supplied to the provider; each start refreshes credentials, overrides literals for that activation only, and leaves declared configuration immutable. Empty literals require an actual binding. Provider errors never fall back to literals; certificates are rejected by Host. Token values preserve the legacy provider-to-SQL-auth-pair behavior, **not** an Azure AD protocol claim. |
| TLS | Shared native-tls/rustls and encryption modes; trust-server-certificate is explicit, not silently forced as in the old reaction. |
| Recovery snapshots | Rejected; this sink does not support arbitrary destination replacement. Cross-process replay may repeat effects and needs application idempotency. |

**Real SQL Server qualification is blocked on the current ARM64 Podman host.**
Unit renderer assertions and successful static/dynamic loading are not database
parameter-effect, rollback, cancellation or timeout proof. The real fixture fails
explicitly on unavailable SQL Server rather than skipping. Exact parameter edge
effects and server-observed cleanup still need a working SQL Server environment.
The user has explicitly deferred that work; do not seek or run another database
without renewed approval.
