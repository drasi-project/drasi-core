# drasi-source-salesforce

One durable Salesforce source discovers the fields and lookup relationships of
the objects you select. No field lists, per-object sources or virtual joins are
required.

```json
{
  "connection": {
    "instanceUrl": "https://YOUR-ORG.my.salesforce.com",
    "expectedOrgId": "YOUR_ORG_ID",
    "apiVersion": "68.0",
    "auth": {
      "type": "clientCredentials",
      "tokenUrl": "https://YOUR-ORG.my.salesforce.com/services/oauth2/token",
      "clientId": {"kind": "EnvironmentVariable", "name": "SALESFORCE_CLIENT_ID"},
      "clientSecret": {"kind": "Secret", "name": "salesforce-client-secret"}
    }
  },
  "objects": ["Account", "Contact"],
  "spoolRoot": "/var/lib/drasi/salesforce/crm"
}
```

This is the dynamic descriptor's configuration; its version is `2.0.0`.
An embedded application can deserialize the corresponding concrete
`SalesforceSourceConfig`, inject a Token identity if needed, and build:

```rust,ignore
let source = SalesforceSource::builder("crm")
    .with_config(config)
    .build()?;
let query = Query::cypher("contacts-with-accounts")
    .query("MATCH (c:Contact)-[:REL_Account]->(a:Account)
            RETURN c.Name AS contact, a.Name AS account")
    .from_source("crm")
    .enable_bootstrap(true)
    .build();
```

The source installs its local snapshot bootstrap provider automatically. The
provider runs inside the source, including when the source is a dynamic plugin;
there is no separate Salesforce bootstrap plugin or Salesforce-specific FFI
contract.
An explicit `with_bootstrap_provider(...)` override is available for Rust callers.
Persistent queries also need a
durable query index backend; the source spool does not replace query storage.

Dynamic loading uses **source subscription ABI 0.15**. Its
separate resume-sequence presence flag preserves `Some(0)` instead of treating it
as `None`. Build the host and plugins with matching ABI major/minor versions;
metadata-bearing ABI 0.14 plugins must be rebuilt before loading into a 0.15 host.
This does not change the Salesforce spool or snapshot formats.

## Query schema

Describe metadata supplies the schema visible to the integration identity:

- Each selected object's API name becomes its node label.
- Supported visible fields become properties with their exact API names.
  `Id` is always present. Formula and REST-only fields are included.
- Lookup fields remain scalar ID properties and also create directed relations
  to selected target objects. Relation labels are `REL_` followed by Salesforce's
  `relationshipName`. If a selected object name already starts with `REL_`,
  underscores are appended to the prefix until no selected object starts with it.
  This keeps all relationship labels distinct from node labels.
- Unsupported fields and references to unselected targets are reported
  explicitly. Selecting objects does not recursively ingest their targets.

Inspect the schema without starting ingestion or changing local/cloud state:

```sh
cd examples/lib/salesforce-getting-started
./quickstart.sh --org drasi-dev --objects Account Contact --schema
```

`--schema` writes the discovered schema as JSON to stdout. This is a read-only
preview of current Salesforce metadata, not a claim that an existing source has
finished loading it.

For a registered source, use Drasi's standard graph inspection APIs without
downcasting:

```rust,ignore
let schema = drasi.get_source_schema("crm").await?;
let graph = drasi.get_graph_schema().await?;
```

The source implements synchronous `Source::describe_schema()` for both embedded
and dynamically loaded plugins. Inspection reads an in-memory snapshot: it never
contacts Salesforce or reads the spool. A newly constructed source returns `None`
until discovery, durable catalog persistence and report publication succeed.
Successful metadata refreshes replace the whole snapshot. Stopping the source or
failing a refresh/restart retains its last successfully published schema;
deprovisioning its owned storage clears it. Schema presence does not imply
`Running`, initial-scan completion, or completed reprojection.

Generic schemas include the actual projected properties and lookup relations.
Date/time properties have String hints, datetimes Timestamp, and multiselect
lists Json with a description of their shape. Relations include the String
properties `Field`, `TargetId` and `TargetObject`. A label shared by multiple
owners or selected targets leaves the corresponding `from` or `to` unspecified,
rather than picking an arbitrary endpoint. Rich nullability and omission details
remain in the Salesforce-specific report.

An embedded running source exposes `source.schema().await?`. The source also
publishes `<spoolRoot>/schema.json`, including source identity, epoch, journal head,
active catalog fingerprint and `pending_records`. The latter counts existing
mirrored records still using an older catalog; zero does **not** prove that the
first upstream scan is complete. The file is an as-of-head report; the accessor
reads current durable metadata. `Source.properties()` remains configuration only.
If the durable catalog commits but report publication fails, this diagnostic
accessor can reflect a newer catalog than the generic last-published snapshot;
the source reports the failure rather than advertising a successful refresh.

Metadata is discovered at startup and refreshed every five minutes. A query
rejected with Salesforce `INVALID_FIELD` also requests immediate rediscovery,
so removed fields do not require a source restart. Transient discovery failures
retry after five seconds; an unchanged catalog that still cannot support the
projection fails explicitly rather than retrying indefinitely. Changes
schedule complete reprojection, preserving the journal and independently resumed
queries. Stale observations are rejected. Removed properties receive explicit
null clears; removed or renamed lookups delete their old edges. Reprojection is
per record, not an atomic org-wide schema/data transaction. Queries depending on
changed field types may need corresponding query changes.

### Joining Contacts to Accounts

With `objects: ["Account", "Contact"]`:

```cypher
MATCH (c:Contact)-[:REL_Account]->(a:Account)
RETURN c.Id AS contactId, c.Name AS contact, a.Id AS accountId, a.Name AS account
```

`Contact.AccountId` supplies the edge, and its described relationship name
`Account` produces `:REL_Account`. Likewise, `Account.ParentId` produces
`:REL_Parent` and `Contact.ReportsToId` produces `:REL_ReportsTo` when those
targets are selected. Custom lookups use the same prefix with their described
relationship names.

Use explicit node labels for record-only queries. The unchanged engine can
include relationships in an unlabelled single-node pattern such as `MATCH (n)`.
The source does not change that engine behavior.

Earlier v2 spools remain readable. The label change updates the catalog
fingerprint and schedules normal per-record reprojection, deleting old edges
before inserting their namespaced replacements with the same element IDs.
Update existing relationship patterns to the names shown by schema inspection.

A null lookup removes the edge. Reparenting deletes the old edge before inserting
its replacement. Deleting a referring record removes its outgoing edges.
An edge may arrive before its target, or remain while the target is absent; it
matches no complete path until both endpoints are present. No placeholder nodes
are created. Polymorphic references resolve only through unambiguous declared
target key prefixes. Missing relationship names never get guessed labels.

Node IDs are `sf:<org>:<object>:<canonical-id18>`. Relation IDs are
`sfrel:<org>:<object>:<owner-id18>:<lookup-field>`, so reparenting preserves the
relation's identity. Relation observation time follows its owner record.

### Field mappings

| Salesforce field type | Drasi property |
| --- | --- |
| Text, textarea, email, phone, URL, picklist, ID, reference | String |
| Checkbox | Boolean |
| `int` | Integer |
| Double, currency, percent | Finite binary64 float |
| Date | `YYYY-MM-DD` string |
| Time | `HH:mm:ss.SSS` string |
| DateTime | Zoned datetime |
| Multi-select picklist | Sorted list of strings |
| Nullable field with no value | Null |
| Address/location compound parent | Omitted; supported scalar components are included |
| Binary/base64 or unknown type | Omitted with a diagnostic |

Standard address components such as `BillingStreet` and `MailingCity` are
discovered automatically. A scalar field's `compoundFieldName` does not exclude
it: `Account.Name` remains a normal property. Numbers are not arbitrary-precision
decimals; currency/percent values are not converted or rescaled. Encrypted values
reflect the authenticated user's permissions, including masking. Fields absent
from Describe cannot be enumerated or advertised as loaded.

## Observation and convergence

OAuth token and instance URLs require HTTPS on `*.salesforce.com`, except for
loopback HTTP/HTTPS fixtures. Embedded URL credentials, queries, and fragments
are rejected. This includes production, sandbox, and My Domain Salesforce hosts;
arbitrary OAuth proxy hosts are not accepted.

Pub/Sub endpoints must use HTTPS. Plain HTTP is permitted only for loopback
protocol fixtures, and invalid endpoints are rejected during configuration
validation and before connecting. Resolved OAuth client secrets are redacted in
`Debug` output, including enclosing source configurations; serialization still
preserves the configuration.

Each object's native Pub/Sub feed invalidates record IDs; REST fetches complete
current values. Periodic complete scans repair missed hints and offline changes.
Scans continue while native Subscribe response headers are pending.

Unavailable/non-subscribable object topics are explicitly logged as **REST-only**;
topic discovery retries without changing Salesforce settings. Authentication and
permission failures are errors, not silent polling fallbacks. Enabling CDC remains
an administrator action. No metadata, OAuth application or permissions are
provisioned by this source.

Object observers serialize their own reads and share one durable journal and
commit/dispatch gate. A successful scan can remove only that object's missing
records; failed or partial scans cannot infer deletion. Unchanged observations
do not emit events. Confirmed deletions are committed before upsert batches so
same-object replacements at `maxRows` do not require temporary extra row capacity.
Deleting an already-absent record is a no-op, including
create/delete hints that coalesce before the first observation. Dirty-ID budget
overflow requests a complete scan.

This is a **current-state convergence source**, not an audit log of every
Salesforce transition. Rapid changes can coalesce. Cross-object scans can reflect
different times. Atomic record/edge persistence does not imply atomic delivery
of several graph events or exactly-once external reaction side effects.

## Durability and recovery

One source-owned redb database atomically stores each record and its outgoing
edges, normalized journal events and consecutive local ordinals. The journal
head is the live sequence authority, independent of Salesforce replay retention.

Bootstrap captures one head H and exports bounded framed records from the same
read transaction. Ticket/context format v2 uses
`salesforce.bootstrap_ticket.v2`. The reader checks identity, expiry, version,
checksums, object scope, counts and byte limits before sending nodes, then
relations. It loads the configured graph rather than interpreting SDK label sets
as exhaustive filters: those sets do not describe mixed labelled/unlabelled
patterns. This matches live delivery and avoids missing required endpoints.

Finished readers release their snapshot lease and files on success or failure.
Cancelling the bootstrap caller does not release files still in use by a blocking
reader; the source retains that task until it finishes and cleans up. Ticket
expiry only reclaims abandoned, unread snapshots, never an active reader.

Each query independently receives snapshot H, replay `(H,R]`, then live `>R`.
Completed-bootstrap checkpoints pair cursor H with sequence **zero**, even for
nonempty snapshots. Nonzero sequences must equal the local cursor ordinal.
Missing, inconsistent or expired positions fail explicitly; there is no silent
reset to latest. Rust callers receive typed `PositionUnavailable` errors. The
unchanged plugin SDK reports subscription failures as generic errors, so typed
`AutoReset` handling of unavailable positions is limited to static linking.
Dynamic-plugin recovery failures require operator intervention.

Retention pins protect catchup and unconfirmed live events. Capacity pressure
cannot discard pinned history: the mutation fails atomically instead. Dropped
receivers release admission slots. Initialization, runtime workers and snapshot
export/reader tasks remain owned across caller cancellation. Stop joins them; a
timeout refuses restart/deprovision until the work is quiescent.

`maxSpoolBytes` bounds aggregate file lengths in the spool, including redb
allocation growth, ownership/schema files, and snapshot exports. Database and
export growth share one serialized budget check before extending files; the
limit is not just an estimate of serialized records. Capacity failures stop
ingestion without advancing the rejected transaction's journal head. Reopen the
source with more capacity to recover; do not delete its durable state. Filesystem
block overhead and writes by external processes are outside this source-owned
budget.

The v1 single-object `projection` configuration and spool are incompatible.
Use `objects` and a new v2 source/query state location. Old directories are
rejected without reinterpretation or deletion. Object-selection changes bind a
different source identity; ordinary discovered-field changes do not.

## Resource settings

| Setting | Default / scope |
| --- | --- |
| Selected objects | Required; 1-32 unique API names |
| `reconcileIntervalSeconds` | 5; native invalidation batching |
| `fullScanIntervalSeconds` | 60; mandatory REST repair |
| Metadata refresh | 300 seconds |
| Concurrent REST acquisition | At most four operations |
| `maxRows` | 100,000 across the source; also the shared dirty-ID budget |
| `maxEvents` / `retentionHours` | 1,000,000 / 96 |
| `maxProjectedRowBytes` | 1 MiB including owned edges |
| `maxScanBytes` | 8 MiB total response bytes per object fetch, across all ID batches and pages |
| `maxSpoolBytes` | 8 GiB aggregate journal/mirror/export budget |
| `maxReplayPageEvents` / `maxReplayPageBytes` | 100 / 1 MiB |
| `maxSubscribers` / `maxSnapshotLeases` | 16 / 16 |
| `snapshotTicketTtlSeconds` | 600 |
| Snapshot file hard limit | 512 MiB |

The source and its internal bootstrap reader use the same protected spool.
Do not share it between source owners or expose it through an unauthenticated
file service. Consider Salesforce API quotas when selecting objects and intervals.

## Testing and troubleshooting

Salesforce has no supported local emulator. Local HTTP/gRPC protocol fixtures
exercise the actual source, bootstrap, query runtime and persistent indexes;
the [interactive example](../../../examples/lib/salesforce-getting-started/README.md)
connects to a real Developer Edition org or sandbox.

```sh
cargo test -p drasi-source-salesforce --tests -- --include-ignored
cargo test -p drasi-salesforce-common
make test-salesforce-ffi
```

The source owns its dynamic-plugin tests in `tests/ffi.rs`, gated by the
`dynamic-plugin` feature. `make test-salesforce-ffi` builds the source cdylib
before running those tests through the unchanged host SDK. They use local
protocol fixtures and require no Salesforce account.

`PositionUnavailable` means retained history or source identity cannot satisfy
the checkpoint. A checksum failure means an invalid artifact or wrong spool.
Preserve state while investigating; do not delete one half of the source/query
storage pair to hide a recovery failure. `REST-only` names a native-topic
limitation; it does not mean that object is omitted from the graph.
