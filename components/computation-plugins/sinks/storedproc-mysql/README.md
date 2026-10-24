# Native MySQL stored procedures

`drasi.mysql/storedproc` is an awaited native sink for query changes. It uses the
legacy family's command renderer, route precedence and typed parameter mapping,
with the shared directly owned MySQL connection/session implementation rather
than the legacy pool's detached cleanup paths.

Configure one secret-referenced `settings` object:

```json
{
  "hostname": "localhost",
  "port": 3306,
  "database": "application",
  "user": "procedure_user",
  "password": "resolved-by-the-host",
  "queries": ["people"],
  "default_template": {
    "added": {"template": "CALL add_person({{param after.id}}, {{param after.name}})"},
    "updated": {"template": "CALL update_person({{param after.id}}, {{param after.name}})"},
    "deleted": {"template": "CALL delete_person({{param before.id}})"}
  }
}
```

The snippet is the resolved object shape, not permission to commit credentials
or bypass host secret/literal validation.

The owned SQL connection layer supports MySQL 5.7.9 or newer, not MariaDB, with
`mysql_native_password` and `caching_sha2_password`.

For provider-driven authentication, bind the optional `identity` dependency to
the Host's typed `NativeIdentityResource` (`ResourceRole::Identity`). Static Host
callers can use `NativeFactory::create_component_with_identity`; canonical Host
and Server definitions use a `nativeIdentity` resource recipe referencing a
loaded identity-provider descriptor. Configured secret/environment references
are resolved on every activation. An injected provider takes precedence over a
configured provider, which takes precedence over literal credentials.

When identity is bound, `user` and `password` may be omitted. Construction performs
no credential lookup and inserts no dummy user. Each ordinary START obtains fresh
password/token credentials with exactly the configured `hostname` and `port`
context. Provider failures, certificates and oversized credentials fail before
native startup, with no stale-token or literal fallback. Acquired authentication
is transient, retained through accepted work and cleanup; it never replaces the
captured configuration returned by `configuration()`. Without an identity
binding, existing literal authentication is unchanged.

Routes resolve by full query ID, last dotted segment, then default. Empty
templates mean no command for that operation. Standard `query_id`, `query_name`,
`operation`, `timestamp`, `metadata`, `before`, `after` and `data` contexts are
preserved, including aggregation's UPDATE routing. Missing fields fail strictly.
Only `{{param ...}}` may emit dynamic SQL values; control blocks are permitted,
but raw output, inline `{{json ...}}`, partials and decorators are rejected.
Parameters use MySQL prepared statements, including exact unsigned integers,
NULL, Unicode and whole JSON objects/arrays.

Defaults: command timeout 30000 ms, connection timeout 5000 ms, shutdown timeout
10000 ms, protocol/result limit 8 MiB, and three connection retries. `ssl: true`
verifies the system CA and hostname; `tls_ca_pem` optionally supplies a bounded
CA certificate. Retries apply only to transient connection failures before
procedure execution. Submitted CALLs are never blindly repeated.

The sink is `Handled` only after MySQL completes the configured commands and all
result sets are consumed. It does **not** claim exactly-once effects or an atomic
effect/checkpoint transaction. Procedures can commit internally or affect
nontransactional tables; a failed command may already have effects. Use
idempotent destination logic when replay is possible. A procedure leaving an
open transaction is rejected, not reported as completed delivery.

The component retains an interrupted operation across cancellation and stop
timeouts. Stop must finish server-observed cleanup before restart. Rendering
and execution failures propagate rather than inheriting the legacy reaction's
log-and-skip loop. All commands in an input are rendered/validated before the
first effect; an empty/no-op input performs no procedure work.

Cleanup requires recorded account-scoped connection attributes. Before KILL,
the same control connection verifies the operation's random per-connection
handshake marker. Every reconnect redoes that proof, so a server restart cannot
turn a reused numeric ID into permission to kill another connection. Missing,
denied or mismatched proof is an error, never a UUID/ID-only fallback.

```sh
cargo build -p drasi-computation-sink-storedproc-mysql --features dynamic-plugin
```
