# Native gRPC sink: configuration and management

Implementation `drasi.network/grpc-sink`, version `1`, configuration version
`1`. For how the sink works, see [DESIGN.md](DESIGN.md).

```sh
cargo build -p drasi-computation-sink-grpc --features dynamic-plugin
```

## Fields

All keys use camelCase. Unknown keys, including the removed
`connectionRetryAttempts` and `initialConnectionTimeoutMs`, are rejected.
Update boundaries: **Pause** = applied in place, **Stop** = restart,
**Replace** = the component is recreated.

| Field | Type | Default | Range / rule | Secret | Update |
|---|---|---|---|---|---|
| `outputs` | object | none | Optional aliases for incoming outputs: alias → `{component, port?}`. An unaliased output is named after its producing component. Aliases name outputs in templates. | no | Replace |
| `endpoint` | string | `grpc://localhost:50052` | `grpc://` or `http://` (plaintext), `grpcs://` or `https://` (TLS). Host and optional port only: no path, credentials, query or fragment. | no | Stop |
| `tlsCaPem` | string | none | PEM CA bundle (≤ 256 KiB), trusted in addition to the bundled Mozilla roots. TLS endpoint only. | no | Stop |
| `tlsClientCertificatePem` | string | none | PEM client certificate chain for mutual TLS. Requires `tlsClientPrivateKeyPem`. TLS endpoint only. | no | Stop |
| `tlsClientPrivateKeyPem` | string | none | PEM private key for the client certificate. | **yes** | Stop |
| `tlsDomainName` | string | endpoint host | Server name verified against the receiver certificate (DNS name or IP, ≤ 253 characters). TLS endpoint only. | no | Stop |
| `metadata` | object | `{}` | Name → value, ASCII gRPC metadata. Names are case-insensitive (sent lowercase) and unique ignoring case; `-bin` names and reserved names are rejected. Sent as headers and in the request `metadata` map. | no | Pause |
| `secretMetadata` | object | none | Name → value, as `metadata`, for credentials. Sent only as headers. Names may not repeat a `metadata` name, a template metadata name, or a reserved name. | **yes** | Pause |
| `timeoutMs` | integer | `60000` | ≥ 1. Deadline of each `ProcessChanges` call, sent as `grpc-timeout`. | no | Pause |
| `connectTimeoutMs` | integer | `10000` | ≥ 1. Bound on each connection attempt (TCP, TLS, HTTP/2). | no | Stop |
| `requireConnectedAtStart` | boolean | `false` | When `true`, Start connects within the retry policy, its deadline and its cancellation. When `false`, the first delivery connects. | no | Pause (read at the next Start) |
| `maxRetries` | integer | `5` | `0..=100`. Retries of one request after a retryable failure; also connection retries at Start. | no | Pause |
| `retryBudgetMs` | integer | `120000` | `1..=3600000`. No retry starts later than this after a request's first attempt (or Start's first connection attempt). | no | Pause |
| `failurePolicy` | string | `strict` | `strict` fails the input so the host retries it. `skip` logs a sanitized warning and completes it. | no | Pause |
| `keepaliveIntervalMs` | integer | `30000` | `0..=3600000`. HTTP/2 PING interval while a call is open; `0` disables keepalive. | no | Stop |
| `keepaliveTimeoutMs` | integer | `20000` | `1..=3600000`. PING acknowledgement timeout before the connection is closed. | no | Stop |
| `compression` | string | `none` | `none` or `gzip`. With `gzip`, the receiver must accept gzip requests. | no | Stop |
| `batchSize` | integer | `1` | `1..=65536`. Fixed batching target in changes; `1` disables host batching. Not with `batching`. | no | Pause |
| `batchFlushTimeoutMs` | integer | `100` | `1..=60000`. Host collection deadline in fixed mode. Not with `batching`. | no | Pause |
| `batching` | object | none | `{"mode":"fixed","batchSize":100,"batchFlushTimeoutMs":1000}` or `{"mode":"adaptive","adaptiveMinBatchSize":1,"adaptiveMaxBatchSize":100,"adaptiveWindowSize":10,"adaptiveBatchTimeoutMs":1000}` (nested defaults shown; sizes ≤ 65536, window ≤ 255 × 100 ms, timeout ≤ 60000). Mutually exclusive with the flat fields. | no | Pause |
| `outputTemplates` | object | none | See [Templates](#templates). | no | Pause |
| `maxBatchBytes` | integer | `16777216` | `1..=67108864`. Host collection byte bound. | no | Pause |
| `maxRequestBytes` | integer | `4194304` | `1..=33554432`. Maximum encoded request (and decoded response). Requests split before it; a larger single change fails under `failurePolicy`. | no | Stop |
| `outputFormat` | string | `canonicalJson` | `canonicalJson` puts the canonical change JSON in `payload`; `proto` omits it unless a body template renders one. | no | Pause |

Reserved metadata names, set by the sink on every request:
`x-drasi-component`, `x-drasi-port` (the real producing output) and
`x-drasi-delivery-id` (see [DESIGN.md](DESIGN.md#idempotency-key)).

Secret fields accept only host secret references; a whole `secretMetadata`
object is one reference that resolves to an object of strings.
`configuration()` returns resolved values because it is the persistence hook;
`Debug` output redacts them. With a `batching` object, `configuration()` omits
the flat batch fields and fills the object's nested defaults.

The host batching contract allows at most 1024 inputs per batch, so a fixed
`batchSize` above 1024 publishes `maxInputs` 1024 while still allowing up to
`batchSize` changes per request.

## Examples

Plaintext, defaults:

```json
{ "endpoint": "grpc://receiver:50052" }
```

TLS with a private CA, a bearer token and batching:

```json
{
  "outputs": { "rooms": { "component": "<query uuid>", "port": "out" } },
  "endpoint": "grpcs://receiver.internal:443",
  "tlsCaPem": "-----BEGIN CERTIFICATE-----\n...",
  "secretMetadata": "<secret reference resolving to {\"authorization\": \"Bearer ...\"}>",
  "metadata": { "x-tenant": "blue" },
  "batchSize": 100,
  "batchFlushTimeoutMs": 200
}
```

Mutual TLS by IP address, connection required at Start:

```json
{
  "endpoint": "https://10.0.0.12:50052",
  "tlsCaPem": "-----BEGIN CERTIFICATE-----\n...",
  "tlsDomainName": "receiver.internal",
  "tlsClientCertificatePem": "-----BEGIN CERTIFICATE-----\n...",
  "tlsClientPrivateKeyPem": "<secret reference>",
  "requireConnectedAtStart": true
}
```

Adaptive batching with gzip:

```json
{
  "endpoint": "grpc://receiver:50052",
  "compression": "gzip",
  "batching": { "mode": "adaptive", "adaptiveMinBatchSize": 1, "adaptiveMaxBatchSize": 500,
                "adaptiveWindowSize": 10, "adaptiveBatchTimeoutMs": 100 }
}
```

## TLS

| Endpoint | Behaviour |
|---|---|
| `grpc://`, `http://` | Plaintext HTTP/2. Any TLS field is rejected. |
| `grpcs://`, `https://` | TLS. The receiver certificate must chain to a bundled Mozilla root or to `tlsCaPem`, and match `tlsDomainName` (or the endpoint host). |
| + `tlsClientCertificatePem` and `tlsClientPrivateKeyPem` | Mutual TLS: the client certificate is presented. Both or neither. |

PEM material is parsed at construction, so a malformed certificate or key
fails creation. The operating system trust store is not consulted.

## Receiver protocol

Implement `drasi.v1.ChangeService/ProcessChanges` from `drasi-grpc-common`'s
reaction protocol and:

1. Process the request's changes in order.
2. Return `success=true` and `items_processed` equal to the number of changes
   only after the effect is complete (durable, if you rely on that).
3. Return `success=false` with `error` for a request you will never accept;
   the sink does not retry it.
4. Use status codes for transient conditions: `UNAVAILABLE` or
   `RESOURCE_EXHAUSTED`, optionally with `grpc-retry-pushback-ms` or
   `retry-after` metadata. Codes such as `INVALID_ARGUMENT` or
   `UNAUTHENTICATED` are not retried.
5. Deduplicate by `x-drasi-delivery-id` (also present in the request
   `metadata` map): retries and replays carry the same value.

Request metadata contains `x-drasi-component`, `x-drasi-port`,
`x-drasi-delivery-id`, configured `metadata` and rendered template metadata;
headers additionally carry `secretMetadata`.

## Templates

`outputTemplates` has `defaultTemplate` and `routes` (alias → template set).
Each template set has optional `added` (`ADD`), `updated` (`REPLACE` and
`PATCH`) and `deleted` (`DELETE`) entries:

```json
{
  "outputTemplates": {
    "defaultTemplate": {
      "added": { "template": "{\"id\": {{json identity.id}}, \"after\": {{json after}}}",
                 "metadata": { "x-operation": "{{operation}}" } }
    },
    "routes": {
      "rooms": { "deleted": { "template": "{\"gone\": {{json identity.id}}}" } }
    }
  }
}
```

- **Routing:** the output alias, then its last dotted segment, then
  `defaultTemplate`. A route without an entry for the operation falls through.
  Every route key must name a bound alias or its last dotted segment (checked
  at creation and at assessment).
- **Context:** the canonical change object: `operation`, `output`
  (`{alias, component, port}`), `sequence`, `timestamp` (when known),
  `identity`, `element`, `before`, `after`, `removedProperties` (patches),
  `effectiveFrom`. Absent fields are absent, not null.
- **Helpers:** `json` (renders its argument as JSON) and the Handlebars
  built-ins. Strict mode: reading a missing field is an error.
- **Body:** must render valid JSON; it replaces `payload` only. Typed images,
  identity, operation and sequence are never rewritten. An empty template uses
  the `outputFormat` default.
- **Metadata:** each entry renders one request metadata value; rendered names
  override `metadata`, must be valid ASCII gRPC metadata and cannot be
  reserved names or `secretMetadata` names. Requests split when rendered
  metadata differs.
- **Render failure** (missing field, invalid JSON, invalid metadata value) is a
  per-change failure: under `strict` the input fails before anything is sent;
  under `skip` that change is logged and skipped (D-5).

## Update matrix

| Change | Boundary |
|---|---|
| `metadata`, `secretMetadata`, `timeoutMs`, `maxRetries`, `retryBudgetMs`, `failurePolicy`, `requireConnectedAtStart`, `batchSize`, `batchFlushTimeoutMs`, `batching`, `maxBatchBytes`, `outputTemplates`, `outputFormat` | Pause (in place) |
| `endpoint`, `tlsCaPem`, `tlsClientCertificatePem`, `tlsClientPrivateKeyPem`, `tlsDomainName`, `connectTimeoutMs`, `keepaliveIntervalMs`, `keepaliveTimeoutMs`, `compression`, `maxRequestBytes` | Stop (restart) |
| `outputs`, or the bound inputs | Replace |
| No change | Pause (nothing to do) |

The strongest boundary of the changed fields applies. An invalid proposed
configuration (including a template route that names no bound alias) is
rejected at assessment.

## Removal and Erase

The sink owns no destination-side state. Removal with `StateIntent::Erase` is
an idempotent no-op and the factory declares `erases_owned_state`. Receiver
data is never touched.

## Operations

- **Receiver down:** Start succeeds (unless `requireConnectedAtStart`); each
  delivery retries within `retryBudgetMs`, then fails the input (`strict`),
  which the host retries. Warnings name the attempt and status.
- **Bounding a stuck delivery:** `timeoutMs` bounds one call,
  `connectTimeoutMs` one connection attempt, and `maxRetries`/`retryBudgetMs`
  the retries of one request. Pause waits for the in-flight delivery; Stop with
  `Abort` cancels it at once.
- **Duplicates:** expected after retries, restarts and host replay; deduplicate
  by `x-drasi-delivery-id`.
- **Credential rotation:** update `secretMetadata` in place; the TLS client
  key and certificate need a restart.

## Migration from the legacy gRPC reaction

This is a breaking change (D-9); there is no compatibility mode.

| Legacy (`reactions/grpc`) | Native | Note |
|---|---|---|
| `drasi.v1.ReactionService/ProcessResults` with query-result items | `drasi.v1.ChangeService/ProcessChanges` with typed changes | Receivers must implement `ChangeService`. |
| `endpoint` | `endpoint` | `grpcs://`/`https://` now select TLS. |
| `metadata` (plain or secret `ConfigValue`) | `metadata`, or `secretMetadata` for credentials | Reserved `x-drasi-*` names are rejected. |
| `timeoutMs` default 5000 | `timeoutMs` default **60000** | One per-call deadline. |
| `maxRetries` default 3 | `maxRetries` default **5**, plus `retryBudgetMs` 120000 | Permanent codes are no longer retried. |
| `connectionRetryAttempts` (5), `initialConnectionTimeoutMs` (10000) | `connectTimeoutMs` (10000) and the retry policy; `requireConnectedAtStart` | Removed fields are rejected. |
| fixed batching default `batchSize` 100, `batchFlushTimeoutMs` 1000 | default `batchSize` **1**, `batchFlushTimeoutMs` **100** | Set `batchSize: 100, batchFlushTimeoutMs: 1000` for the legacy behaviour. |
| adaptive `batching` object | same object | Same nested defaults. |
| `outputFormat` | `outputFormat` | Unchanged values. |
| `outputTemplates` keyed by query id; context `query_id`, `query_name`, `data`, `row_signature`, `before`/`after` | keyed by output alias; truthful change context (`operation`, `output`, `identity`, `before`, `after`, …) | `query_id` → `output.component`, `query_name` → `output.alias`, `data`/`row` → `after` (or `before` for deletes); a `PATCH`'s `after` holds only the changed properties. |
| render failure falls back to the canonical payload | render failure follows `failurePolicy` | |
| `GOAWAY`/connection errors matched by text; 5 s `RESOURCE_EXHAUSTED` sleep | classification by status code; channel rebuilt; jittered backoff with receiver hints | |
| no TLS | TLS and mutual TLS | |
| no idempotency key | `x-drasi-delivery-id` | |
