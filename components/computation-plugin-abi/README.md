# Native ComputationGraph ABI

This dependency-free crate freezes the C layouts for native computation plugins.
The ABI version is **1.0.0**, independent of crate releases and the
Source/Reaction/Bootstrap SDK **0.17.0** (which also accepts 0.16 fast-mode plugins).
Its two symbols are
`drasi_computation_plugin_metadata` and `drasi_computation_plugin_entry`.
Do not call a legacy registration function to interpret native metadata.
The native payload contract is **wire version 2**: bounded MessagePack with bulk
binary buffers and binary computation envelopes. The initial, unreleased
JSON-envelope wire version 1 is not accepted. The C table layouts are unchanged.

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

## Optional services, version 1

`drasi_computation_plugin_services_v1` is an optional, independently versioned
extension. The two base entry points, base vtables, ABI version, wire version and
strict base metadata remain unchanged. Genuine ABI 1.0 binaries without this
symbol retain their existing behavior; loading does not grant recovery support.
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

Both extensions leave base tables, strict metadata, ABI 1.0 and wire 2 unchanged.
Only negotiated, bound factories may use them; missing services never imply
recovery guarantees. See the SDK's authoring and lifecycle contracts.
