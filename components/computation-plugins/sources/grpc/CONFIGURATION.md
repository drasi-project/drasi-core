# Native gRPC source: configuration and management

Implementation `drasi.network/grpc-source`, version `1`, configuration version
`1`. For how the source works, see [DESIGN.md](DESIGN.md).

```sh
cargo build -p drasi-computation-source-grpc --features dynamic-plugin   # requires protoc
```

## Fields

All keys use camelCase. Unknown keys, including the removed `adaptiveEnabled`
and the HTTP-only `maxBatchEvents`, are rejected.

| Field | Type | Default | Range / rule | Update |
|---|---|---|---|---|
| `stream` | string | required | Nonempty stream ID. It must match the graph's stream binding and, when durable, the QoS channel's stream. | Replace |
| `host` | string | `127.0.0.1` | Literal IP address. A non-loopback address requires `authToken` or `allowUnauthenticated`. | Stop |
| `port` | integer | `50051` | `0..=65535`. `0` picks a free port, which is shown only in the log. | Stop |
| `timeoutMs` | integer | `60000` | ≥ 1. Unary admission deadline. The transport deadline is set slightly above it. | Stop |
| `ingressCapacity` | integer | `1024` | `1..=1048576`. Unary admissions in flight, and the volatile queue bound. | Stop |
| `maxMessageBytes` | integer | `4194304` | `1..=33554432`. Maximum decoded message size. | Stop |
| `maxConnections` | integer | `1024` | `1..=65536`. Open connections; excess connections are closed at once. | Stop |
| `maxConcurrentStreams` | integer | `64` | `1..=65536`. Open streaming RPCs, separate from unary slots. | Stop |
| `streamIdleTimeoutMs` | integer | `300000` | ≥ 1. A stream that sends nothing for this long is closed. | Pause |
| `headerReadTimeoutMs` | integer | `10000` | `1..=600000`. A connection must finish its TLS handshake and send the HTTP/2 connection preface within this deadline, or it is closed. | Stop |
| `authToken` | string, **secret** | none | 16–4096 visible ASCII characters. Required as `authorization: Bearer <token>` metadata on every RPC. | Pause |
| `authTokenSecondary` | string, **secret** | none | A second accepted token, for rotation. Requires `authToken`. | Pause |
| `allowUnauthenticated` | boolean | `false` | Permits a non-loopback listener without a token, for use behind an authenticating proxy. | Pause |
| `rateLimitPerSecond` | integer | none (off) | `1..=1000000` calls and streamed messages per second per client IP. | Pause |
| `rateLimitBurst` | integer | `rateLimitPerSecond` | `1..=1000000`. Requires `rateLimitPerSecond`. | Pause |
| `tlsCertificatePem` | string | none | PEM certificate chain. Requires `tlsPrivateKeyPem`. | Stop |
| `tlsPrivateKeyPem` | string, **secret** | none | PEM private key matching the certificate. | Stop |
| `tlsClientCaPem` | string | none | PEM CA bundle. When set, clients must present a certificate it issued (mutual TLS). | Stop |

Secret fields accept only host secret references. `configuration()` returns the
resolved values because it is the persistence hook; `Debug` output redacts them.

## Examples

```json
{ "stream": "telemetry" }
```

```json
{
  "stream": "telemetry",
  "host": "0.0.0.0",
  "authToken": "<secret reference>",
  "maxConcurrentStreams": 256,
  "streamIdleTimeoutMs": 60000,
  "rateLimitPerSecond": 1000,
  "tlsCertificatePem": "-----BEGIN CERTIFICATE-----\n...",
  "tlsPrivateKeyPem": "<secret reference>",
  "tlsClientCaPem": "-----BEGIN CERTIFICATE-----\n..."
}
```

## Services

**`drasi.v1.SourceService`** (shared `source.proto`):

| RPC | Behaviour |
|---|---|
| `SubmitEvent` | Admits one event. Success means accepted (volatile) or committed (durable). An invalid event returns `success=false`. |
| `StreamEvents` | Admits each streamed event and replies with the cumulative `events_processed`. Invalid input closes the stream after one failed response. |
| `HealthCheck` | `HEALTHY`, or `DEGRADED` while paused, with the admission mode in the message. `UNAVAILABLE` once stopping. |
| `RequestBootstrap` | `UNIMPLEMENTED`. Configure a bootstrap provider on the query. |

**`drasi.admission.v1.AdmissionService`** (durable only; otherwise each call
returns a `NOT_ENABLED` error result):

| RPC | Result |
|---|---|
| `RegisterProducer` | Session `{incarnation, producer, epoch}`. Idempotent while active. The source's own UUID is reserved. |
| `ProducerStatus` | `{session, next_sequence, earliest_receipt}` |
| `RetireProducer` | `retired=true` once obligations finish |
| `AdmissionReceipt` | A receipt, or `not_accepted` for the next unaccepted sequence |
| `Admit` / `StreamEvents` | A receipt or a typed error per event. A streamed failure ends the stream. |

Typed error kinds are `NOT_ENABLED`, `BUSY` (also returned while paused),
`CLOSED`, `CAPACITY_EXHAUSTED`, `SESSION_EXPIRED`, `SEQUENCE`,
`RECEIPT_EXPIRED`, `PAYLOAD_CONFLICT`, `PENDING_OBLIGATIONS`,
`IDENTIFIER_TOO_LONG`, `ACCEPTANCE_UNKNOWN`, `METADATA_UNKNOWN` and `INVALID`.
After an unknown outcome, retry the **same** session, sequence and event.

**gRPC status codes:**

| Status | Meaning |
|---|---|
| `UNAUTHENTICATED` | Missing or invalid token |
| `RESOURCE_EXHAUSTED` | Rate limit exceeded, or stream slots exhausted |
| `UNAVAILABLE` | Paused, stopping, saturated, or durable outcome unknown |
| `DEADLINE_EXCEEDED` | Admission deadline, or stream idle timeout |
| `INVALID_ARGUMENT` | Malformed request |

## Durable admission

Bind the source's `admission` dependency to a QoS channel with producer
admission enabled, on the same stream.

- `SourceService` calls then commit through the source's server-sequenced
  producer (acknowledged after commit, at-least-once). No client changes are
  needed.
- `AdmissionService` gives idempotent client sequencing.
- A momentarily full graph mailbox or journal makes a call wait, up to
  `timeoutMs`, instead of failing; streams are not ended by it. Each event
  commits on its own, and responses always report the committed prefix.
- While paused, the producer metadata RPCs keep answering so that clients can
  resolve uncertain outcomes; only event admission is refused.

## Exposure

A non-loopback `host` requires `authToken`, or `allowUnauthenticated: true`
behind an authenticating proxy. Construction fails otherwise.

## Updates

The source supports InPlace (Pause), Restart (Stop) and Recreate (Replace)
updates, as listed in the Update column of the field table. All updates
preserve data, because the source owns no processing state. A proposed
configuration that fails validation, including the exposure rule, is rejected
during assessment.

## Lifecycle and removal

- **Pause:** new admissions and new streams are refused, and open streams end
  at their next message. Health and producer metadata RPCs keep answering.
- **Stop Drain:** new calls are refused; admissions already in flight finish,
  so a durable commit in progress is answered with success.
- **Stop Abort:** all connections close; in-flight durable admissions answer
  that their outcome is unknown.
- A Stop that misses its deadline reports a retryable error and keeps the
  server until a later Stop completes.
- **Removal with erase** succeeds and is idempotent. Durable journals belong to
  the host's QoS channel.

## Operations

- **Health:** call `SourceService.HealthCheck`, sending the token if one is
  configured.
- **Token rotation:**
  1. Add `authTokenSecondary` (Pause update).
  2. Move clients to the new token.
  3. Promote it to `authToken`.
- **Certificate rotation:** a Stop update.
- **Long-lived producers:** use `StreamEvents`, and raise
  `maxConcurrentStreams` and `streamIdleTimeoutMs` to match the number of
  producers and their quiet periods.
- **Troubleshooting:**

  | Symptom | Cause |
  |---|---|
  | `UNAUTHENTICATED` | Token not sent |
  | `RESOURCE_EXHAUSTED` on a new stream | `maxConcurrentStreams` reached |
  | `DEADLINE_EXCEEDED` on a quiet stream | `streamIdleTimeoutMs` |
  | Connections refused or closed | `maxConnections` reached, or a TLS mismatch |
