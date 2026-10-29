# Native inter-instance bridge

[`drasi-computation-source-bridge`](../../sources/bridge) and
[`drasi-computation-sink-bridge`](../../sinks/bridge) connect two **normal,
complete DrasiLib instances**. They supply independent native `EnvelopeSource`
and `EnvelopeSink` factories, not remote
component-method execution, a transformer facade, or process supervision.
Register only the endpoints that process needs in its ordinary `FactoryRegistry`:

```rust,ignore
registry.register(drasi_computation_source_bridge::factory())?;
registry.register(drasi_computation_sink_bridge::factory())?;
```

`drasi-computation-bridge-common` supplies shared protocol, transport, controls and
recovery helpers. It contains neither component implementation nor an aggregate
component factory.

| Factory | Implementation/configuration version | Port |
|---|---|---|
| `drasi.bridge/source` | `"1"` / `1` | output `out` |
| `drasi.bridge/sink` | `"1"` / `1` | input `in` |

These remain statically linked native `ComponentFactory` implementations. The
endpoint crates do **not** export a C plugin ABI or imply that arbitrary host-owned
index/TLS resources can cross that ABI. The independently versioned bridge wire
is **1**, containing standard binary computation envelopes **1**.

## Configuration and registration

Both endpoints use `BridgeConfig`; unknown fields reject. This receiver example
shows all configuration keys:

```json
{
  "link_id": "plant-to-analysis",
  "sender_instance": "plant",
  "receiver_instance": "analysis",
  "sender_endpoint": "to-analysis",
  "receiver_endpoint": "from-plant",
  "stream": "analysis/ingress",
  "peer_stream": "plant/changes",
  "delivery": "events",
  "profile": "durable",
  "failure_scope": "processRestart",
  "namespace": {
    "version": 1,
    "source_prefixes": [
      "plant/",
      "drasi.output/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    ]
  },
  "transport": {
    "kind": "unix",
    "data_path": "run/analysis-data.sock",
    "control_path": "run/analysis-control.sock"
  },
  "limits": {
    "max_envelope_bytes": 1048576,
    "max_operations": 1024,
    "max_values": 65536,
    "max_depth": 32,
    "max_lineage": 32,
    "max_annotations": 64,
    "queue_capacity": 16,
    "timeout_ms": 5000,
    "retry_attempts": 3,
    "retry_delay_ms": 50
  }
}
```

The sender exchanges `stream` and `peer_stream`; the rest of its semantic
configuration is identical. `stream` must equal the local graph binding.
`failure_scope` and `limits` are optional, with defaults shown. All other fields
are required. `profile` is explicitly `volatile` or `durable`. Every link uses
the **exact** standard graph schema descriptor, not just matching names or
fingerprints. `delivery` is `events` or `complete-transactions`: it selects
behavior, never a data schema. Source, query, transformer and source-transaction
data are all ordinary graph data; there is no separate query or transaction
schema (`query-change`, `graph-change`, `source-transaction`, the old `schema`
field and `query_prefixes` reject as unknown values or fields, with no alias or
compatibility mode). `complete-transactions` requires every frame to carry its
own current, valid completion marker (scoped to that frame's change set, with
the operation count and commit position) and enforces the transaction limits and
source namespace; `events` accepts ordinary batches and validates any marker
that is present, so an unchanged relay keeps its completeness. A marker
inherited by a different change set is history, not completeness.

`BridgeConfig::specification(ComponentRole::Source/Sink)` builds the normal
component specification, descriptor and port requirements. Add these dependencies:

| Endpoint/profile | Dependency | Role/concrete resource |
|---|---|---|
| Durable source | `admission` | `StateStore`, actual `QosChannel` |
| Durable sink | `state` | `IndexBackend`, `QueryIndexProviderResource` |
| TLS source or sink | `tls` | `Identity`, this package's `TlsResource` |
| Source, optional host control | `control` | `Component`, `BridgeIngressControl` |
| Sink, optional host control | `control` | `Component`, `BridgeEgressControl` |

Every dependency has cardinality zero or one; configuration makes the listed
ones mandatory. Volatile endpoints reject admission/state bindings; non-TLS
endpoints reject a TLS binding. Factories validate the actual instance,
component, graph and stream identities. Construction never binds sockets or
starts workers. Startup/stop remain normal graph lifecycle operations.

The receiver journal must be dedicated to this link, lossless, persistent, and
configured for producer admission **before** constructing the source:

- `construction_scope = receiver_instance`
- `graph_id =` the actual receiving graph ID
- `component_id = receiver_endpoint`
- `failure_scope =` the selected, actually supported failure mode
- `max_producers = 1`, `receipts_per_producer = 1`

The bridge bounds each dedicated journal to 1–16 required subscribers.

Every outgoing edge uses that same journal. The standard SourceAdmission host
checks reject mixed/memory paths. The source revalidates the complete enabled
admission options, not a provider name. The single producer's persisted name
contains the link contract fingerprint; changing its identity/schema/namespace
contract cannot silently register a second producer.

The sender needs a dedicated, atomic `ComputationIndexProvider` bundle with a
real asynchronous cleanup owner and the requested failure survival. Its input
port requires FIFO, backpressure, durable acceptance, replay and explicit
acknowledgement. Use an actual retained/acknowledged input pipe. Calling the
sink manually makes preserving that upstream obligation the caller's job.
Receiver storage and sender mapping storage belong to their respective instances;
do not open another process's RocksDB directory.

## Admission, ordering and restart

There is **one outstanding envelope per configured link stream**. Logical
upstream sequences may have gaps; receiver client sequences start at one and
are consecutive. A complete source transaction stays one envelope and one
admission. No fragment gets its own acknowledgement.

- **Volatile:** success means acceptance into the explicitly bounded receiver
  queue. The sink declares `Accepted`. Stop, process failure or reconstruction
  can lose accepted queued work. No process-restart guarantee is inferred.
- **Durable:** success means the actual receiver QoS transaction committed both
  the envelope and its retry receipt, at `failure_scope`. The sink declares
  `Handled` **at this admission boundary only**. This does not mean the receiving
  query/subgraph finished, an external effect completed, or a composition drained.

The durable sender uses the existing `ComputationTransaction`, storing one
bounded mapping row in its outbox interface. It persists receiver session,
consecutive client sequence, canonical upstream logical identity/content and
the exact first-admission frame **before any data transmission**. Keeping that
one frame permits an upstream replay with a different transport ID or
post-commit timing to retry the identical QoS admission content. It is not a
second general-purpose delivery engine, a detached forwarder or an unbounded
outbox. The upstream input remains retained until the remote receipt is also
saved; then the frame is discarded and the latest receipt remains.

Canonical durable identity comes from the library's shared
`DeliveryBatchIdentity::for_input`, not a new bridge-specific definition. It
requires persistent immediate-producer progress or a supported native query
recovery identity. A complete source-transaction envelope without that persistent
producer evidence is accepted only in explicit volatile mode; its commit
position alone does not establish this sender's replay contract.

Lost acknowledgements resolve through the separate receipt lane or by retrying
the same session/sequence/content. Changed content conflicts. Older receipts,
client gaps, changed producer identity, missing sender state with existing
receiver history, changed receiver incarnation, and changed saved configuration
fail explicitly. There is no automatic receipt expiry-to-new-input conversion,
session retirement, state deletion, downgrade, or competing replacement owner.
An uncertain storage operation uses the existing transaction fencing/cleanup
contract. Exhausted bounded connection/backpressure retries leave the obligation
unresolved; they never report success.

Connections carry a receiver-issued generation. Reconnection invalidates old
data/control connections. A previously admitted old-generation operation can
still commit; it cannot report success for a replacement connection and must be
resolved by its original session and sequence. Disconnect/timeout/EOF is unknown
acceptance, **never** normal source exhaustion or evidence of rollback.

## Data and namespace preservation

Records, schema, full-width sequences, source timestamps and positions,
annotations (including duplicate keys), context identity and lineage cross the
wire through `BinaryEnvelopeCodec`. Local output gets the receiver's actual
stream, increasing emission sequence and producer identity. The input's origin
and logical progress remain in inherited context/lineage; a bounded
`drasi.bridge.origin.v1` annotation identifies the admission.

Namespace version one is an **explicit identity mapping**, not automatic
rewriting. Every element namespace (`ElementReference::source_id`) — of a record
key, relation endpoint, or reference embedded in a value — must match an entry of
`source_prefixes`. That single list covers physical source namespaces and derived
output dataset namespaces alike, because both are graph element namespaces. An
entry ending in `/` is a prefix requiring at least one more character; any other
entry is one exact namespace. Entries are limited to 256 bytes, with at most 16.

A query, transformer or other derived output publishes elements in the dataset
namespace `drasi.output/<SHA256(scope, graph, component, port)>`
(`output_identity::dataset_namespace`), not its query ID. Callers compute the
actual namespace for each exported output and list it **exactly**, next to the
physical source prefixes, for example
`["plant/", "drasi.output/<64 hex digits>"]`. Do not list `drasi.output/` as a
prefix unless every present and future output of every graph is intended to
cross. Older `query_prefixes` configurations reject; translate query IDs into
exact dataset namespaces rather than copying the IDs. Prefix checks still apply
recursively to embedded references.
Hosts must assign disjoint authority entries when independent producers could
otherwise collide. IDs already carry their namespace on both sides. Overlap
resolution, arbitrary renaming and non-identity namespace mappings are not
implemented; do not deploy overlapping IDs and assume the bridge fixes them.
Both peers negotiate and persist the exact namespace contract.

Portable graph records (canonical full element images, element patches including
property removals, and partial before-images) from sources, queries and
transformers, and complete source transactions, are supported. Query output
needs no query metadata to cross: a result node is an ordinary node of its
dataset namespace, and the same canonical record validation applies to it as to
any source record. Graph records carry shared values,
including expressions and awaiting values, as inert data: they are walked for
limits and embedded references, never executed. Event-local graph controls such
as scheduled notifications or `FuturesDue` on the current envelope reject; a
control kept only as lineage provenance is not a command, so ordinary data derived
from such an input crosses normally. Query snapshot and progress-only frames are
explicit recovery controls: they are recognized from the current envelope's own
scoped query projection, never from the schema or an inherited annotation, and
reject, as do custom plugin schemas. A complete source transaction passes the
same record guards and shared-value limits as any other graph data.
Decoding never discovers a plugin or calls a child-supplied executable validator.

Graph records are canonical MessagePack (`ElementImage`, `ElementPatch`,
`ElementBeforeImage`, MessagePack element-reference keys). Before standard
validation, an iterative MessagePack preflight bounds each record's framing,
collection counts, node budget (four times `max_values`) and container nesting
(`8 + 6 × max_depth`, which covers the wrapper variants around at most
`max_depth` value levels) before serde allocates. Only then are the shared
`ElementImage`, `ElementPatch` (including its set/remove overlap check) and
`ElementBeforeImage` codecs used to decode the record, which is walked for value,
depth, string and namespace limits, including element references nested in rich
values and expressions; its key must name the same element. An iterative envelope
preflight bounds framing and collection counts; record payloads are opaque to it
and are scanned separately. Limits cover operations, complete-transaction
changes, total values, identifiers, opaque fields, annotations and lineage;
control payloads and historical controls are bounded only by the configured
frame byte budget. Nested values are at most 32 levels and the envelope wire 24
containers. Excess depth rejects **before constructing recursively dropped
objects**. Standard executable validators still run; they are not relaxed or
replaced. The wire/probe and typed payload versions are deliberately pinned
together.

## Transports and independently serviceable control

Data and control use **different addresses and owned I/O workers**. A full
data queue cannot occupy a receipt/health buffer. There is one active data
connection and at most two control connections (allowing fenced reconnection).
The receiver's SourceAdmission mailbox supplies the existing bounded producer
metadata and commit operations. Full queues return explicit `Busy`; the sender
holds its input and uses only its bounded retry policy.

`BridgeSink::control()` exposes a cloneable, read-only `BridgeControl`:
`status`, `producer_status`, and `receipt` remain independently callable while
data is pending. `BridgeSource::status()` and `BridgeSink::last_receipt()` expose
bounded local evidence. None proves whole-graph/composition drain. Supervisors
must separately quiesce producers and retain required return/control paths
until obligations are drained or explicitly retained for recovery.
`in_flight` counts the currently serviced data request, not all graph processing.
An unresolved storage/admission timeout keeps `acceptance_unknown` set until
receipt resolution; a false `in_flight` alone never proves a clean boundary.
`connected` describes the current authenticated control connection, not proof
that a remote process remains alive across a partition.

### Explicit composition cuts

Hosts can provide `Arc<BridgeIngressControl>` / `Arc<BridgeEgressControl>` through
the optional normal `control` resource dependency. `resource()` supplies the
actual `Component` resource and cleanup; successful endpoint stop revokes its
lifetime. The controls retain no separate graph registry.

After quiescing an egress and its **entire upstream path**, call
`BridgeEgressControl::finish(timeout)`. It refuses unresolved saved/current inputs,
forbids new local inputs, and uses the actual configured transport/client TLS to
seal its peer. The peer resolves the actual accepted session/sequence/journal
position behind its admission gate; the sender checks that cut against its
saved final receipt. Failure/cancellation does not reopen admission.

`BridgeIngressControl::sealed_cut()` observes an existing cut without creating
one, so an uncoordinated child stop cannot seal an active producer implicitly.
`seal(timeout)` is the explicit local operation. `drain_status(&cut)` and
`wait_drained(&cut, timeout)` validate the receiver lifetime and exact cut:

- Volatile: every accepted envelope has been dequeued by the source into the
  normal graph. The host must then quiesce the source to finish its native
  in-flight forwarding and apply normal downstream drain rules.
- Durable: actual required journal subscriber cursors have acknowledged through
  the cut. Retiring a required subscriber is not accepted as completion.

An empty queue, `in_flight=false`, or an admission receipt alone is never used
as this proof. Cuts are scoped to the running receiver epoch; restart invalidates
old cut observations. No unseal operation exists. Receipt/control traffic stays
available while sealed.

`BridgeEndpointControl::connect(config, role, client_tls)` exposes the same
seal/drain operations from serialized native configuration. Its authenticated
observing connection does not register a producer or replace the sender's data
generation. A local TLS source uses its typed ingress control rather than
pretending its server credential is a client credential.

These are transport-local facts, **not whole-child effects or a completed
composition algorithm**. The host must keep return ingress/control alive,
coordinate every producer cut, and perform normal graph quiescence/drain before
stopping owners. Cycles, unsupported intermediate queues and external effects
still require explicit host qualification.

### Local IPC

- Unix: `{"kind":"unix","data_path":"...","control_path":"..."}`. Paths are at
  most 100 bytes. Parents create/protect the containing directory. Sockets are
  owner-only, peers must match the socket owner's UID, existing paths are never
  unlinked on startup, and cleanup only removes the inode actually owned.
  After an abrupt process exit, an operator/supervisor must verify old process
  ownership before removing a stale socket; the bridge does not guess.
- Windows: `{"kind":"named_pipe","data_pipe":"\\\\.\\pipe\\...","control_pipe":"\\\\.\\pipe\\..."}`.
  The cfg-gated byte-mode adapter rejects remote pipe clients and uses
  first-instance ownership. Access control inherits the hosting process's DACL.
  It needs Windows qualification; Unix tests do not constitute that evidence.

### Authenticated network

```json
{
  "kind": "tls",
  "data_address": "127.0.0.1:7443",
  "control_address": "127.0.0.1:7444",
  "server_name": "analysis.example"
}
```

The same frames and receipt semantics run over `tokio-rustls`/`rustls`; there is
**no plaintext TCP transport**. `TlsResource::client/server` require WebPKI
trust roots, a certificate chain/private key, and the expected peer leaf's
SHA-256 certificate fingerprint. Both directions authenticate certificates,
the client checks the server name, both pin the configured peer, and ALPN is
`drasi-bridge/1`. This reuses existing cryptographic implementations; no custom
cryptography or verification bypass exists. Rotate credentials explicitly with
normal hosting resource replacement.

`TlsResourceRecipe` is the shared host/Server loader, not another certificate
parser in each host. Its strict JSON shape is:

```json
{
  "kind": "bridgeTls",
  "version": 1,
  "role": "client",
  "trust_roots": ["tls/ca.der"],
  "certificate_chain": ["tls/client.der"],
  "private_key": "tls/client-key.der",
  "peer_certificate_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

The illustrated pin is a placeholder, not a usable identity. `validate()` is pure.
`resolve(base)` performs bounded local reads and returns `Arc<TlsResource>`.
Paths may be absolute or relative to the owning host's supplied base; parent
traversal rejects. Inputs are DER certificates and unencrypted PKCS8 DER keys:
1–16 roots, 1–8 chain entries, 64 KiB per certificate, 32 KiB per key, 1 MiB
combined. Files must be regular, non-symlink files. Unix private-key files must
grant no group/other access; opened inode identity is checked. The helper never
serializes key contents into configuration or diagnostics. Private-key ACL
validation for this recipe on non-Unix platforms remains explicitly unsupported.

The Host and Server integrate this normal resource recipe. The Host distinguishes
owned local children from independently owned remote peers: a network failure
does not authorize an unfenced duplicate replacement. Deployment-specific
certificate/secret provisioning, distributed ownership leases, production
telemetry and end-to-end performance/partition qualification are not supplied
by this package. The data credential cannot invoke process lifecycle management.
The package does not provision remote machines or claim hierarchy shutdown
completion.

## Bounds, cleanup and validation

An envelope is limited to 1 MiB by default (maximum 4 MiB). Control frames are
16 KiB. Queue capacity is 16 (maximum 128), with the product of envelope limit
and capacity limited to 64 MiB. These are encoded-payload/queue bounds, **not**
an exact heap-size promise: typed objects, bounded provenance, framing and the
one sender retry row add bounded overhead. Limits reject instead of fragmenting
or expanding capacity. Wire/configuration limit changes are contract changes.

I/O workers are acquired through `spawn_owned_worker` and retained through
`join_owned_worker_gracefully`. Cancellation/timeouts do not discard unjoined
handles. Always await component/instance stop before replacement. Drop requests
cancellation but is not a join, durable receipt, rollback, or storage cleanup.
No extra Tokio runtime, process worker or permanent background server is created.

Run focused validation from the workspace after package registration:

```sh
cargo test -p drasi-computation-bridge-common -p drasi-computation-source-bridge -p drasi-computation-sink-bridge
cargo clippy -p drasi-computation-bridge-common -p drasi-computation-source-bridge -p drasi-computation-sink-bridge --all-targets -- -D warnings
```

Tests use package-local `.bridge-test-data/` directories and remove their own
files. Network tests require the installed `openssl` executable only to generate
short-lived test certificates; no credentials or certificates are checked in.

### Compatibility coverage

The shared library's protocol unit tests and `bridge` integration-test binary
cover current-thread operation and both separate endpoint crates. The
[Host network profile](../../../instance-host/README.md#qualified-unix-platforms-and-prepared-network-peer)
adds actual mutual-TLS macOS/Linux-VM traffic, receipt loss/delay,
version/authentication rejection, persistent reopen and independent peer
ownership. The Host process suite qualifies its explicit returning cuts and
hierarchy shutdown on both Unix platforms. None of these claims qualifies
Windows, physical cross-machine production partitions or production performance.

Exact tests:

```text
protocol::tests::partial_reads_and_writes_preserve_a_whole_frame
protocol::tests::lengths_versions_truncation_and_trailing_data_reject
protocol::tests::exact_encoder_limit_is_enforced_before_writing
codec::portable_standard_schemas_preserve_exact_binary_context_and_transactions
codec::namespace_and_wire_versions_do_not_load_or_substitute_a_validator
codec::hostile_messagepack_is_rejected_iteratively_before_deserialization
codec::opaque_messagepack_depth_and_lineage_are_checked_before_recursive_construction_or_drop
codec::hostile_fixture_is_the_canonical_node_encoding_and_depth_limits_are_semantic
codec::exact_envelope_byte_limit_and_annotation_limits_reject_next_unit
codec::rich_canonical_patch_partial_and_removal_records_cross_exactly
codec::rich_graph_values_keep_namespaces_and_value_limits
codec::expressions_and_awaiting_values_are_inert_graph_data_with_limits
codec::current_graph_controls_are_rejected_not_silently_ignored
codec::historical_controls_are_provenance_bounded_only_by_the_frame_budget
codec::rich_source_transaction_values_keep_validation_and_namespaces
codec::complete_transaction_markers_are_scoped_to_their_own_change_set
integration::factories_exchange_each_portable_schema_between_two_normal_instances
integration::durable_factories_use_real_acknowledged_pipes_and_preserve_complete_transactions
integration::full_volatile_queue_keeps_receipts_and_control_live_and_retries_fifo
integration::lost_wire_ack_and_real_two_sided_reopen_resolve_exact_persisted_mapping
integration::receiver_state_loss_and_missing_sender_state_never_create_fresh_input
integration::malformed_peer_and_stale_connections_do_not_stop_the_receiver_or_control_lane
integration::transactional_sender_mapping_persists_gaps_rejects_expiry_and_configuration_changes
integration::sealed_volatile_cut_requires_actual_queue_handoff_and_revokes_old_lifetimes
integration::durable_cut_waits_for_actual_required_subscriber_acknowledgement
tls::mutual_tls_uses_the_same_protocol_and_rejects_wrong_peer_pins_and_plaintext
```
