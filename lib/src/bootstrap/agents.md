# agents.md — drasi-core/lib/src/bootstrap

## FFI Boundary Warning

The `BootstrapProvider` trait and supporting types in this directory cross the dynamic
plugin FFI boundary. Bootstrap providers are unique because they involve **cross-plugin
communication**: a bootstrap plugin (Plugin A) provides data to a source plugin (Plugin B)
mediated by the host.

### Types that cross FFI

| Type | How it crosses | FFI wrapper |
|------|---------------|-------------|
| `BootstrapProvider` trait | Wrapped as `BootstrapProviderVtable` (plugin→host) AND reverse-wrapped as vtable (host→plugin) | Both directions |
| `BootstrapRequest` | Deconstructed into individual FFI args (query_id, node_labels, etc.) | Multiple `FfiStr` + `*const FfiStr` arrays |
| `BootstrapContext` | Deconstructed into individual FFI args (server_id, source_id) | Multiple `FfiStr` args |
| `BootstrapEvent` | Serialized to MessagePack bytes (`BootstrapEventPayload`) and transferred as a `payload_ptr` + `payload_len` buffer freed via `payload_drop_fn`; never a reinterpreted `repr(Rust)` pointer (issue #602) | `FfiBootstrapEvent` + `payload.rs::consume_bootstrap_event` |
| `BootstrapResult` | Delivered exactly once through a push-based result receiver (null result = provider ended without one; negative `event_count` = failure, with optional error text) | `FfiBootstrapResult` via `FfiBootstrapResultReceiver` |

### Cross-plugin bootstrap flow

```
Source Plugin B calls set_bootstrap_provider(provider)
  → Host wraps host-side BootstrapProvider into BootstrapProviderVtable
  → Plugin B stores vtable, calls vtable.bootstrap_fn() when subscribing
  → bootstrap_fn starts the provider on its own thread and returns an
    FfiBootstrapStream immediately (FfiBootstrapReceiver for events +
    FfiBootstrapResultReceiver for completion)
  → Plugin B consumes both via BootstrapStreamConsumer / wrap_result_receiver
    (plugin-sdk ffi/bootstrap_stream.rs); every link is bounded, so consumer
    backpressure stalls the provider (issue #686), and cancellation is
    achieved by dropping the receiver
  → When the provider is Bootstrap Plugin A, the host-side provider is
    BootstrapProviderProxy, which consumes Plugin A's FfiBootstrapStream
    the same way
```

### What to update when changing `BootstrapProvider`

1. **FFI vtable** — `components/plugin-sdk/src/ffi/vtables.rs` → `BootstrapProviderVtable`

2. **Plugin-side vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - `build_bootstrap_provider_vtable()` — host→plugin direction
   - The function that wraps a `BootstrapProvider` impl into vtable fn pointers

3. **Plugin-side proxy** — `components/plugin-sdk/src/ffi/bootstrap_proxy.rs`
   - `FfiBootstrapProviderProxy` — plugin-side wrapper that calls vtable to reach host

4. **Host-side proxy** — `components/host-sdk/src/proxies/bootstrap_provider.rs`
   - `BootstrapProviderProxy` — host-side wrapper for plugin-provided bootstrap providers

5. **Shared stream consumer** — `components/plugin-sdk/src/ffi/bootstrap_stream.rs`
   - `BootstrapStreamConsumer` / `wrap_result_receiver` — consume the
     `FfiBootstrapStream` returned by `bootstrap_fn` (shared by both proxies)

6. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs` → `FFI_SDK_VERSION`

### What to update when changing `BootstrapRequest` or `BootstrapContext`

These types are NOT passed as opaque pointers — they are deconstructed into individual
`FfiStr` arguments. If you add fields:

1. **Vtable function signature** — `components/plugin-sdk/src/ffi/vtables.rs` →
   `BootstrapProviderVtable.bootstrap_fn` — add the new parameter

2. **Plugin-side vtable gen** — `components/plugin-sdk/src/ffi/vtable_gen.rs` →
   the `bootstrap_fn` implementation that deconstructs `BootstrapRequest`

3. **Plugin-side proxy** — `components/plugin-sdk/src/ffi/bootstrap_proxy.rs` →
   `FfiBootstrapProviderProxy.bootstrap()` that calls the vtable with new args

4. **Host-side proxy** — `components/host-sdk/src/proxies/bootstrap_provider.rs` →
   `BootstrapProviderProxy.bootstrap()` that constructs `FfiStr` args

### What to update when changing `BootstrapPluginDescriptor`

1. **FFI vtable** — `components/plugin-sdk/src/ffi/vtables.rs` → `BootstrapPluginVtable`
2. **Vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
3. **Host proxy** — `components/host-sdk/src/proxies/bootstrap_provider.rs` → `BootstrapPluginProxy`
