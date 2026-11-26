# Bridge sink

`drasi-computation-sink-bridge` is the plugin exporting the `drasi.bridge/sink`
factory (implementation `"1"`, configuration `1`) and its `BridgeSink`
(`NativeSink`). It consumes the standard graph schema on `in`.

Link it statically through the host SDK, or load the library built with the
`dynamic-plugin` feature (`libdrasi_computation_sink_bridge`):

```rust,ignore
NativePlugin::from_definition(drasi_computation_sink_bridge::plugin()?)?
    .register_factories(&mut registry)?;
```

The [shared bridge contract](../../common/bridge) covers configuration, volatile
versus durable acceptance, sender mapping recovery, namespace preservation,
transports, TLS, explicit cuts and cleanup. The durable profile requires
`mappingDirectory`: the sender's mapping file is saved before first
transmission, and a receiver receipt proves admission, never downstream
completion or whole-graph drain. The `tls` configuration key carries the client
identity for the `tls` transport.

The sender completes an input as `Handled` in both profiles, once the receiver
has admitted it. It declares what it requires, so the graph checks it when the
edge is bound: a durable sender keys its mapping on the producer's durable
identity and declares exactly-once input from a durable producer and a durable
progress, so a volatile or unidentified producer is refused at wiring (the
sender node carries a terminal binding failure). A volatile sender declares an
unkeyed input.

The sink has no worker: Pause and Resume succeed because the Host calls no
`handle` while paused. Stop releases the connection and the mapping lock.
Erasing disposal removes the durable mapping file idempotently.

Delivery failures follow the shared
[delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures):
`Busy`, `Closed`, `AcceptanceUnknown` and `StaleGeneration` are transient; a
receiver that rejects the input (`Invalid`, `Incompatible`) is permanent and
follows `onPermanentFailure`; lost, conflicting or expired link state ends the
activation (the node fails).
