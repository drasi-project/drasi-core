# Bridge source

`drasi-computation-source-bridge` is the plugin exporting the `drasi.bridge/source`
factory (implementation `"1"`, configuration `1`) and its `BridgeSource`
(`NativeSource`). It emits the standard graph schema on `out`.

Link it statically through the host SDK, or load the library built with the
`dynamic-plugin` feature (`libdrasi_computation_source_bridge`):

```rust,ignore
NativePlugin::from_definition(drasi_computation_source_bridge::plugin()?)?
    .register_factories(&mut registry)?;
```

The factory declares the `source_admission` Host service. The durable profile
requires its `admission` dependency (a QoS journal configured for this link's
producer admission); the volatile profile forbids it and admits frames into a
bounded queue that `next` drains. The `tls` configuration key carries the
server identity for the `tls` transport.

Pause refuses new data with a retryable `Busy` and returns once no admission is
in flight; receipts and control stay live. Stop cancels the listeners and joins
their workers within the hook deadline (bounded by `timeoutMs`); an unjoined
worker keeps ownership and a retried Stop finishes it. The source owns no state
and refuses erasing disposal. Seal and drain are served on the link's control
lane (`BridgeEndpointControl`).

The [shared bridge contract](../../common/bridge) covers configuration, volatile
versus durable admission, namespace preservation, transport limits, TLS, explicit
cuts and shutdown.
