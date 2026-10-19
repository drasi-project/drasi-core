# SQLite common mapping

Shared SQL parameter/REST/table-key configuration, bounded schema discovery,
SQLite value conversion and native graph identity mapping for the SQLite source
and its native bootstrap providers.

Primary/configured keys use `sqlite:v1:` followed by the table and a sorted map
of column names to SQLite type/value pairs. Physical-rowid identities use
`sqlite:rowid:v1:` followed by the table and actual rowid. User columns that
shadow SQLite rowid aliases are never mistaken for the physical identity.
These current-version formats must remain consistent across capture,
standalone file bootstrap and coordinated source snapshots.

`read_table` streams through a callback and respects early termination; it does
not collect rows or create a worker. Connection ownership, read transactions,
limits, cancellation, source replay and query handover belong to its callers.
The crate depends on shared core values and SQLite/data utilities, not the query
runtime, evaluator or either plugin SDK.
