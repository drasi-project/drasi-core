# Native SQL Server bootstrap providers

**Live SQL Server qualification is explicitly deferred by the user (CW-78);
this family is verified by unit tests and database-free Host checks only.**

One bootstrap-role crate exports two explicit factories, both through the real
static SDK and the native ABI 4.0 dynamic library:

| Factory | Permissions and behavior |
|---|---|
| `drasi.mssql/snapshot` | Standalone **read-only** table snapshot. Explicit `tables`, primary/custom keys, SELECT/catalog visibility and the database's SNAPSHOT-isolation option. No CDC catalogs, marker, DDL or data writes. Returns **no CDC handover watermark**. |
| `drasi.mssql/bootstrap` | Opt-in coordinated CDC snapshot. Requires actual query progress and an explicitly configured, administrator-provisioned CDC `barrierTable`. Writes and deletes its own marker and holds shared table locks until snapshot COMMIT. |

## Configuration

Both take one flat camelCase object: `source` (the paired source's component
ID), `stream`, optional `labels` and optional `stopTimeoutMs` (how long
`stop` waits for snapshot cleanup, default `30000`), beside **the same
settings the native source accepts** (see the source's
[CONFIGURATION.md](../../sources/mssql/CONFIGURATION.md)). `password` is the
only secret. Bootstrap factories receive no activation credentials, so they
always use `user` and `password`; encryption is required by default, as for
the source. Read-only mode requires `barrierTable` to be absent and
`tables` to be explicit; coordinated mode requires `barrierTable`.

```json
{"source": "585da1dd-2d7d-5cec-bcf7-e84ac13a88f8", "stream": "orders/out",
 "labels": ["Orders"], "host": "sql.internal", "database": "shop",
 "user": "drasi_cdc", "password": {"secret": "mssql-password"},
 "tables": ["dbo.Orders"], "barrierTable": "drasi.Barrier"}
```

Schema-qualified table identities and
type conversion agree with the source. Unknown labels produce no rows, without releasing the coordinated table fence.
Rows flow through an owned, row/byte-bounded channel with backpressure. `maxRows`
limits queued rows and each server-cursor FETCH batch, **not total snapshot size**.
`maxBytes` counts actual serialized graph-record payload bytes, not Debug output;
one oversized individual record fails rather than being truncated. A worker may
also hold the currently decoded row and driver/catalog state.

Both modes use a session-owned forward-only SQL cursor and bounded FETCH batches.
Cancellation requests cleanup without dropping a pending SQL future; stop can
time out while that owned work remains. Cleanup closes/deallocates the cursor,
rolls back any unfinished transaction and, in coordinated mode, deletes only the
owned marker. Unconfirmed cleanup remains an error on subsequent stop attempts.
Streams are generation-fenced. **No watermark is returned at stream startup**:
only `complete_snapshot`, after EOF, successful commit and confirmed cleanup,
returns the real CDC cut. Standalone mode never returns a CDC cut.

## Why the modes are separate

The old bootstrapper opened a SNAPSHOT transaction and sampled
`sys.fn_cdc_get_max_lsn()`. SQL Server CDC capture is asynchronous: that number
does **not** prove that every row visible in the snapshot is captured, even when
read inside the transaction. Returning it as an exact handover could overlap or
lose updates. The standalone factory preserves the useful read-only snapshot
without repeating that unsupported handover claim.

The coordinated algorithm:

1. Discover and validate all selected tables/capture/key definitions.
2. Begin SERIALIZABLE and take `TABLOCK,HOLDLOCK` read locks on **every selected
   source table**, including tables excluded by a query's labels.
3. Through a separate connection, commit a unique marker into the explicitly
   provisioned CDC marker table, which must not be a selected source table.
4. Wait for that exact INSERT to appear in its actual CDC capture table.
5. Read selected rows while the table locks remain held, verify metadata, commit
   the snapshot transaction, and delete only the owned marker.

Those locks now remain held while downstream consumers apply backpressure.
There is no default total-row/dataset cap; administrators still must plan for
writer blocking and sufficient CDC retention during large coordinated snapshots.

The proposed cut depends on **both** the retained table locks and the captured
marker's commit ordering—not the marker/LSN alone. Table writers can block for
the bounded snapshot duration, unlike the old nonblocking SNAPSHOT reads.
No plugin performs CREATE/ALTER/CDC administration, and normal credentials need
no sysadmin or KILL privileges. A DBA provisions CDC and a table such as
`dbo.DrasiBarrier(token uniqueidentifier primary key)` once; the application
needs INSERT/DELETE on it plus appropriate selected/CDC-table SELECT.

There is no public DrasiLib SQL API or general SQL service.

### Marker ownership (decision D-1)

Each marker's `token` starts with 64 bits derived from the graph and the
paired **source** (`SHA-256("drasi.mssql/marker/v1", graph, source)`),
followed by 64 random bits, so every marker belongs to exactly one source.
The provider deletes its marker after each attempt. A process crash or a
failed cleanup may leave one behind; **the source owns those markers** and
deletes every marker with its prefix on erasing removal, idempotently. The
providers do not sweep: they have no erase hook, and a sweep at snapshot
start could delete the marker of another snapshot still in progress. Do not
configure a retention cleanup policy that could delete an in-flight marker.

## Qualification

**Not qualified: live qualification is deferred by the user (CW-78).**
Static/dynamic metadata, configuration and shared decoder tests are not proof
of concurrent snapshot correctness. Concurrent writers,
multi-table transactions, lock timeout/rollback, marker cleanup failure, stream
cancellation, and read-only least-privilege behavior require real-server
qualification. A read-only snapshot must never be advertised as coordinated
snapshot/live delivery.
