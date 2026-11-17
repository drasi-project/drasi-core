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

The sender declares what it requires, so the graph checks it when the edge is
bound: a durable sender keys its mapping on the producer's durable identity and
declares exactly-once input from a durable producer, a durable progress and a
durable `state` dependency, so a volatile or unidentified producer is refused at
wiring (the sender node carries a terminal binding failure). A volatile sender
declares an unkeyed input.

Delivery failures follow the shared
[delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures):
`Busy`, `Closed`, `AcceptanceUnknown` and `StaleGeneration` are transient; a
receiver that rejects the input (`Invalid`, `Incompatible`) is permanent and
follows `onPermanentFailure`; lost, conflicting or expired link state ends the
activation (the node fails).
