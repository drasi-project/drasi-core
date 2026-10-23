# drasi-bootstrap-salesforce

Bootstrap provider for durable Salesforce source snapshots.

## Design

This bootstrapper **does not call Salesforce**.

It reads source-produced immutable local snapshots from `spool_root` using a
validated snapshot ticket provided via bootstrap context properties.

## Required bootstrap context property

- Key: `salesforce.bootstrap_ticket.v2`
- Value: JSON-serialized `SnapshotTicket` (version `2`)

Ticket/manifest binding includes:

- source identity (`source_id`, `source_identity`, `org_id`)
- object scope (`objects`)
- snapshot boundary (`epoch`, `head_ordinal`)
- manifest path and checksum

## Snapshot rows format

Rows are a framed `GraphRecord` stream:

- 4-byte big-endian payload length (`u32`)
- msgpack payload (`GraphRecord`)

The provider validates:

- ticket version/source/expiry/path/checksum shape
- manifest checksum and identity/object binding
- rows checksum
- row count and per-record byte limits
- framed stream integrity (partial header/body rejected)
- record object scope and record ID validity
- path safety including leaf-symlink escape rejection

Returns bootstrap `source_position` at snapshot head (`epoch + ordinal`).

Emission order is deterministic:

1. all node inserts
2. all relation inserts

from the same immutable rows file (two-pass read, bounded memory).

## Configuration

Builder options:

- `spool_root` (required)
- `max_rows` (default `100000`)
- `max_row_bytes` (default `1048576`, per framed `GraphRecord` payload)

## Tests

```bash
cargo test -p drasi-bootstrap-salesforce
```

Coverage includes:

- successful snapshot load + boundary return
- unlabeled-endpoint preservation
- checksum mismatch rejection
- high-bit epoch context round-trip
- oversized manifest/rows rejection before decode
