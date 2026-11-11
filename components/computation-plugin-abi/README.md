# Native ComputationGraph ABI

This dependency-free crate freezes the C layouts for native computation plugins.
The ABI version is **2.0.0**, independent of crate releases and the
Source/Reaction/Bootstrap SDK **0.17.0** (which also accepts 0.16 fast-mode plugins).
Its two symbols are
`drasi_computation_plugin_metadata` and `drasi_computation_plugin_entry`.
Do not call a legacy registration function to interpret native metadata.
The native payload contract is **wire version 2**: UUID component identities,
bounded MessagePack with bulk binary buffers and binary computation envelopes.
Wire version 1 is rejected; rebuild the host and plugins together. ABI 2 changes
the family magic to `DRASICG2`, the header major to 2 and the factory table layout.
It does not change BinaryEnvelopeCodec/EnvelopeCodec serialization version 2.

Headers must match the family magic, version and exact structure size before
reading any subsequent field. Missing metadata, unsupported capabilities and
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

## ABI 2 lifecycle and update planning

`FactoryVTable::plan_update` is an additional
`unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Reply` callback. Its input is
JSON `UpdateRequest { current: CreateRequest, proposed: CreateRequest }`; success
returns JSON `ComponentUpdatePlan { mode, data }`. Both are bounded by
`MAX_METADATA_BYTES` (1 MiB). Planning inspects configuration only, without
construction, I/O or activation. Unsupported changes fail explicitly.

Mutable operations still use `ComponentVTable::begin` and producer-owned,
caller-polled operation handles. Lifecycle hooks take empty payloads (except the
existing credential-bound START). Added/renamed opcodes are:

| Opcode | Value | Meaning |
|---|---:|---|
| `PAUSE_PROCESSING` | 7 | Temporary processing pause, retaining activation |
| `DELETE_OWNED_RESOURCES` | 17 | Explicit permanent owned-resource deletion |
| `INITIALIZE` | 24 | Once per object, before its first START |
| `DISPOSE` | 25 | Awaited final instance cleanup after positive STOP cleanup |
| `RESUME_PROCESSING` | 26 | Resume a completed temporary pause |
| `APPLY_UPDATE` | 27 | Execute an approved retained-instance update |

`APPLY_UPDATE` uses bounded named MessagePack
`ApplyUpdateRequest { operation_id, request: UpdateRequest, plan }`. Recreate
cannot be applied to an instance. Factory metadata declares unique
`capabilities.update_modes`; empty means no update support. These declarations
are not permission to discard state: the graph owns data-boundary, storage and
downstream safety checks.

Owner-approved reconstruction intent uses a separate named MessagePack creation
envelope: `update_creation_version: 1`, `request: CreateRequest`,
`directive: UpdateDirective { operation_id, plan }`, and optional
`activation_credentials_version: 1` for the negotiated credentials path.
Only declared Recreate plans and nonempty operation IDs are valid. This directive
does not authorize state loss or replace the graph's data-safety checks.
Ordinary construction and pure planning descriptions have no directive.

Successful duplicate transitions acknowledge completion without repeating hooks.
Incomplete initialization must be disposed, not retried on the same object;
partial start needs stop. Stop/disposal/update cancellation does not roll back or
complete cleanup, and cannot authorize restart. Retained cleanup can be retried.
Disposal fences and settles independent calls/readers; synchronous release is not
its replacement. Permanent deletion is separate and explicit, after positive
activation cleanup and before disposal. The old destructive and service-quiesce
operation names have no compatibility aliases in ABI 2.

## Optional services, version 1

`drasi_computation_plugin_services_v1` is an optional, independently versioned
extension. The two base entry points, base vtables, ABI version, wire version and
strict base metadata remain unchanged. Matching wire-version-2 libraries may omit
this symbol; loading does not grant recovery support.
An extension without the base entry points is malformed, not a legacy plugin.

`PluginServicesV1` has a checked header, version/reserved fields, per-factory
capability discovery and a service-aware create callback. Only single-output
sources may declare admission. `SourceAdmissionV1` contains a producer-owned,
revocable context with retain/release, bounded caller-polled requests, immutable
identity description and synchronous failure reporting. Plugins copy and retain
the table before returning from creation. All data crosses as bounded
MessagePack or, for a failure reason, at most 4096 UTF-8 bytes.

Admission, producer registration/retirement, status and receipt lookup share the
host graph's bounded service. At most 16 native operation handles are outstanding;
cancelling/dropping a handle releases its slot. Listener failure reporting is
independent of those slots and preserves the first unobserved failure. Requests
perform no host storage work under a plugin runtime: the host graph validates
and commits into its actual outgoing QoS channel. Replacement revokes retained
capabilities before releasing the remote component. There is no unrestricted
store, transaction owner or commit method in this interface.

## Optional recovery, version 1

`drasi_computation_plugin_recovery_v1` is independent of service-v1 and leaves all
existing layouts and metadata unchanged. `PluginRecoveryV1` negotiates per-factory
source progress, constructs with a borrowed `SourceProgressV1`, and inspects the
immutable instance retention declaration. That declaration is not proof of a
transaction owner; the host retains and validates its actual local resource.

`SourceProgressV1` offers retain/release, bounded identity/snapshot reads and owned
`ProgressSubscriptionV1` handles. Subscriptions have release, snapshot and
caller-polled wait operations. Frames are at most 1 MiB and 4,096 combined
checkpoint/transport entries; a binding permits 16 subscriptions, each with one
active wait. Reads mark only the encoded observation; cancellation never consumes
an observation. Revocation wakes pending waits and rejects subsequent calls.
There is no storage I/O, forwarding worker, writable progress object or commit
operation. Admission and separate replay progress are mutually exclusive bindings.
An extension without base entry points is malformed, not a legacy fallback.

## Optional consumer progress, version 1

`drasi_computation_plugin_consumer_progress_v1` separately negotiates a producer's
complete consumer set. `PluginConsumerProgressV1` supplies per-factory discovery,
construction with `ConsumerProgressV1`, and immutable upstream-retention inspection.
An advertising factory requires the binding and must reject unbound creation.
Only source and transformer producers support it; single-owner recovery cannot
be combined with this capability. Base ABI 2.0, wire 2 and recovery-v1 are unchanged.

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

`drasi_computation_plugin_bootstrap_v1` negotiates query-owned bootstrap factories,
bounded snapshot streaming and borrowed initialization-state calls. Preparation,
snapshot polling, completion and stop retain their provider/generation ownership.
The query owns the final state/watermark commit; a stream end is not that commit.

`drasi_computation_plugin_consumer_v1` negotiates per-factory external/transactional
consumer mode, construction, inspection, batch/operation calls and batch release.
It reuses component lifecycle handles and the existing revocable transaction
request layout. The Host owns progress and commit; the plugin cannot acknowledge
upstream transport. A full batch uses the existing 64 MiB wire bound, while each
subsequent generation/index/key request is limited to 4096 bytes. Batch release
rejects stale generations or active calls. Explicit retryable handler failures use
the extension's `RETRYABLE` status, not an empty success response.

Both extensions leave base tables, strict metadata, ABI 2.0 and wire 2 unchanged.
Only negotiated, bound factories may use them; missing services never imply
recovery guarantees. See the SDK's authoring and lifecycle contracts.

## Optional output reads and follow, version 2

`drasi_computation_plugin_output_reads_v2` and
`drasi_computation_plugin_output_follow_v2` are independent extensions; base ABI
2.0, wire 2 and every other table are unchanged. A factory declares the output
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
