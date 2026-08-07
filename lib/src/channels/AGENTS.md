# AGENTS.md: drasi-core/lib/src/channels

## The payload rule, and why it exists

Event types defined here cross the dynamic plugin FFI boundary **by value as serialized MessagePack bytes** carried inside `#[repr(C)]` envelopes. The serialization mapping lives in `components/plugin-sdk/src/ffi/payload.rs`: some types serialize directly, others map through wire DTOs there. A field added to an event type is not on the wire until that mapping carries it, and nothing fails to compile if you forget. Simple enums cross as `#[repr(C)]` mirrors instead.

Do **not** transfer these `repr(Rust)` types as opaque `Box::into_raw` / `Box::from_raw` pointers. `repr(Rust)` has no stable layout across cdylib boundaries, and types like `bytes::Bytes` and `Arc<str>` carry module local pointers. That combination is undefined behaviour and caused the heap corruption in issue #602. The SDK version check does not protect you here: it validates the `#[repr(C)]` envelope ABI, not the payload layout.

The envelope is the part with a stable layout. It carries a pointer, a length, and a drop function for the serialized bytes. The distinction matters: seeing a raw pointer in the vtable does not mean a Rust value is being handed across.

For which types cross and in what form, read the `Ffi*` envelope definitions in `components/plugin-sdk/src/ffi/vtables.rs`. That is the authority and it stays current.

## What to update when changing these types

**Event payload fields.** Changing fields on a serialized payload does not alter the envelope layout, but host and plugin must still agree on how to decode it, so bump `FFI_SDK_VERSION` in `components/plugin-sdk/src/ffi/metadata.rs`. If metadata is extracted out of a payload, for example a source id, update that extraction in `components/plugin-sdk/src/ffi/vtable_gen.rs`.

**New enum variants.** Add the variant to the `#[repr(C)]` mirror in `components/plugin-sdk/src/ffi/types.rs`, update the conversions in `components/plugin-sdk/src/ffi/vtable_gen.rs` and the matching host proxies under `components/host-sdk/src/proxies/`, then bump `FFI_SDK_VERSION`.

**`SubscriptionResponse`.** This one is deconstructed into FFI parts on the plugin side and reconstructed on the host side, so it has four touchpoints rather than the usual pattern: the construction in `vtable_gen.rs`, the struct in `vtables.rs`, the reconstruction in `components/host-sdk/src/proxies/source.rs`, and the channel types rebuilt in `components/host-sdk/src/proxies/change_receiver.rs`.
