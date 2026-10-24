# Native bridge sink

`drasi-computation-sink-bridge` contains the native `BridgeSink` implementation
and only the `drasi.bridge/sink` factory (implementation `"1"`, configuration
`1`). It consumes the configured portable schema on `in`.

This endpoint retains **static Rust hosting**, not a dynamic C ABI export:

```rust,ignore
registry.register(drasi_computation_sink_bridge::factory())?;
```

Depend only on this crate to use the sink. It re-exports its `BridgeConfig`,
portable schema/profile/transport types, egress/receipt controls and TLS resource
types. It does not depend on or register the bridge source component.

The [shared bridge contract](../../common/bridge) covers configuration, volatile
versus durable acceptance, sender mapping recovery, namespace preservation,
transports, TLS, explicit cuts and cleanup. Durable mode requires an actual host
index provider. The saved input/mapping precedes first transmission; a receiver
receipt proves admission, never downstream completion or whole-graph drain.
