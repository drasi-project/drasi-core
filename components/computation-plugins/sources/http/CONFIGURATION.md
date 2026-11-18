# Native HTTP source: configuration and management

Implementation `drasi.network/http-source`, version `1`, configuration version
`1`. For how the source works, see [DESIGN.md](DESIGN.md).

```sh
cargo build -p drasi-computation-source-http --features dynamic-plugin
```

## Fields

All keys use camelCase. Unknown keys are rejected. The published configuration
schema (`GET /api/v1/plugins/computation` in Drasi Server) carries the same
descriptions and defaults.

| Field | Type | Default | Range / rule | Update |
|---|---|---|---|---|
| `stream` | string | required | Nonempty stream ID. It must match the graph's stream binding and, when durable, the QoS channel's stream. | Replace |
| `host` | string | `127.0.0.1` | Literal IP address. A non-loopback address needs authentication (see [Exposure](#exposure)). | Stop |
| `port` | integer | `9000` | `0..=65535`. `0` picks a free port, which is shown only in the log. | Stop |
| `timeoutMs` | integer | `60000` | ≥ 1. Deadline for reading a request body, and separately for admitting it. | Pause |
| `ingressCapacity` | integer | `1024` | `1..=1048576`. Concurrent admitting requests, and the volatile queue bound. | Stop |
| `maxMessageBytes` | integer | `2097152` | `1..=33554432`. Maximum request body. | Pause |
| `maxBatchEvents` | integer | `1024` | `1..=1048576`. Events per batch request, and mappings per webhook route. | Pause |
| `maxConnections` | integer | `1024` | `1..=65536`. Open connections; excess connections are closed at once. | Stop |
| `headerReadTimeoutMs` | integer | `10000` | `1..=600000`. Deadline for a client to send complete headers, and for a TLS handshake. | Stop |
| `authToken` | string, **secret** | none | 16–4096 visible ASCII characters. Required as `Authorization: Bearer <token>` on standard and `/admission/v1` routes. | Pause |
| `authTokenSecondary` | string, **secret** | none | A second accepted token, for rotation. Requires `authToken`. | Pause |
| `allowUnauthenticated` | boolean | `false` | Permits a non-loopback listener without authentication, for use behind an authenticating proxy. | Pause |
| `rateLimitPerSecond` | integer | none (off) | `1..=1000000` requests per second per client IP. | Pause |
| `rateLimitBurst` | integer | `rateLimitPerSecond` | `1..=1000000`. Requires `rateLimitPerSecond`. | Pause |
| `tlsCertificatePem` | string | none | PEM certificate chain. Requires `tlsPrivateKeyPem`. | Stop |
| `tlsPrivateKeyPem` | string, **secret** | none | PEM private key matching the certificate. | Stop |
| `tlsClientCaPem` | string | none | PEM CA bundle. When set, clients must present a certificate it issued (mutual TLS). | Stop |
| `adaptive` | object | absent (disabled) | Volatile mode only. Enables bounded prefetch; see [Adaptive prefetch](#adaptive-prefetch). `maxBatchSize` ≤ `ingressCapacity`. | Stop |
| `webhooks` | object | none | Webhook routes (see [Webhooks](#webhooks)). When present, they replace the standard event routes. | Pause (routes, mappings, auth, error policy); Stop (`cors`, or adding or removing `webhooks`) |

Secret fields accept only host secret references, never literal values.
`configuration()` returns the resolved values because it is the persistence
hook; `Debug` output always redacts them.

## Examples

A loopback listener for local producers:

```json
{ "stream": "orders" }
```

An exposed, authenticated, rate-limited TLS listener (secret values come from
secret references in the graph specification):

```json
{
  "stream": "orders",
  "host": "0.0.0.0",
  "port": 8443,
  "authToken": "<secret reference>",
  "rateLimitPerSecond": 50,
  "rateLimitBurst": 100,
  "tlsCertificatePem": "-----BEGIN CERTIFICATE-----\n...",
  "tlsPrivateKeyPem": "<secret reference>"
}
```

A webhook receiver:

```json
{
  "stream": "rooms",
  "host": "0.0.0.0",
  "webhooks": {
    "errorBehavior": "reject",
    "routes": [{
      "path": "/rooms/:tenant",
      "methods": ["POST"],
      "auth": { "bearer": { "tokenEnv": "ROOM_WEBHOOK_TOKEN" } },
      "mappings": [{
        "operation": "update",
        "elementType": "node",
        "effectiveFrom": { "value": "{{payload.time}}", "format": "unix_millis" },
        "template": {
          "id": "{{payload.id}}",
          "labels": ["Room"],
          "properties": { "value": "{{payload.value}}", "tenant": "{{route.tenant}}" }
        }
      }]
    }]
  }
}
```

## Request formats

**Standard event** (`POST /sources/{componentId}/events`):

```json
{"operation":"insert","timestamp":1700000000000000000,
 "element":{"type":"node","id":"order-1","labels":["Order"],"properties":{"total":12.5}}}
```

- `operation` is `insert`, `update` or `delete`.
- A delete carries `id`, optional `labels` and `timestamp` instead of `element`.
- Relations use `"type":"relation"` with `from` and `to`.
- Timestamps are nanoseconds. If omitted, the receive time is used, except on
  the session protocol, which requires them.

**Batch** (`POST /sources/{componentId}/events/batch`): `{"events":[...]}`.

**Responses**:

- Success: `{"success":true,"message":"...","events_processed":N}`.
- Failure: `success: false`, an `error` message, and the number of events
  accepted before the failure.

| Status | Meaning |
|---|---|
| 200 | All events accepted (volatile) or committed (durable) |
| 400 | Invalid JSON, event, source ID or batch size |
| 401 | Missing or invalid bearer token, or webhook authentication failed |
| 408 | The request body was not received within `timeoutMs` |
| 413 | The body exceeds `maxMessageBytes` |
| 429 | Per-client rate limit exceeded (`Retry-After: 1`) |
| 503 | Paused, stopping, saturated, timed out, or durable outcome unknown (`Retry-After: 1` where retrying is safe) |

`GET /health` returns `{"status":"healthy"|"paused","service":"http-source","mode":"standard"|"webhook","admission":"volatile"|"durable"}`,
or 503 once the source is stopping.

## Webhooks

`webhooks` uses the published webhook contract shared with the legacy HTTP
source. Error-policy values are snake_case.

| Field | Meaning |
|---|---|
| `errorBehavior` | `accept_and_log` (default), `accept_and_skip` or `reject`. Applies to routing, parsing and mapping problems, never to authentication or capacity failures. Durable requests always reject partial mappings. |
| `cors` | `enabled`, `allowOrigins`, `allowMethods`, `allowHeaders`, `exposeHeaders`, `allowCredentials`, `maxAge` |
| `routes[].path` | Path with `:name` parameters. Must not be `/health` or start with `/sources/`. |
| `routes[].methods` | `GET`, `POST`, `PUT`, `PATCH`, `DELETE` |
| `routes[].auth.signature` | `type` (`hmac-sha1`, `hmac-sha256`), `secretEnv`, `header`, optional `prefix`, `encoding` (`hex`, `base64`) |
| `routes[].auth.bearer` | `tokenEnv` |
| `routes[].errorBehavior` | Per-route override |
| `routes[].mappings[]` | `when` condition; `operation` (`insert`, `update`, `delete`), or `operationFrom` plus `operationMap` to read it from the payload; `elementType` (`node`, `relation`); `effectiveFrom` (`value`, `format`: `iso8601`, `unix_seconds`, `unix_millis`, `unix_nanos`); `template` (`id`, `labels`, `properties`, `from`, `to`) |

Templates use Handlebars syntax and can reference `payload`, `route`, `query`,
`headers`, `method`, `path` and `source_id`.

- Payloads may be JSON, XML, YAML or text, selected by `Content-Type`. The
  parser and template engine are shared with the HTTP bootstrap provider
  (`drasi_http_common::content` and `::template`), so the same document
  yields the same template values on both paths; see that provider's README
  for the XML conversion rules.
- When signature and bearer checks are both configured, both must pass.
- Webhook secrets are read from the named environment variables once, when a
  configuration is accepted: construction, and assessment and apply of an
  update. An unset or empty variable refuses the configuration or update.
  Requests never read the environment; a changed variable takes effect with
  the next configuration.

## Durable admission

Bind the source's `admission` dependency to a QoS channel with producer
admission enabled, on the same stream. The source then commits through that
channel and allocates no local queue. Adaptive prefetch is rejected in durable
mode.

- **Standard and webhook routes** are server-sequenced. They acknowledge after
  commit, with at-least-once delivery. No client changes are needed.
- **Session protocol**, for idempotent client-sequenced delivery. Every route is
  `POST` under `/sources/{componentId}/admission/v1`:

| Route | Body | Success |
|---|---|---|
| `/producers/register` | `{"producer":"<uuid>"}` | session `{incarnation, producer, epoch}`. Idempotent while active. The source's own UUID is reserved. |
| `/producers/status` | `{"session":...}` | `{session, next_sequence, earliest_receipt}` |
| `/producers/retire` | `{"session":...}` | `{"retired":true}` once obligations finish |
| `/receipts` | `{"session":...,"sequence":N}` | receipt, or `null` for the next unaccepted sequence |
| `/events` | `{"session":...,"sequence":N,"event":{...}}` | `{"receipts":[...],"error":null}` |
| `/events/batch` | `{"session":...,"first_sequence":N,"events":[...]}` | the same receipt-list shape |

Session-protocol errors carry `{kind, message}`:

| Status | Kinds |
|---|---|
| 429 | `Busy` (returned while paused), `CapacityExhausted` |
| 503 | `Closed`, `AcceptanceUnknown`, `MetadataUnknown` |
| 409 | `NotEnabled`, `Sequence`, `PayloadConflict`, `PendingObligations` |
| 410 | `SessionExpired`, `ReceiptExpired` |
| 400 | `Invalid`, `IdentifierTooLong` |

After an unknown outcome, retry the **same** session, sequence and event.

A durable webhook can use the session protocol by sending the JSON session in
`x-drasi-producer-session` and the first sequence in `x-drasi-first-sequence`.
Every mapping must then set `effectiveFrom`. A volatile source refuses requests
carrying these headers with 409 `NotEnabled`.

A momentarily full graph mailbox or journal makes a durable request wait, up
to `timeoutMs`, rather than fail. Batches are not atomic: each event commits on
its own, and a failed batch reports exactly the committed prefix
(`events_processed` on standard and webhook routes, one receipt per committed
event on session routes). Only the event after that prefix can be uncertain.

While paused, the producer metadata routes (register, status, receipt,
retire) keep answering, so a client can resolve an uncertain outcome; only
event admission is refused.

## Exposure

The source refuses to construct a non-loopback listener in these cases unless
`allowUnauthenticated: true`:

- **standard mode** without `authToken`;
- **webhook mode** if any route has no `auth`;
- **durable webhook mode** without `authToken`, because the admission routes
  then admit data.

Set `allowUnauthenticated` only behind a proxy that authenticates and rate-limits
clients.

## Updates

The source supports managed updates at every boundary: InPlace (Pause),
Restart (Stop) and Recreate (Replace). The required boundary is the strongest
one among the changed fields, as listed in the Update column of the field table.
The source owns no processing state, so every update preserves data.

- **Pause** updates (tokens, rate limits, deadlines, size limits, webhook
  routes and mappings) take effect for the next request, without dropping
  connections.
- **Stop** updates rebind the listener when the source restarts.
- **Replace** (a changed `stream`) constructs a new component.

A proposed configuration that fails validation, including the exposure rule,
is rejected during assessment and changes nothing.

## Lifecycle and removal

- **Pause** refuses new data admissions with 503 and `Retry-After: 1` (429
  `Busy` on session routes) and waits for in-flight admissions to finish.
  Volatile events already accepted stay queued. Health and producer metadata
  routes stay available.
- **Stop Drain** refuses new requests but lets in-flight admissions finish:
  a durable commit in progress completes and its client receives success.
- **Stop Abort** closes all connections immediately; in-flight durable
  admissions answer that their outcome is unknown.
- A Stop that misses its deadline reports a retryable error and keeps the
  listener until a later Stop completes.
- **Removal with erase** succeeds and is idempotent. The source owns no state:
  durable admission journals and producer registrations belong to the host's
  QoS channel and are cleaned up by its owner.

## Operations

- **Health:** `GET /health` (unauthenticated) suits liveness and readiness
  probes. It returns 200 when running or paused, and 503 when stopping.
- **Rotating a token:**
  1. Set `authTokenSecondary` to the new token (Pause update).
  2. Move clients to the new token.
  3. Make it `authToken` and remove the secondary (Pause update).
- **Rotating a certificate:** update `tlsCertificatePem` and
  `tlsPrivateKeyPem`. This is a Stop update and restarts the listener.
- **Capacity:** buffered request memory is at most about `ingressCapacity ×
  maxMessageBytes`. Lower either for small hosts.
- **Logs:** a statically linked host receives the source's logs through its
  logger (target `drasi.network`). A dynamically loaded library writes
  warnings and errors to standard error.
- **Troubleshooting:**

  | Symptom | Cause |
  |---|---|
  | 401 everywhere | `authToken` set but not sent |
  | 503 with `Retry-After` | Paused, or `ingressCapacity` saturated |
  | 413 | Raise `maxMessageBytes` |
  | Connections closed immediately | `maxConnections` reached |
  | Construction fails with an exposure error | Configure authentication for a non-loopback `host` |

## Adaptive prefetch

`adaptive` is the shared native adaptive batching object, identical in the
HTTP source and the HTTP and gRPC sinks
(`drasi_computation_network_common::batching::AdaptiveBatchSettings`). Unknown
keys are rejected; every key has a default and the configuration getter
returns all of them.

| Key | Default | Rule |
|---|---|---|
| `minBatchSize` | `10` | `1..=maxBatchSize`; collection target at idle throughput |
| `maxBatchSize` | `1000` | ≤ `ingressCapacity` (and ≤ 65536); target at burst throughput |
| `minWaitMs` | `1` | `1..=maxWaitMs`; wait at idle throughput |
| `maxWaitMs` | `100` | ≤ 60000; busier tiers wait 1, 10, 25 or 50 ms, clamped to `minWaitMs..=maxWaitMs` |
| `windowMs` | `5000` | `1..=3600000`; throughput observation window |

With the default `ingressCapacity` (1024) `"adaptive": {}` is valid; a smaller
capacity requires an explicit smaller `maxBatchSize`.
