# Native gRPC sink

`drasi-computation-sink-grpc` provides `drasi.network/grpc-sink`
(implementation `1`, configuration version `1`, native ABI 4.0). Its `in` port
consumes standard graph-change envelopes from any producer (a source, a
transformer or a query) and calls the receiver's
`drasi.v1.ChangeService/ProcessChanges` with ordered, typed changes. The
protocol is defined in `drasi-grpc-common`.

```sh
cargo build -p drasi-computation-sink-grpc --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: service contract, acknowledgement rule,
  batching, delivery profile with its duplicate and loss windows, retry
  classification, idempotency key, lifecycle, management, security and known
  limitations.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with its default,
  range, secrecy and update boundary; TLS; templates; examples; the update
  matrix; and the migration from the legacy gRPC reaction.

Minimal configuration: `{"endpoint": "grpc://receiver:50052"}`. Use
`grpcs://` or `https://` for TLS. Start does not contact the receiver unless
`requireConnectedAtStart` is `true`. A request completes only when the
receiver returns `success=true` with an exact `items_processed` count. Every
request carries an `x-drasi-delivery-id` for deduplication.

This is a breaking replacement for the legacy gRPC reaction: it uses
`ChangeService`, not `ReactionService`, and has different defaults. See the
[migration section](CONFIGURATION.md#migration-from-the-legacy-grpc-reaction).
