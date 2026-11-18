# Native ComputationGraph ABI

This dependency-free crate freezes the C layouts for native computation plugins.
The ABI version is **5.0.0**, independent of crate releases and the
Source/Reaction/Bootstrap SDK **0.17.0** (which also accepts 0.16 fast-mode plugins).
Its two symbols are
`drasi_computation_plugin_metadata` and `drasi_computation_plugin_entry`.
Do not call a legacy registration function to interpret native metadata.
The native payload contract is **wire version 5**: UUID component identities,
bounded MessagePack with bulk binary buffers and binary computation envelopes.
Other wire versions are rejected. The family magic is `DRASICG5` and the header
major is 5. This does not change BinaryEnvelopeCodec/EnvelopeCodec serialization
version 2.

Wire 5 descriptors carry a port `kind` (`Data` or `Bootstrap`, serialized only
for bootstrap ports). Bootstrap ports are optional graph connections that carry
bootstrap sessions between providers, sources and consumers. Control
notifications include a typed `BootstrapRequest` that addresses one bootstrap
session to its consumer edge.

Headers must match the family magic and major before reading any subsequent
field. A host accepts the same major with an older or equal minor, whose
structure size must match that minor's layout, and rejects a newer minor or
another major before invoking any callback. Tables grow only by appending fields
in a later minor. Missing metadata, unsupported capabilities and
unknown wire versions are errors. Enum-like ABI fields are integers, not Rust
enums with invalid discriminants.

Borrowed bytes last for one call. Owned buffers and opaque handles are released
only by their producer's function. Operation poll/wake/cancel/release transfers
no Rust futures, trait objects, task-local context, `Arc` or `Bytes`. Retained
owned buffers stay immutable and valid independently of their originating
operation until release; ownership can move between threads and release must be
callable on any thread. This permits decoding directly from the producer's bytes
without transferring a Rust allocation across the ABI. Retained
callbacks remain callable after cancellation until their final release.
Cancellation is not success or rollback. Polling is cooperative and never waits
for I/O. Every exported function and callback must contain Rust panics.

Native libraries are pinned for process lifetime. Hot unloading is unsupported.
Native code is trusted in-process code, not a memory or security sandbox.

## Lifecycle and update management

Mutable operations use `ComponentVTable::begin` and producer-owned,
caller-polled operation handles. Lifecycle operations carry a bounded named
MessagePack `LifecycleRequest { version: 1, operation_id, component_id,
generation, deadline_ms?, required?, credentials?, stop_mode?, dispose?, update? }`.
`deadline_ms` is cooperative: expiry is neither completion nor rollback. An
unknown entry in `required` rejects the operation. Lifecycle opcodes are:

| Opcode | Value | Meaning |
|---|---:|---|
| `START` | 1 | Begin an activation after INITIALIZE; credentials only for bound factories |
| `STOP` | 2 | End an activation, including a partial START or a pause; `stop_mode` is `drain` or `abort` |
| `PAUSE` | 7 | Temporary pause retaining the activation; fails `UNSUPPORTED` rather than succeeding as a no-op |
| `INITIALIZE` | 24 | Once per object, before its first START; failure requires DISPOSE |
| `DISPOSE` | 25 | Final retirement after STOP, including partial initialization |
| `RESUME` | 26 | Resume a completed pause, not another START |
| `APPLY` | 28 | Management apply at an accepted Pause or Stop boundary |

DISPOSE carries `DisposeRequest { reason, intent, attempt }`. `reason` is
`shutdown`, `removal`, `replacement` or `initializationFailure`; `intent` is
`preserve` or `erase`, and shutdown or initialization failure must preserve.
`erase` is permanent removal of state the component owns, requested only of
factories declaring `erases_owned_state`; there is no separate deletion opcode.
Retries repeat the same reason and intent, so erasure must be idempotent.
For a factory bound to activation credentials, an `erase` DISPOSE also carries
`credentials`, resolved anew for that attempt, because erasing may need the
external system even when the object never started in this process.

`FactoryVTable::assess` is the pure factory planner:
`unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Reply`. Its input is JSON
`UpdateRequest { current: CreateRequest, proposed: CreateRequest }`; success
returns a JSON `Assessment`, either `{"outcome": "unsupported", "reason"}` or
`{"outcome": "supported", "boundary": "pause" | "stop" | "replace", "data",
"fingerprint"?}`. Both are bounded by `MAX_METADATA_BYTES` (1 MiB). Planning
inspects configuration only, without construction, I/O or activation; a failure
to assess is an error, never an implicit Replace. Boundaries are stored as
`capabilities.update_modes` `InPlace`, `Restart` and `Recreate`; empty means no
update support.

`drasi_computation_plugin_management_v1` is an optional extension
(`PluginManagementV1`) declared exactly when `capabilities.management` is set,
which is required when `update_modes` names Pause or Stop. Its synchronous,
side-effect-free `assess` uses the same JSON request/response with live instance
facts and fails `BUSY` while another operation is active, or `UNSUPPORTED` so the
host uses the factory planner. Its `apply` begins an APPLY operation whose
`LifecycleRequest.update` is `ApplyUpdateRequest { operation_id, request, plan,
fingerprint? }`. Replace is never applied to an instance: the host disposes the
old object and constructs anew. A failed apply reports `{"apply": "refused" |
"failed" | "damaged", "nowRequires"?: boundary}` in its failure data. Refused and
failed leave the object unchanged; damaged retires it. An interrupted apply is
never retried on the same object. These declarations are not permission to
discard state: the graph owns data-boundary, storage and downstream safety checks.

Owner-approved reconstruction intent uses a separate named MessagePack creation
envelope: `update_creation_version: 1`, `request: CreateRequest`,
`directive: UpdateDirective { operation_id, plan }`, and optional
`activation_credentials_version: 1` for the negotiated credentials path.
Only declared Recreate plans and nonempty operation IDs are valid. This directive
does not authorize state loss or replace the graph's data-safety checks.
Ordinary construction and pure planning descriptions have no directive.

Successful duplicate transitions acknowledge completion without repeating hooks.
Incomplete initialization must be disposed, not retried on the same object;
partial start needs stop. Stop/disposal/apply cancellation does not roll back or
complete cleanup, and cannot authorize restart. Retained cleanup can be retried.
Disposal fences and settles independent calls/readers; synchronous release is not
its replacement.

## Host services: declared once, bound together

A factory declares the Host services its components are constructed with in
its manifest, `capabilities.services` (`source_progress`, `consumer_progress`,
`source_admission`, `output_snapshot_reads`, `consumer`, `sink_batching`,
`bootstrap_coordination`, `activation_credentials`; indexed processing is
`capabilities.indexed_processing`). Hosts read the declaration from the
manifest; there are no per-factory capability probes and no per-service create
callbacks. Manifest validation refuses role violations and contradictory
combinations (single-owner with consumer-group progress, admission with a
progress owner, a consumer with Host-owned batching, durable output with
consumer progress, coordination without exactly one progress mode) at load.
An extension table a declared service needs (recovery, consumer progress,
consumer, batching, bootstrap coordination, activation credentials) must be
present then, or the library is refused before its entry runs.

`FactoryVTable::create` is the single constructor. Its non-null
`services::ServiceBindingsV1` table carries every Host service bound to this
construction: `admission`, `source_progress`, `consumer_progress`,
`output_snapshots`, `processing` (a `ScheduledProcessingV1` prefix when
scheduling is declared), the source's retained `coordinator`, and a nonzero
`consumer` when Host-owned delivery is bound. Null means unbound. The producer
refuses an undeclared binding and a missing required one (consumer progress,
indexed processing, consumer delivery). Each table is borrowed for the call;
the producer retains what it keeps.

### Source admission

`SourceAdmissionV1` contains a producer-owned,
revocable context with retain/release, bounded caller-polled requests, immutable
identity description and synchronous failure reporting. Plugins copy and retain
the table before returning from creation. All data crosses as bounded
MessagePack or, for a failure reason, at most 4096 UTF-8 bytes.

Admission, producer registration/retirement, status and receipt lookup share the
host graph's bounded service. At most 16 native operation handles are outstanding
(`MAX_OUTSTANDING_REQUESTS`; the SDK's `NativeAdmission` queues its callers below
it); cancelling/dropping a handle releases its slot. An ADMIT with `wait` waits
for graph activation, mailbox and journal capacity instead of answering `Busy` or
`CapacityExhausted`. Producer metadata is answered even while the source has no
activation (parked or stopped); only ADMIT requires one. Listener failure reporting is
independent of those slots and preserves the first unobserved failure. Requests
perform no host storage work under a plugin runtime: the host graph validates
and commits into its actual outgoing QoS channel. Replacement revokes retained
capabilities before releasing the remote component. There is no unrestricted
store, transaction owner or commit method in this interface.

## Recovery, version 1

`drasi_computation_plugin_recovery_v1` is required when a factory declares
`source_progress`. The bound `SourceProgressV1` arrives in `ServiceBindingsV1`;
`PluginRecoveryV1` only inspects the immutable instance retention declaration. That declaration is not proof of a
transaction owner; the host retains and validates its actual local resource.

`SourceProgressV1` offers retain/release, bounded identity/snapshot reads and owned
`ProgressSubscriptionV1` handles. Subscriptions have release, snapshot and
caller-polled wait operations. Frames are at most 1 MiB and 4,096 combined
checkpoint/transport entries; a binding permits 16 subscriptions, each with one
active wait. Reads mark only the encoded observation; cancellation never consumes
an observation. Revocation wakes pending waits and rejects subsequent calls.
There is no storage I/O, forwarding worker, writable progress object or commit
operation. Admission and separate replay progress are contradictory declarations.
An extension without base entry points is malformed, not a legacy fallback.

## Consumer progress, version 1

`drasi_computation_plugin_consumer_progress_v1` is required when a factory
declares `consumer_progress` (a producer's complete consumer set). The bound
`ConsumerProgressV1` arrives in `ServiceBindingsV1` and is required for that
factory; `PluginConsumerProgressV1` inspects the immutable upstream-retention
declaration and position policy. Only source and transformer producers declare
it; single-owner recovery and consumer progress are contradictory.

The identity fixes the graph, producer and 1-256 distinct consumer IDs. Each
snapshot carries every member's existing recovery-v1 snapshot as a binary frame,
preserving opaque positions, readiness, failures, reset generations and durability
independently. Unknown, duplicate, missing or changed members fail rather than being
omitted. The **whole set** is bounded to 1 MiB and 4,096 combined checkpoint and
transport entries. No aggregate cursor or consumer durability is synthesized.

Producer-owned contexts use retain/release; owned subscriptions release once.
There are at most 16 retained subscriptions per binding and one pending wait each.
Synchronous snapshot callbacks perform no I/O. Failed encoding observes no member,
and cancelled or completed waits do not consume updates. Stop fences the binding
before cleanup, wakes pending waits and permanently revokes that generation's
subscriptions. A successful restart permits new subscriptions, not rebinding old
ones. Failed/cancelled activation or cleanup requires successful stop before
reactivation. Dropping the binding permanently revokes retained readers.
No forwarding task, graph owner, writable progress object or commit operation
crosses this interface.

The same extension optionally returns a borrowed `InputPositionPolicyV1`. Its
producer-retained immutable context describes a validated checkpoint key and
compares event/handled positions, each at most 65,536 bytes. The callback returns
one MessagePack boolean; invalid positions and comparison failures/panics remain
errors. It never borrows mutable component state or performs I/O, so comparisons
remain available while data processing is pending. The graph owns installation
and revocation; a retained immutable comparator can outlive the component handle.
Absence is valid and does not imply any generic ordering.

## Optional bootstrap and consumer services, version 1

`drasi_computation_plugin_bootstrap_v1` negotiates bootstrap factories,
bounded snapshot streaming and borrowed initialization-state calls. The Host
publishes each bootstrap factory as a graph component with the
`BootstrapProvider` role; a provider instance serves one bootstrap session for
the consumer named by the session request. Preparation,
snapshot polling, completion and stop retain their provider/generation ownership.
The query owns the final state/watermark commit; a stream end is not that commit.

`drasi_computation_plugin_consumer_v1` is required when a factory declares
`consumer` (external or transactional mode). Construction goes through the
single create entry with `consumer` set; the extension carries inspection,
batch/operation calls and batch release.
It reuses component lifecycle handles and the existing revocable transaction
request layout. The Host owns progress and commit; the plugin cannot acknowledge
upstream transport. A full batch uses the existing 64 MiB wire bound, while each
subsequent generation/index/key request is limited to 4096 bytes. Batch release
rejects stale generations or active calls. A handler failure crosses as an ordinary
failure status whose JSON carries the sink's delivery-failure class (`delivery`:
transient with an optional retry-after, or permanent), never an empty success
response.

`drasi_computation_plugin_logging_v1` (optional, `logging::HostLoggerV1`) is
called once by the host right after loading a library. The plugin SDK installs
a `log` logger that forwards each enabled record (level, target, message) to
the host's callback, so a loaded library's diagnostics reach the host's logger.
The table carries the host's level at load time; it is host-owned and valid for
the process lifetime.

Both extensions leave base tables, strict metadata, ABI 5.0 and wire 5 unchanged.
Only negotiated, bound factories may use them; missing services never imply
recovery guarantees. See the SDK's authoring and lifecycle contracts.

## Optional output reads and follow, version 2

`drasi_computation_plugin_output_reads_v2` and
`drasi_computation_plugin_output_follow_v2` are independent extensions; base ABI
5.0, wire 5 and every other table are unchanged. A factory declares the output
ports it serves in manifest `capabilities.output_snapshots`, `output_replays` and
`output_follows` (empty lists are omitted); the Host requires the matching
extension exactly then.

`PluginOutputReadsV2::open` returns one producer-owned `OutputReadHandle` per
declared (port, capability), with `request(op, input)` returning an ordinary
operation (`SNAPSHOT`, `COMMITTED_HEAD`, `REPLAY_AFTER`). The handle's `transfer`
callback opens an independently owned, activation-fenced paged transfer for a
large immutable snapshot/replay observation. `PluginOutputFollowV2::open`
returns a provider handle whose `begin(limits)` writes a **lease handle** and the
operation that opens it; the lease has its own `next(ack)` operations and a
synchronous `release`. Replies are bounded MessagePack: 16 MiB, 16,384 snapshot
records, 1,024 replay envelopes, 16 reads and 16 leases per component. Provider
results, including typed errors, are an `Outcome` inside a successful reply;
lifecycle and transport failures use ABI statuses (`NOT_ACTIVE` 0x2001 while not
started, `CLOSED` once retired). Nothing is truncated: a larger single-message
answer is an explicit limit outcome; paged transfers bound each page rather than
the whole observation. Handles and leases are released once by their holder; a
component's stop, update, disposal or release fences them and ends leases at once.
`next` carries the last observed item index so an unacknowledged item is resent
rather than lost when a cancellation races completion.
