# Native bridge source

`drasi-computation-source-bridge` contains the native `BridgeSource` implementation
and only the `drasi.bridge/source` factory (implementation `"1"`, configuration
`1`). It emits the configured portable schema on `out`.

This endpoint retains **static Rust hosting**, not a dynamic C ABI export:

```rust,ignore
registry.register(drasi_computation_source_bridge::factory())?;
```

Depend only on this crate to use the source. It re-exports its `BridgeConfig`,
portable schema/profile/transport types, ingress control and TLS resource types.
It does not depend on or register the bridge sink component.

The [shared bridge contract](../../common/bridge) covers configuration, volatile
versus durable admission, namespace preservation, transport limits, TLS, controls
and shutdown. Durable mode requires the actual configured host `QosChannel`;
construction does not bind sockets or start workers. Source stop retains owned
listeners and workers through completed cleanup.
