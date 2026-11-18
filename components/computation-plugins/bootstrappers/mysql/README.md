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

Every factory declares its bootstrap output, which the graph checks when the
provider's edge binds; see the
[native bootstrap provider standard](../NATIVE-BOOTSTRAP-PROVIDER-GUIDE.md).

The coordinated factories take **no configuration**. They pair with the native
MySQL source through its declared coordination protocol: `drasi.mysql/transactions`,
`drasi.mysql/shared-transactions` and `drasi.mysql/shared-changes` configured
with `startFrom: snapshot` declare (and require) one protocol each. Connect the
provider node to that source's `bootstrap` input. The source's coordinator
opens each query's provider from the source's own identity, stream, server id,
tables, keys and retention, and connects with the source's current activation
credentials; the graph refuses the provider on any other source, and refuses
an unpaired provider on a coordinated source, when the edge binds. Each query
loads only the selected tables whose label (the table name) it declares.

The `snapshot` factory takes one flat camelCase object: the same shared
settings as the native MySQL source (see the source's
[CONFIGURATION.md](../../sources/mysql/CONFIGURATION.md)) plus `labels`. Only
`password` and `tlsClientKeyPem` are secret; use secret references for them.
The served source (whose element identities and server id it uses) and its
stream come from the graph relation, not from configuration. It takes no
`sourceProgress`, requires an explicit `tables` allowlist and `startFrom: end`,
and performs no global read lock, binlog
query or replication registration. It reuses the same owned connection, bounded
binary cursor, key/value decoding and server-observed cleanup as coordinated
loading. It returns no source watermark or durable handover record. This
preserves standalone initial loading without treating a cursor sampled after
starting a read transaction as a gap-free CDC boundary. Use the coordinated
factories when initialization must hand over to replayable live changes.

Standalone settings support manual `tableKeys` and views under SELECT-only
credentials; a table or view without a primary key needs a `tableKeys` entry
(identity is never inferred from a column name). An optional factory-level `labels` array
restricts scanning to requested names in the configured allowlist; names outside
that allowlist are logged and excluded, and unrequested tables are not inspected.
An absent or empty `labels` array selects the whole allowlist. A query's
declared labels narrow the selection further; the configuration stays the
upper bound. Completed volatile
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
`tableKeys` supplies explicit graph key columns when the database
primary key is not the intended identity. Their actual values must be non-null;
nullable column declarations alone do not prevent loading.

Persistent initialization retains its versioned ownership metadata in the
query's borrowed, bounded bootstrap state. Completion metadata and the cursor
are committed by the query together. Memory row initialization does not fabricate
persistent metadata; restart requests volatile reinitialization instead.
Single/shared transaction and row handover identities remain distinct.

A snapshot boundary includes a constant-size proof of its binlog file's identity
(a hash of the file's format description event), so neither the provider nor
the source re-reads the file prefix, however large the file.
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
