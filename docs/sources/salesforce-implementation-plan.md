# Salesforce source implementation plan

**Status:** Metadata-driven source and public example implemented. The current
scope preserves the existing FFI, SDK version, public interfaces, and index
storage formats. Historical verification receipts below describe earlier
revisions and do not replace verification of the contract-preserving revision.

**Decision:** Current-state convergence with durable local replay and persistent
query recovery in the first release. Durability is not a later phase. This
document replaces the earlier nonpersistent-only proposal.

The [source-planner](../../.github/agents/source-planner.md),
[source-plan-executor](../../.github/agents/source-plan-executor.md),
[source guide](../../components/sources/README.md), and
[bootstrap guide](../../components/bootstrappers/README.md) govern delivery.
Executable repository APIs take precedence over stale guide examples.

## Contract-preserving revision

The FFI implementation, SDK version, shared source/bootstrap/checkpoint
interfaces, DTOs and index formats match the pre-branch baseline `207ef6f3`.
The default bootstrap provider now runs inside the Salesforce source rather
than in a separate plugin. Query recovery uses existing checkpoint, result
sequence and live-results operations; there is no new completion-marker format.

Current verification includes 132 common/source tests, 223 query-runtime tests,
bootstrap-failure cleanup and output-atomicity regressions, and the parallel
solver tests. The two source-plugin FFI cases exercise the unchanged ABI,
including populated/empty snapshots, schema discovery, invalid positions and
replay of a committed update after the snapshot boundary. The standalone
example's tests, offline create/update/delete/repair flow and read-only live
schema acceptance passed again.

`cargo test --workspace --exclude drasi-host-sdk` encountered an environmental
failure in the unchanged `test_sse_basic_integration`: another Docker service
owns `127.0.0.1:18080`. That service was left untouched. The workspace rerun with
`-- --skip test_sse_basic_integration` passed, as did
`JQ_LIB_DIR=/opt/homebrew/opt/jq/lib make clippy`. The original macOS FFI CI failure
was a missing `protoc` introduced by the host test's Salesforce-common
dependency; that unnecessary dependency has been removed.

## Metadata-driven graph design

This approved redesign replaces the earlier single-object projection. Callers
select objects, not fields or joins:

```sh
./quickstart.sh --org drasi-dev --objects Account Contact
./quickstart.sh --org drasi-dev --objects Account Contact --schema
```

```cypher
MATCH (c:Contact)-[:Account]->(a:Account)
RETURN c.Name AS contact, a.Name AS account
```

`SalesforceSourceConfig.objects` replaces `projection`. Normal source startup
discovers metadata and installs a source-internal snapshot bootstrap provider.
The provider stays inside the source plugin; no separate Salesforce bootstrap
plugin or cross-plugin invocation metadata is needed. Runtime schema inspection
is separate from `Source.properties()`, which continues to persist configuration.

### Standard source schema discovery

`Catalog::source_schema()` derives Drasi's `SourceSchema` from the same typed
plans and selected lookup targets used for graph projection. The source's
`describe_schema()` hook serves an immutable in-memory snapshot through both
`get_source_schema()` and `get_graph_schema()`, including dynamically loaded
plugins through the existing FFI hook. No generic API, ABI, persisted format,
configuration or state namespace changes are required.

Startup publishes the snapshot after catalog persistence and report publication.
Changed metadata refreshes also publish to observers before replacing the
snapshot. Failed discovery/persistence/publication preserves the previous view.
Fresh source instances expose no schema until successful metadata preparation;
stopped sources retain the last published schema, and successful local
deprovision clears it before common cleanup. This is not a health/readiness signal
or an atomic snapshot of backfill completion. Durable diagnostics can be newer
if a commit succeeds but report publication fails.

Generic hints describe actual emitted values: Date/Time are String, Instant is
Timestamp, and MultiSelect is Json with a list-of-strings description. Each
relationship label has one declaration; ambiguous owner/target endpoints remain
unspecified rather than letting generic graph aggregation select the first one.
Existing local metadata fixtures cover public inspection, refreshes, failed
publication and lifecycle behavior; the host fixture exercises positive schema
discovery through an independently loaded plugin without a cloud account.

Historical schema-discovery verification, before contract restoration:

- `cargo test -p drasi-salesforce-common -p drasi-source-salesforce` — passed,
  124 tests.
- `cargo clippy -p drasi-salesforce-common -p drasi-source-salesforce -p drasi-host-sdk --all-targets --features drasi-source-salesforce/dynamic-plugin -- -D warnings` — passed.
- `make test-host-sdk` — passed after rebuilding the plugins: 71 tests passed,
  six pre-existing unrelated tests remained ignored. All three Salesforce FFI
  tests ran, including positive schema discovery.
- `cargo fmt -- --check` and `git diff --check` — passed.

Real-endpoint follow-up: with `SF_BIN` and `SALESFORCE_TEST_ORG` set to the saved
developer-org login,
`cargo test --manifest-path examples/lib/salesforce-getting-started/Cargo.toml live_schema_discovery -- --ignored --nocapture`
passed (one live test executed, zero ignored). Both public inspection APIs
matched independent live Describe requests: Account had 68 properties, Contact
had 65, and the relationship labels were Account, MasterRecord, Parent and
ReportsTo. The test also checked fresh-instance absence, stopped-schema
retention, rediscovery after restart, graph cleanup after source removal, and
deprovisioning of an isolated temporary spool. It awaits lifecycle events before
asserting status; graph status propagation is asynchronous. No Salesforce data
or settings were changed, and existing example state was not reused. Changed
metadata refresh and failure scenarios remain covered by the local fixtures,
not by mutations to the live org.

The prior repository-wide receipts below remain historical; the full workspace
suite was not rerun for this addition.

### Selected shape and synthesis

Three designs were compared: unified graph-element storage, record-owned graph
aggregates, and normalized records/reference slots. The parent and independent
cross-judge chose **record-owned aggregates**: a Salesforce record owns its node
and outgoing lookup relations. This avoids reverse-reference indexes and
target-driven fanout. The choice is conditional on executable query tests proving
late endpoints, target deletion/restoration, and persistent-index recovery.

From the other designs, retain metadata-only relationship names, explicit
unsupported-field diagnostics, active-versus-discovered schema reporting, and
bounded per-record reprojection. Reject guessed edge labels, target-dependent
edge identity, distributed per-object cursors, and whole-catalog atomic rewrites.
The store derives each graph delta from its authoritative previous aggregate;
callers cannot supply a mismatched row and event list.

The component contract is:

```rust,ignore
// Common metadata and graph model; no SDK/FFI layout changes.
Catalog::from_describes(Vec<Describe>) -> Result<Catalog>;
Catalog::record(object: &str, record: Record) -> Result<GraphRecord>;

struct GraphRecord {
    object: String,
    id: String,
    properties: Properties,
    relationships: Vec<LookupRelationship>,
    observed_at_ms: u64,
    schema_revision: String,
}
struct LookupRelationship {
    field: String,
    label: String,
    target_object: String,
    target_id: String,
}

// Private source mutation boundary, committed in one redb transaction.
enum RecordMutation {
    Upsert(GraphRecord),
    Delete {
        object: String,
        id: String,
        observed_at_ms: u64,
        schema_revision: String,
    },
}
DurableMirrorStore::commit_mutations(Vec<RecordMutation>)
    -> Result<Vec<Arc<SourceEventWrapper>>>;
```

Node IDs remain `sf:<org>:<object>:<id18>`. Edge IDs are
`sfrel:<org>:<object>:<owner-id18>:<lookup-field>`, directed from referring node
to referenced node. Labels come only from Describe `relationshipName`.
Missing names, unsupported fields and unselected targets are reported; raw
foreign-key properties remain available. Selected polymorphic targets resolve
only through unambiguous metadata key prefixes. No placeholder nodes are made.

Null lookups remove edges; reparenting deletes the old edge and inserts the new
one; deleting a record removes its owned edges before its node. Relations may
precede their targets and remain after target removal, matching no complete path
until that target exists. Removed properties are explicitly set to null because
the core merges sparse updates.

Object observers share one store, one delivery gate and one global journal.
Each object serializes its observations; successful scans sweep only that
object, and failed scans never infer deletion. Bound object admission, concurrent
REST work, dirty IDs, row/edge bytes, journal and snapshots. Keep cancellation-safe
task ownership and stop-timeout exclusion. Native hints remain optional for
convergence: an unavailable object topic is reported as REST-only, never silently
treated as a working native subscription; authentication failures remain errors.

Persist the active metadata catalog. Re-discovery changes its revision and
schedules full reprojection; each stored record retains its revision so inspection
can report pending work. Reject stale observations at the transactional boundary.
Per-record replacement atomically updates its node/edges, journal and head; it
does not promise an atomic multi-event downstream query transaction.

Snapshot/ticket/store format v2 contains the selected object set and framed
`GraphRecord` values, bounded while reading and writing. Bootstrap validates the
artifact, emits nodes before relations, and preserves one captured head H.
Bootstrap filtering must be proved equivalent for labelled/unlabelled patterns,
not inferred from endpoint labels. Preserve replay `(H,R]`, live `>R`, sequence-zero
bootstrap handover, independent cursors, retention pins and typed unavailable
positions. Reject v1 state explicitly and preserve existing demo directories.

### Redesign acceptance

Required evidence includes actual joined result changes for create/reparent/null/
delete/late target/self-reference; metadata omissions and compound scalar fields;
object-scoped failed scans; snapshot/replay equivalence and restart at every
multi-event delta prefix; schema clears and stale-revision rejection; cancellation,
retention and quota regressions; default and dynamic bootstrap; and the exact
public example against isolated live Account/Contact fixtures. No cloud settings
changes or non-fixture deletion are authorized by this plan.

### Metadata-graph acceptance results

The exact public executable was exercised against the authorized developer org
with two isolated Accounts and one Contact. It discovered 68 Account and 65
Contact scalar fields, including the metadata-derived `Contact.AccountId`
relationship, without projections or manual joins. Schema-only mode returned
parseable JSON and created no local state.

The live run verified creation, target rename, reparenting, clearing/restoring
the lookup, scalar nulls, and owner deletion. Account rows and counts matched the
existing org plus the fixtures, with no relationship-as-node rows. Unchanged and
offline-edit/deletion restarts reused all four query checkpoints without another
bootstrap. A custom graph query used its own query namespace and preserved the
default queries' state. Cleanup independently confirmed all three fixtures were
absent and the user's existing Account remained present.

Contact's native topic is unavailable in this org; those live Contact changes
were observed through REST repair. No CDC settings, OAuth apps or permissions
were changed. Account's native topic is available. This graph run does not claim
native delivery for every edit; local protocol tests isolate that path.

| Check | Result |
| --- | --- |
| Common/source/bootstrap unit tests | 46 / 46 / 8 passed |
| Source protocol and graph integration | 12 passed; none ignored |
| Independently loaded host SDK suites | 70 passed; 6 existing cases ignored |
| Randomized cross-cdylib layouts | 2 passed |
| Standalone example | 7 unit tests, Clippy, formatting, helper syntax and offline CRUD/repair passed |
| Workspace formatting and supported `make clippy` | Passed |
| Full workspace suite | 5,200 passed; 0 failed; 264 existing cases ignored across 294 test targets |

The metadata integration test changes Describe without editing any record or
sending a native event, advances the Tokio clock once, and checks fingerprint
change, explicit field clearing in joined results, journal advancement and
completed reprojection. Store tests compare snapshots and journal replay at
every graph-event prefix and reject stale schema mutations atomically.

Two runtime defects exposed by graph acceptance were fixed: match slots now
distinguish nodes from relations even when labels collide, and permanent
`DrasiLib.shutdown()` releases component ownership cycles that kept redb locked.
Shutdown owns its task across caller cancellation, propagates stop failures and
allows retry; ordinary `stop()` remains restartable. Snapshot export cancellation
and stop-timeout tests verify that restart/deprovision cannot race retained work.

The first full workspace run exposed a recovery-conformance observation race:
the test read a checkpoint immediately after the earlier side-effect journal
write. It now uses the existing bounded checkpoint-sequence wait before asserting
the same complete recovery result; no expected result or production checkpoint
behavior was weakened.

The retained live script and receipt are session artifacts:
`verify-salesforce-graph-example.py` and
`salesforce-public-graph-2cd1cd17-f6d3-425c-bbcb-e321f1ff44b0/result.json`.
The source-specific review follow-up adds regressions for unseen-record deletes,
`INVALID_FIELD` rediscovery (including transient discovery failures), cancelled
initialization during stop, and actual redb file-growth rejection. The latter
also reopens the database and checks that rows, journal events and the head retain
only successful commits. Database allocation, snapshot exports and schema files
now share an enforced file-growth budget.
Follow-up validation passed 124 tests across the three Salesforce packages and
their integration suites, plus two recovery tests using freshly rebuilt dynamic
plugins. Workspace formatting and targeted Salesforce Clippy passed. The
full-workspace receipts in the table above are from the preceding acceptance run,
not a new full-workspace run for these fixes.

Live client-credentials OAuth, production-source coverage of the prototype's complete
datatype matrix, and a process kill between snapshot publication and lease
persistence remain unverified boundaries.

## 1. Overview

| Crate | Responsibility |
| --- | --- |
| `drasi-salesforce-common` | Authentication, protocol, metadata catalog, record-owned graph mapping, local positions and framed snapshots |
| `drasi-source-salesforce` | Native CDC, REST reconciliation, transactional mirror/journal, snapshot exports, independent replay and live delivery |
| Source-private `bootstrap` module | Read a source-owned immutable snapshot; no independent REST enumeration or Salesforce credentials |

One source represents one Salesforce organization and a selected object set.
Describe discovers supported fields and physical lookup relationships. Objects
have independent acquisition loops but share one durable journal and snapshot
boundary. Lookup IDs also remain scalar properties.

### Consistency contract

Native CDC is an invalidation signal. REST supplies full current observations.
After writes and visibility stabilize, a successful complete scan and delivery
of its observations converge the query to the rows visible to the integration
identity. Mandatory periodic scans repair missed notifications.

REST reconciliation continues while native connection setup is pending, including
when Subscribe response headers wait for Salesforce's idle keepalive. Native
gap notifications schedule a complete scan. Configured REST fields need not
exist in the native event schema: only its change header supplies invalidations.

The source commits each changed observation and its normalized graph events
atomically before delivery. Queries can recover the exact retained sequence of
**locally observed** changes, independently of Salesforce's replay retention.

This does not promise an atomic Salesforce snapshot, every upstream transition,
Salesforce transaction boundaries, historical event-time answers, or exactly-once
external reaction side effects. Intermediate changes can coalesce; a short-lived
record may never be observed. Full scans can temporarily reflect mixed times.

Use this for recoverable CRM views, dashboards and current-state aggregates,
not per-change billing or an upstream audit trail.

### Scope

Object selection, metadata-derived fields and relationships, whole-object
membership, channel backpressure, bounded resources, static embedding and
an independently loaded source plugin with an internal bootstrap provider.
No arbitrary SOQL predicates,
joined SOQL projections, Bulk API or automatic Salesforce metadata provisioning.

Salesforce has no supported local server image. Developer-org acceptance is the
explicit SaaS exception to the executor's generic Docker requirement. Local
HTTP/gRPC protocol servers are tests, not Salesforce emulators.

## 2. Example usage

### Embedded application

The application supplies a validated `SalesforceSourceConfig`; secret acquisition
belongs to the application or its injected identity provider.

```rust,ignore
use drasi_lib::{DrasiLib, Query};
use drasi_source_salesforce::{SalesforceSource, SalesforceSourceConfig};

let config: SalesforceSourceConfig = load_salesforce_config()?;
let source = SalesforceSource::builder("crm")
    .with_config(config)
    .build()?;
let query = Query::cypher("account-contacts")
    .query("MATCH (c:Contact)-[:Account]->(a:Account)
            RETURN c.Name AS contact, a.Name AS account")
    .from_source("crm")
    .enable_bootstrap(true)
    .build();
let drasi = DrasiLib::builder()
    .with_source(source)
    .with_query(query)
    .build()
    .await?;
drasi.start().await?;
```

`load_salesforce_config` is application code, not a plugin API. Construction
validates configuration without contacting Salesforce. Inject durable query
indexes using the runtime's index-provider configuration when query state must
survive restart; the source spool and query indexes are separate durable stores.

`with_identity_provider(Arc<dyn IdentityProvider>)` on the source supports a
runtime Token identity. Do not give the bootstrapper Salesforce credentials.
Production components never invoke the Salesforce CLI.

### Dynamic component properties

This is the source descriptor's properties DTO, not an enclosing server
configuration format. `ConfigValue` resolves environment/secret references.

```json
{
  "connection": {
    "instanceUrl": "https://YOUR-ORG.my.salesforce.com",
    "expectedOrgId": "00D000000000001",
    "apiVersion": "68.0",
    "auth": {
      "type": "clientCredentials",
      "tokenUrl": "https://YOUR-ORG.my.salesforce.com/services/oauth2/token",
      "clientId": {
        "kind": "EnvironmentVariable",
        "name": "SALESFORCE_CLIENT_ID"
      },
      "clientSecret": {
        "kind": "Secret",
        "name": "salesforce-client-secret"
      }
    }
  },
  "objects": ["Account", "Contact"],
  "spoolRoot": "/var/lib/drasi/salesforce/crm",
  "reconcileIntervalSeconds": 5,
  "fullScanIntervalSeconds": 60,
  "maxRows": 100000,
  "maxEvents": 1000000,
  "retentionHours": 96
}
```

Replace the example organization ID with the deployment's validated identity.
The corresponding bootstrap configuration is local-only:

```json
{
  "spoolRoot": "/var/lib/drasi/salesforce/crm",
  "maxRows": 100000,
  "maxRowBytes": 1048576
}
```

Both plugins must see the same protected spool directory. Do not expose that
directory over an unauthenticated file service or share it between source owners.

## 3. Data extraction and CDC mechanisms

### Historical protocol verification

The retired prototype established the observations below. Its executable and
fixture tooling have been removed. Current mapping and protocol regressions live
in [salesforce-common](../../components/salesforce-common/README.md); the supported
runnable example is [salesforce-getting-started](../../examples/lib/salesforce-getting-started/README.md).

| Evidence | Established boundary |
| --- | --- |
| Local suite | 24 tests covering Avro, HTTP/gRPC protocols and real Drasi queries |
| Account acceptance | Bootstrap, create, sparse update, null, in-memory CUSTOM reconnect and delete |
| Datatype acceptance | 22 native projected properties, three REST-only fields, two hash-verified native text diffs |
| Cleanup | Temporary objects, CDC members, permissions and regression records independently checked as removed |
| Authentication diagnosis | `sf org display` returned a redacted token; `sf org auth show-access-token` fixed native connectivity |
| Remaining uncertainty | Some rapid long-text-only changes produced no captured CDC event within 60 seconds; spaced and scalar-plus-text controls worked |

These prove prototype behavior, not production durability, client-credentials OAuth or
dynamic-plugin recovery. Keep org-specific logs and raw records local; component
tests use synthetic, credential-free fixtures.

Use native TLS gRPC at `api.pubsub.salesforce.com:7443`; port 443 is also
supported. Send `accesstoken`, `instanceurl` and `tenantid` metadata. Preserve
the provenance/license of the protobuf subset audited against upstream commit
`20fb138250aa603394a670733bb41930095e0e85`.

Use compatible existing dependencies: tonic/tonic-prost 0.14, prost 0.14,
apache-avro 0.20, reqwest 0.12, chrono, tokio and redb 2.6. Native text-patch
regressions remain in the common crate; production reconciliation does not apply
historical patches.

### Capture protocol

1. Authenticate, verify organization identity, Describe the selected object and
   validate fields before publishing observations.
2. Resolve the change topic and schemas. Bound schema cache, message size,
   invalidation queue and concurrent requests. Fetch at most 100 events.
3. Decode an entire envelope before enqueueing every affected record ID.
   `changeType` is an Avro enum, not necessarily an Avro string.
4. Fetch full REST observations. CREATE, UPDATE, DELETE and UNDELETE are hints:
   an old DELETE must not remove a subsequently restored visible record.
5. Reconcile periodically even when CDC appears healthy. Validate every page
   before treating a scan as complete or inferring absent records.

Replay tokens are opaque, non-sortable and reusable after maintenance or org
migration; CDC has no platform-event `ProducerEvent.id`. Do not deduplicate only
by replay token. Coalescing repeated record invalidations is permitted.

Upstream reconnect may use LATEST followed by mandatory full reconciliation.
This is the declared current-state mode, not a hidden fallback for a missing
query checkpoint. Query recovery always uses the local journal.

Account for `pending_num_requested`; replenish exhausted demand within 60
seconds. An idle keepalive can take 270 seconds. Do not interpret that interval
as failed startup or add sleeps to hide missing text-only notifications.

## 4. Data mapping strategies

**Selected:** full REST observations plus a durable journal. Native historical
patching would require an exact prior baseline and continuous upstream history,
which this consistency contract does not claim.

Use the common identity helper on both paths:

```text
source_id = configured Drasi source ID
element_id = sf:<canonical-org-id>:<object-api-name>:<canonical-record-id>
node label = object API name
```

Canonicalize Salesforce 15-character IDs using the checksum algorithm; validate
18-character IDs without lowercasing case-sensitive data.

| Salesforce value | Drasi representation |
| --- | --- |
| Text, textarea, rich/long text, email, phone, URL, picklist, autonumber | String, preserving Salesforce's stored value |
| ID and lookup | Canonical ID string |
| Checkbox | Bool, including false |
| Number, currency, percent | Finite binary64 Float; no decimal fidelity or rescaling |
| Genuine integer | Integer only when Describe and fixtures establish that contract |
| Date | `YYYY-MM-DD` string |
| Time | `HH:mm:ss.SSS` string |
| DateTime | UTC `ZonedDateTime`; RFC3339 in query JSON |
| Multi-select picklist | Canonically sorted string list |
| Geolocation | Complete object with named components |

Every observation contains every discovered supported property, including explicit
null. A missing requested key is an error, not an implicit null. Reject malformed
temporal/nonfinite numeric values. Unsupported compound parents and field types
are reported; supported scalar components remain available.

The prototype's native Date LONG was epoch milliseconds, not epoch days. Custom
Number(scale=0) was still double. Formula and classic encrypted fields were
REST-only in the fixture. Advertise those, Shield, binary, roll-ups, custom
address and specialized relationships only after dedicated acceptance establishes
their permissions and refresh semantics. Do not enable global org features to
make a test pass.

Bootstrap emits Inserts. The journal records full upserts and confirmed deletes
only when materialized state changes. Verify Update-as-upsert behavior with real
aggregate queries. Use the record timestamp discovered in metadata, or local
observation time for undated objects, not an old CDC commit time attached to
newly fetched REST data. Unchanged observations retain their prior timestamp.
Owned edges follow the record observation time; retained edges receive metadata
updates when a changed owner observation advances that time.

## 5. Architecture and components

### Transactional source store

One exclusively owned redb database contains format version, source identity,
epoch, committed head, record-owned graph aggregates, active schema/progress and
normalized journal events. Source identity binds organization, selected objects
and API/mapping version, independently of the mutable discovered catalog.
Reject incompatible identity or corrupted metadata; never silently recreate it.

One writer commits row changes, consecutive ordinals and head together. Dispatch
only committed entries. Restore the committed head on restart, never an allocated
but uncommitted counter. Unchanged observations do not consume journal positions.
Failed or incomplete scans cannot delete unseen rows.

The generic WAL and StateStore cannot express this atomic row/event/metadata
transaction; do not combine their separate writes and call it durable.

### Snapshot and subscription protocol

1. Capture mirror head H and export an immutable snapshot from the same read
   transaction. Bound export size and concurrent work; publish files atomically.
2. Issue a scoped, versioned ticket in bootstrap context property
   `salesforce.bootstrap_ticket.v2`.
3. The ticket binds source identity, organization/object set, epoch, H, expiry and
   manifest checksum. All values must survive JSON context encoding, including
   epochs above `u64::MAX`.
4. Bootstrap resolves only files inside the configured spool, validates identity,
   checksums, version, expiry, record/relationship shape, count and byte limits
   before streaming framed records as nodes followed by relations.
5. Register the live receiver and capture head R under the same delivery gate
   used by the sole publisher.
6. Return snapshot rows at H, then journal `(H,R]`, then live events strictly
   after R. A recovering query instead receives `(checkpoint,R]` before live.

The provider returns `BootstrapResult.source_position = H`.

Completed bootstrap checkpoints pair cursor H with sequence zero, even for a
nonempty snapshot. Live sequences equal local journal ordinals. Recovery accepts
the zero bootstrap sentinel or a nonzero sequence equal to the cursor ordinal;
a nonzero sequence without a local position is invalid.

Use `SourceBase::subscribe_with_bootstrap_context`, preserve its receivers,
position handles and sequence floors, and dispatch through SourceBase. There is
no public `apply_subscription_settings` API to invoke.

Pin exports and replay readers against retention. Failed/cancelled subscriptions
must release quota, receivers and leases. Stop cancels and joins tasks; a timeout
must not detach a writer that can continue mutating a supposedly stopped source.

Initialization and the runtime task remain source-owned across cancellation.
Commit/pruning, snapshot/cut capture and pin registration share the delivery
gate. Catchup-only pins release when replay drains; confirmation-driven pins
protect unconfirmed live history, including the suffix after a sequence-zero
bootstrap checkpoint. Dropped receivers release admission quota.

### Authentication and dynamic boundaries

Only the source owns Salesforce credentials. Support runtime Token identities
and configured Salesforce client credentials. Resolve credentials at the network
boundary, redact Debug/error output and refresh without logging tokens. Restrict
redirects and validate destinations before attaching authorization metadata.

The source descriptor uses `ConfigValue`, camelCase DTOs, complete persisted
properties and a gated `dynamic-plugin` export. Config properties preserve secret references;
logs and diagnostic formatting do not reveal resolved secrets.

The existing FFI and `FFI_SDK_VERSION` **0.14.0** are unchanged. Snapshot tickets
stay inside the source plugin. Bootstrap events, results and replay positions
use the existing source subscription transport. No host proxy, vtable, envelope,
or resume-sequence encoding changes are included.

Static subscriptions preserve typed `SourceError::PositionUnavailable`.
The existing SDK reports dynamic subscription failures generically, so a
dynamic source cannot trigger the runtime's typed-error AutoReset path. It fails
explicitly instead. Host callback scheduling and lifetime behavior remain those
of the existing SDK; this source adds no stronger forwarding guarantee.

## 5.5. Replay and recovery design

Positions encode a version, source epoch and local journal ordinal. They are not
Salesforce replay tokens. `supports_replay()` is true. Each query replays its own
checkpoint independently; recovery must not rewind every subscriber upstream.

An event at the saved position is already processed. Replay and
`PositionComparator::position_reached` are strictly-after. Retention must preserve
the exact restart boundary or return typed `PositionUnavailable` with the
requested bytes and an earliest available position when known.

The earliest resumable boundary is one less than the first retained event, or
the committed head when no events remain. An empty journal must not admit the
unavailable predecessor of its head.

Keep source event sequences and opaque positions conceptually separate.
Restore sequence floors from saved checkpoints and preserve monotonicity through
bootstrap, the first live event, reconnection and process restart.

### Durable query bootstrap

The existing `CheckpointStore`, `LiveResultsWriter` and session operations commit
source boundaries, the initial live-results snapshot and result sequence zero
together. No trait methods, index storage keys, or serialization changes are
introduced. Source boundaries remain opaque existing checkpoint values.

Begin, staging or handover-commit failure cancels bootstrap/tail processing and
fails the query. No live result may follow a failed durable handover.

Strict recovery rejects a matching bootstrap-enabled configuration with no
committed result sequence, including interrupted initial snapshots and legacy
handover-only checkpoints. AutoReset explicitly clears and rebuilds them.
This does not add a new marker that retroactively proves the completeness of
legacy snapshots with an existing result sequence.

## 6. Testing strategy

Tests assert real query/reaction state, not only received events or successful
compilation. Use bounded deadlines and deterministic fault injection.

| Layer | Required cases |
| --- | --- |
| Common | ID canonicalization; all advertised types; null/missing distinction; config/secret references; auth refresh/redaction; header scope; corrupt Avro; multi-record envelopes |
| Journal | Unchanged observations; atomic row/event/head commit; reopen; pre/post-commit crash; identity mismatch; corrupt metadata; incomplete scan never deletes |
| Snapshot | Immutable H; populated/empty snapshots; high-bit epoch codec; expiry/checksum/count/size failure; path traversal and symlinks; cancelled export cleanup |
| Replay | Strictly-after boundaries; two independent positions; retention pinning; partial fanout; `(H,R]` cutover; capacity-one backpressure; quota cleanup |
| Runtime | Fresh persistent bootstrap; no-live-event restart; partial bootstrap; Strict/AutoReset; failed begin/stage/commit; mixed inputs; immediate live event then reopen |
| Dynamic | Independently loaded source on the unchanged ABI; internal populated/empty bootstrap; schema inspection; replay; invalid positions fail without silently resetting |
| Live | Authorized developer org, real native Pub/Sub, full observations, aggregates, restart, missing-hint repair and independently verified cleanup |

Local HTTP/gRPC fixtures must include pagination, auth failures, throttling,
reconnect and bounded queue behavior. Tests must not depend on unrestricted
production endpoints, invented tokens or fixed sleeps for synchronization.

The production example includes `setup.sh`, `quickstart.sh`, `diagnose.sh` and
`test-updates.sh`. `quickstart.sh --org <alias>` watches the real org using the
production source and its internal bootstrap reader, CLI-supplied runtime identity, persistent
RocksDB queries and console reaction output. Users edit their Accounts and Contacts in
the UI; the watcher never writes Salesforce data. State survives Ctrl+C/restart.
`--local` explicitly selects the offline protocol demonstration; it and live
mode share the runtime builder. `test-updates.sh` runs that offline scenario.
Validation reports clear PASS/FAIL outcomes and nonzero failure status.
Cleanup targets only recorded fixture resources and verifies absence afterward.
It never removes unrelated objects, permissions, source spools or query indexes.
Source `deprovision` deletes only source-owned artifacts after stopping writers;
it must not recursively remove an arbitrary configured parent directory.

Historical protocol findings are recorded above, but do not substitute for running the
production components.

## 7. State management and recovery

Use a protected, exclusively owned spool with restrictive permissions and
versioned files. Never store credentials or unfiltered raw org payloads there.
Snapshot paths must not escape through a final-component or parent symlink.
Validate sizes before reading or allocating; offload disk/database operations
from current-thread Tokio workers.

Recommended defaults are 100,000 rows, 1,000,000 journal events, 96-hour
retention, 1 MiB projected rows, 8 MiB REST/native envelopes, 100-event/1 MiB replay
pages and 16 subscribers. Serialize snapshot export and bound outstanding leases.
An aggregate spool budget must include snapshots and journal data; 8 GiB is the
initial deployment target, not permission to grow until the disk is exhausted.

Reject zero, overflowing or unsafe bounds. Prune only entries no active replay or
snapshot requires. If a pin prevents staying within limits, fail explicitly
rather than losing recovery data or allocating without bound.

Transient transport errors retry with bounded backoff and cancellation.
Authentication, projection, identity, storage corruption and commit failures
surface as errors. Runtime task failure changes component status; a logged error
with a permanently Running source is not acceptable.

Removing local journal data, changing integration visibility or changing source
identity is an explicit recovery/reset operation. Never silently substitute
LATEST for a query's missing local history.

## 8. Implementation phases

1. Shared mapping/auth/protocol and atomic mirror/journal; prove commit/reopen
   before using it for dispatch.
2. Snapshot exports, source subscription/catchup and local bootstrap provider;
   prove race-free cutover and bounded resources.
3. Commit the bootstrap output and source boundaries using existing checkpoint
   operations; leave all shared interfaces and FFI definitions unchanged.
4. Integrate persistent queries, the independently loaded source, cancellation,
   retention and crash tests; resolve every failed behavioral contract.
5. Run authorized Salesforce acceptance, independently audit cleanup, complete
   the example/docs and obtain an independent code review.

Each phase ends in a verifiable state. No compatibility shim or placeholder is a
substitute for a completed recovery path.

## 9. Definition of done

- The common and source crates are registered; xtask discovers the source plugin.
  Static linking and the separately built source cdylib work.
- Source/bootstrap IDs and projected values agree; real aggregates converge.
- Durable snapshot/replay and incomplete-bootstrap recovery work without
  requiring the user to disable persistent query state.
- Every configured resource limit, failure boundary and cleanup obligation has
  a behavioral test. No silent reset, dropped error or detached writer remains.
- The supported example and four helpers run; live fixtures are removed and
  absence is independently verified.
- Documentation describes actual configuration, storage ownership, migration
  policy, consistency limitations and operational recovery.
- Independent review findings are resolved and relevant tests rerun.

Required repository gates:

```sh
cargo fmt -- --check
make clippy
cargo test --workspace --exclude drasi-host-sdk
make test-host-sdk
make test-ffi-layout-mismatch
```

Use system libjq for `make clippy`, not `cargo clippy --all-features`.
Docker, protoc and cmake are available prerequisites for relevant suites.
Report real failures/ignored cases and live-test limitations; do not label this
checklist complete merely because the prototype passed.

## 10. Known limitations

Current-state convergence is not complete Salesforce history. Local durability
preserves observed changes only. Upstream retention is 72 hours and does not
govern local query checkpoints.

REST scans consume API quota and are not atomic snapshots. Metadata changes are
reprojected per record; changing integration-user visibility changes the mirrored
graph. Floating-point mapping does not preserve arbitrary decimal precision.
Only selected, unambiguously resolved lookup targets produce relationships;
unsupported fields and omitted targets are reported by schema inspection.

The spool is single-owner local storage, not distributed high availability.
The internal bootstrap reader uses the source's filesystem view. Losing both
the spool and its backup requires an explicit recovery decision. The existing
FFI loses typed subscription failures, so dynamic-plugin AutoReset cannot
classify an unavailable position and requires operator intervention.

Live prototype acceptance used developer tooling for tokens. Production client-
credentials acceptance requires an appropriately configured Salesforce OAuth
application; creating credentials or changing access needs separate approval.

### Baseline production-component acceptance before the redesign

Developer-org acceptance exercised the actual source and local bootstrap with
RocksDB-backed queries and an ApplicationReaction. It passed initial Account
loading, native-triggered phone updates and nulls, aggregate counts, unchanged
process restart without exporting a new bootstrap, and recovery of an Account
deleted while Drasi was offline. Terminal aggregates retain their zero-count
group under the engine's existing semantics; the corresponding row query became
empty. The final code passed a second complete live run. Including the public
example verification below, all nine scoped test Accounts were independently
confirmed deleted.

The live harness used an injected Token identity and `initialCursorBehavior:
earliest` to cover writes made before the first native subscription was ready.
OAuth client credentials were tested against the local HTTP fixture, not a new
live OAuth application. That harness projected Id, Name and Phone; the public
example additionally projected Description. The earlier prototype's broader datatype
acceptance is separate evidence, not a claim that every datatype was rerun
through the production source.

The public `salesforce-getting-started --org` executable was separately exercised
against the org: create, update, explicit null, delete, row/count queries and
ApplicationReaction output passed. Ctrl+C saved state; an unchanged restart
printed saved rows without a fresh bootstrap export, and an offline edit
converged after restart. This is the runnable interactive example, not the prototype
or an alternative private worker implementation.

Local protocol tests isolate native CRUD from periodic scans, exercise native
gap repair, and prove initial/periodic REST reconciliation and shutdown progress
while Subscribe headers remain blocked. REST transport resets, throttling,
authentication refresh, redirect refusal, and bounded chunked reads have focused
regression coverage.

### Baseline verification before the metadata redesign

| Check | Result |
| --- | --- |
| Workspace tests excluding host SDK | 5,160 passed; 0 failed; 265 ignored across 293 test targets |
| Salesforce common/source/bootstrap unit tests | 34 / 36 / 6 passed |
| Salesforce protocol integration, including ignored end-to-end case | 2 passed; no ignored cases |
| Rebuilt host SDK integration suites | 69 passed; 0 failed; 6 ignored |
| Cross-cdylib randomized-layout regression | 2 passed |
| Workspace formatting and `make clippy` | Passed |

The dynamic Salesforce test verifies the zero bootstrap sentinel and rejects
missing/mismatched nonzero checkpoints through independently built plugins.
The broader suites' ignored cases remain ignored; the Salesforce end-to-end
protocol case was run explicitly.

Live client-credentials OAuth and the full datatype matrix on the production
source remain unexecuted. At this baseline, exact physical redb file-growth
crossing and a process crash between snapshot-file publication and lease
persistence were not fault-injected. The source-specific review follow-up above
now covers physical file-growth rejection; the process-crash boundary remains
unverified.

## 11. Assumptions, decisions and remaining questions

The durable mirror/journal was selected over native historical patching and over
a nonpersistent-only first release. It supplies an exact local snapshot/tail
boundary without pretending REST aligns to Salesforce's historical event log.

Deployment must decide the integration identity and accepted field permissions,
API/retention/spool budgets, persistent query index backend, Strict versus
AutoReset policy and backup/reset procedure. These are operational choices, not
reasons to defer the first-release durability contract.

Periodic repair of a missed notification is covered by the production protocol
harness. The original live text-only notification gap's cause remains unknown;
it was not rerun as part of production datatype acceptance, and no sleep
workaround was added.
New OAuth apps, persistent access, or org-wide features are not implicitly
authorized by test execution.

## 12. References

### Repository

- [Getting-started example](../../examples/lib/salesforce-getting-started/README.md),
  [mapping](../../components/salesforce-common/src/mapping.rs),
  [protocol tests](../../components/salesforce-common/src/protocol_tests.rs).
- [SourceBase](../../lib/src/sources/base.rs),
  [source traits](../../lib/src/sources/traits.rs),
  [bootstrap interface](../../lib/src/bootstrap/mod.rs),
  [query manager](../../lib/src/queries/manager.rs).
- [Checkpoint contract](../../core/src/interface/checkpoint_store.rs),
  [RocksDB implementation](../../components/indexes/rocksdb/src/checkpoint.rs),
  [Garnet implementation](../../components/indexes/garnet/src/checkpoint.rs).
- [Plugin architecture](../../components/plugin-architecture.md),
  [existing FFI definitions](../../components/plugin-sdk/src/ffi/vtables.rs),
  [source proxy](../../components/host-sdk/src/proxies/source.rs),
  [bootstrap proxy](../../components/host-sdk/src/proxies/bootstrap_provider.rs).
- [Common crate](../../components/salesforce-common/Cargo.toml),
  [source crate](../../components/sources/salesforce/Cargo.toml),
  [internal bootstrap reader](../../components/sources/salesforce/src/bootstrap.rs).

### Salesforce

- [Supported authentication](https://developer.salesforce.com/docs/platform/pub-sub-api/guide/supported-auth.html).
- [Subscribe RPC and credits](https://developer.salesforce.com/docs/platform/pub-sub-api/references/methods/subscribe-rpc.html).
- [Event durability and replay-ID reuse](https://developer.salesforce.com/docs/platform/pub-sub-api/guide/event-message-durability.html).
- [Error handling](https://developer.salesforce.com/docs/platform/pub-sub-api/guide/handling-errors.html).
- [CDC message structure](https://developer.salesforce.com/docs/platform/change-data-capture/guide/cdc-message-structure.html).
- [Native text diffs](https://developer.salesforce.com/docs/platform/change-data-capture/guide/cdc-data-diff.html).
- [Client-credentials OAuth](https://developer.salesforce.com/blogs/2023/03/using-the-client-credentials-flow-for-easier-api-authentication).
- [Custom-address prerequisites](https://developer.salesforce.com/docs/platform/caf-dev/guide/caf-dev-manage.html).
- [Audited proto](https://github.com/forcedotcom/pub-sub-api/blob/20fb138250aa603394a670733bb41930095e0e85/pubsub_api.proto).
