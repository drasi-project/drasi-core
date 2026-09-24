# Native ComputationGraph ABI

This dependency-free crate freezes the C layouts for native computation plugins.
The ABI version is **1.0.0**, independent of crate releases and the unchanged
Source/Reaction/Bootstrap SDK **0.15.0**. Its two symbols are
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
