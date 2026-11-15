# Native HTTP sinks

`drasi-computation-sink-http` provides two native ComputationGraph factories
(implementation `1`, configuration version `1`, native ABI 4.0). Both consume
standard graph-change envelopes on their `in` port from any producer: a source,
a transformer or a query ([sink input contract](../../SINKS.md)).

- **`drasi.network/http-sink`** sends each change, or each batch of changes
  with adaptive batching, as an HTTP request shaped by optional Handlebars
  templates. Completion is `Handled`: the destination answered a configured
  success status (`200`, `201`, `203`, `204` by default; `202` only when
  configured). Retries are jittered, budgeted and honour `Retry-After`; every
  request carries a stable `x-drasi-delivery-id`.
- **`drasi.network/http-delivery`** is an external consumer for the host
  `DeliveryRunner`. It sends each input batch once to a confirming endpoint and
  completes only on an exact `200` echoing the batch identity.

```sh
cargo build -p drasi-computation-sink-http --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: architecture, delivery profiles with exact
  duplicate and loss windows, idempotency key, retry and classification,
  templates, lifecycle, management, security model and differences from the
  legacy reaction.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges,
  secrecy and update boundaries, examples, request formats, template
  reference, destination prerequisites, operations and the legacy mapping.

Minimal configuration: `{"url": "https://hooks.example/drasi"}`.

The `delivery` module also provides `HttpDeliveryEndpoint`, a reference
receiver for the confirmation protocol. It owns a host `DeliveryRunner` and is
therefore behind the `delivery-receiver` feature, which depends on `drasi-lib`;
the plugin itself never needs that feature.
