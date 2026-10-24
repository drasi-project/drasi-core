# Native MySQL bootstrap

This bootstrap-only library reuses the existing MySQL snapshot protocol in
`drasi-mysql-common`. It opens no connection at construction and creates no
source, private query, graph or forwarding service.

Standalone SQL loading supports MySQL 5.7.9 or newer; coordinated loading retains
its MySQL 8.0.20/FULL-metadata/commit-clock requirements. MariaDB is not claimed.
The owned connection layer supports `mysql_native_password` and
`caching_sha2_password`.

| Factory | Query/source contract |
|---|---|
| `drasi.mysql/coordinated-bootstrap` | One persistent transaction-query owner |
| `drasi.mysql/shared-transaction-bootstrap` | One actual member of a shared transaction source |
| `drasi.mysql/shared-row-bootstrap` | One actual row-query owner, including memory queries |
| `drasi.mysql/snapshot` | Query-scoped repeatable-read snapshot without a replication or handover claim |

All factories require `source`, `stream`, secret-referenced `settings` and query
scope. Coordinated factories also require the actual query's `sourceProgress`
capability. Bind the provider to that query's single bootstrap slot, not to a
fake owner. Use the same source/stream/settings as the live native source and
explicitly select snapshot locking mode.

The `snapshot` factory is the exception to the progress requirement: it takes no
`sourceProgress`, requires an explicit table allowlist, `output: changes`,
`replay: null` and `start: {mode: end}`, and performs no global read lock, binlog
query or replication registration. It reuses the same owned connection, bounded
binary cursor, key/value decoding and server-observed cleanup as coordinated
loading. It returns no source watermark or durable handover record. This
preserves standalone initial loading without treating a cursor sampled after
starting a read transaction as a gap-free CDC boundary. Use the coordinated
factories when initialization must hand over to replayable live changes.

Standalone settings support manual `table_keys`, the non-PK `id` fallback and
views under SELECT-only credentials. An optional factory-level `labels` array
restricts scanning to requested names in the configured allowlist; names outside
that allowlist are logged and excluded, and unrequested tables are not inspected.
An absent or empty `labels` array selects the whole allowlist. Completed volatile
queries refresh their snapshot on restart; persistent query initialization does
not silently discard committed state. Nontransactional tables can be scanned,
but repeatable-read does not make their data transactionally consistent.
SQL cursor text is converted by MySQL to the negotiated UTF-8 encoding, so
standalone loading does not incorrectly impose a raw-binlog UTF-8-only column
restriction on tables such as MySQL 5.7's default latin1 tables.

The coordinated implementation samples the server clock before establishing
the snapshot, uses `FLUSH TABLES WITH READ LOCK` to establish a consistent InnoDB view and
binlog boundary, and releases the global lock before scanning. It pins selected
table definitions and streams through bounded binary server cursors. Initial
and live rows share typed `mysql:v1:` identities and value conversions.
`settings.table_keys` supplies explicit graph key columns when the database
primary key is not the intended identity. Their actual values must be non-null;
nullable column declarations alone do not prevent loading.

Persistent initialization retains its versioned ownership metadata in the
query's borrowed, bounded bootstrap state. Completion metadata and the cursor
are committed by the query together. Memory row initialization does not fabricate
persistent metadata; restart requests volatile reinitialization instead.
Single/shared transaction and row handover identities remain distinct.

A snapshot boundary includes a bounded-memory hash of the actual binlog prefix.
The source verifies retained history before admitting live changes. Keep the
anchor file and subsequent history; missing or reused history is an error.

Cancellation drops the stream and signals its owned reader. Stop retains and
joins cleanup, including server-observed retirement of this operation's actual
sessions. It never kills an unrelated lock holder. The lock timeout is not a
hard bound on an unrelated application's pause: MySQL can retain a waiting
`FLUSH TABLES` until a pre-existing read finishes.

Owned SQL sessions require recorded connection attributes in
`performance_schema.session_account_connect_attrs`. The account-scoped table
works with SELECT-only snapshot accounts; access to all users' attributes is not
required. Every KILL is preceded on the same control connection by a check of
the connection's random handshake marker. A reconnect must recheck it: a
persistent server UUID and reused numeric connection ID are not ownership proof.
Missing, denied or mismatched proof fails closed and retains cleanup obligations.

MyISAM/nontransactional initial data is not silently treated as a consistent
InnoDB snapshot. Row streaming of logged nontransactional changes is separate.
Native bootstrap is initialization control, not handled-input progress.

```sh
cargo build -p drasi-computation-bootstrap-mysql --features dynamic-plugin
```

Static and separately built dynamic hosting use the same native SDK contract
inside the hosting DrasiLib instance.
