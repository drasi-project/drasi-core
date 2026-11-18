# Native HTTP sinks: configuration and management

Implementations `drasi.network/http-sink` and `drasi.network/http-delivery`,
version `1`, configuration version `1`. For how they work, see
[DESIGN.md](DESIGN.md).

```sh
cargo build -p drasi-computation-sink-http --features dynamic-plugin
```

All keys use camelCase. Unknown keys are rejected. The published configuration
schema (`GET /api/v1/plugins/computation` in Drasi Server) carries the same
descriptions and defaults. Secret fields accept only host secret references,
never literal values; `configuration()` returns the resolved values because it
is the persistence hook, and `Debug` output redacts them.

## HTTP sink fields

| Field | Type | Default | Range / rule | Update |
|---|---|---|---|---|
| `outputs` | object | none | Optional aliases: alias → `{component, port?}`. Every incoming edge is accepted; an unaliased output is named after its producing component. | Replace |
| `url` | string | none | Absolute `http`/`https` URL, no credentials or fragment. The default destination of every change. With `baseUrl`, it must be on the base origin. | Stop |
| `baseUrl` | string | origin of `url` | Absolute `http`/`https` URL, no credentials, query or fragment. Required when `url` is absent; the default destination is then `{baseUrl}/changes/{alias}`. | Stop |
| `token` | string, **secret** | none | Sent as `Authorization: Bearer <token>`. Cannot be combined with a configured `authorization` header. | Pause |
| `headers` | object | `{}` | Static headers. Names are case-insensitive and normalized to lower case; duplicates and reserved names reject. `content-type` defaults to `application/json`. Values are not templates. | Pause |
| `secretHeaders` | object, **secret** | none | Secret static headers (API keys), merged into `headers` with the same rules. A name may not appear in both. | Pause |
| `outputTemplates` | object | none | See [Templates](#templates). | Pause |
| `adaptive` | object | none | The shared native adaptive object: `minBatchSize` (10), `maxBatchSize` (1000, ≤ 65536), `minWaitMs` (1), `maxWaitMs` (100, ≤ 60000), `windowMs` (5000, ≤ 3600000); unknown keys reject. Identical in the gRPC sink and HTTP source (`drasi_computation_network_common::batching`). Requires `batchEndpoint`. | Pause (bounds); enabling or disabling it also changes `batchEndpoint` |
| `batchEndpoint` | string | none | Path beginning with one `/`, no template. Requires `adaptive`. | Stop |
| `maxBatchBytes` | integer | `16777216` | `1..=67108864`. Host collection bound for one batch of inputs. | Pause |
| `maxRequestBytes` | integer | `33554432` | `1..=33554432`. Largest rendered body; batches split here. | Pause |
| `timeoutMs` | integer | `60000` | ≥ 1. One attempt, from connecting until the response body is read. | Pause |
| `connectTimeoutMs` | integer | `10000` | ≥ 1. TCP and TLS connection deadline. | Stop |
| `poolIdleTimeoutMs` | integer | `90000` | ≥ 1. Idle pooled connections close after this long. | Stop |
| `poolMaxIdlePerHost` | integer | `10` | `0..=1024`. `0` disables connection reuse. | Stop |
| `maxRetries` | integer | `3` | `0..=100`. Additional attempts after the first for a retryable failure. | Pause |
| `retryBudgetMs` | integer | `120000` | `1..=3600000`. No retry starts after this long from the first attempt. | Pause |
| `onPermanentFailure` | string | `"stall"` | What the graph does with an input the destination can never accept: `stall` records a terminal failure on the sink; `skip` logs it, counts it on the sink and continues. Transient failures are always retried in place. See [Delivery failures](#delivery-failures). | Replace |
| `successStatusCodes` | array | `[200, 201, 203, 204]` | Non-empty, unique, each `200..=299`. `202` is not success unless listed. | Pause |
| `unroutedChanges` | string | `"send"` | `send` posts a change whose operation has no template route (in its alias's route or `defaultTemplate`) to the default destination; `ignore` completes it without a request, so a route listing only `added` sends nothing for updates and deletes. | Pause |
| `tlsCaPem` | string | none | PEM CA certificates trusted in addition to the built-in roots. | Stop |
| `tlsClientCertificatePem` | string | none | PEM client certificate chain (mutual TLS). Requires `tlsClientPrivateKeyPem`. | Stop |
| `tlsClientPrivateKeyPem` | string, **secret** | none | PEM private key for the client certificate. | Stop |

Reserved headers, set by the sink on every request and rejected in `headers`,
`secretHeaders` and template headers:

| Header | Value |
|---|---|
| `x-drasi-component` | Producing component ID |
| `x-drasi-port` | Producing output port |
| `x-drasi-delivery-id` | `{component}/{port}/{sequence}.{ordinal}`, or `…/{seq}.{ord}-{seq}.{ord}` for a batch run ([idempotency](DESIGN.md#idempotency-key)) |

## HTTP delivery consumer fields

| Field | Type | Default | Range / rule | Update |
|---|---|---|---|---|
| `url` | string | required | Absolute `http`/`https` endpoint URL, for example `https://receiver.example/drasi/delivery/v1`. | Stop |
| `token` | string, **secret** | none | Sent as `Authorization: Bearer <token>`. | Pause |
| `timeoutMs` | integer | `30000` | `1..=60000`. One delivery attempt including the confirmation. | Pause |
| `connectTimeoutMs` | integer | `10000` | ≥ 1. | Stop |
| `maxMessageBytes` | integer | `16777216` | `1..=16777216`. Largest serialized delivery request. | Pause |
| `tlsCaPem` | string | none | As for the sink. | Stop |
| `tlsClientCertificatePem` | string | none | As for the sink. | Stop |
| `tlsClientPrivateKeyPem` | string, **secret** | none | As for the sink. | Stop |

The consumer has no `outputs`: it forwards the complete input envelope.

## Examples

Secret values are shown as `"<secret reference>"`; use the host's secret
reference syntax. A webhook with a fixed URL and a bearer token:

```json
{
  "outputs": {"comfort": {"component": "building-comfort", "port": "out"}},
  "url": "https://hooks.example/drasi",
  "token": "<secret reference>"
}
```

Per-output REST calls with an API key and a private CA:

```json
{
  "outputs": {"rooms": {"component": "rooms", "port": "out"}},
  "baseUrl": "https://receiver.internal",
  "secretHeaders": {"x-api-key": "<secret reference>"},
  "tlsCaPem": "-----BEGIN CERTIFICATE-----\n...",
  "outputTemplates": {
    "routes": {"rooms": {
      "added":   {"method": "POST",   "url": "/rooms",                 "template": "{{json after}}"},
      "updated": {"method": "PUT",    "url": "/rooms/{{identity.id}}", "template": "{{json after}}"},
      "deleted": {"method": "DELETE", "url": "/rooms/{{identity.id}}"}
    }}
  }
}
```

Batched delivery to a receiver that queues work (202 accepted deliberately):

```json
{
  "baseUrl": "https://ingest.example",
  "batchEndpoint": "/batch",
  "adaptive": {"minBatchSize": 10, "maxBatchSize": 500, "maxWaitMs": 200},
  "successStatusCodes": [200, 202, 204],
  "maxRetries": 10,
  "retryBudgetMs": 600000
}
```

A confirming consumer:

```json
{"url": "https://receiver.internal/drasi/delivery/v1", "token": "<secret reference>"}
```

## Request formats

**Single change** (no template): `POST` to the default destination with the
change notification as JSON:

```json
{"operation":"ADD","output":{"alias":"q","component":"…","port":"out"},"sequence":1,
 "timestamp":"2023-11-14T22:13:20.000000123+00:00","identity":{"namespace":"…","id":"…"},
 "element":{"kind":"node","labels":["Room"]},"after":{"value":42},"effectiveFrom":1700000000000}
```

**Batch:** `POST {baseUrl}{batchEndpoint}` with `{"batch":[item, …]}`, items in
order, all from one output. Each item is the route's rendered body template
(which must be JSON) or the notification.

**Consumer:** `POST {url}` with
`{"version":1,"port":"in","identity":{…},"envelope":{…}}`; the endpoint must
answer `200` with exactly the `identity` object after completing the whole
batch.

## Templates

`outputTemplates` has `routes` (keyed by output alias; a full alias wins over
its last dotted segment) and `defaultTemplate`. Each holds optional `added`,
`updated` (covers `REPLACE` and `PATCH`; `operation` tells them apart) and
`deleted` entries with:

| Key | Meaning |
|---|---|
| `template` | Body. Empty: the notification JSON. In batches, must render JSON. |
| `url` | Path appended to the base URL, or an absolute URL on the base origin. Empty: the default destination. |
| `method` | `GET`, `POST` (default), `PUT`, `PATCH`, `DELETE`. |
| `headers` | Header templates; override configured headers of the same name. Ignored in batches. |

Context keys: `operation`, `output.alias`, `output.component`, `output.port`,
`sequence`, `timestamp` (when present), `identity.namespace`, `identity.id`,
`element.kind`, `element.labels`, and when carried `before`, `after` (a patch's
`after` holds only the properties it sets), `removedProperties`,
`effectiveFrom`.

Templates use the [shared native template engine](../../common/template/README.md), the same in every native sink.
Helpers: `json`, and the Handlebars built-ins `if`, `unless`, `each`, `with`,
`lookup`, `raw`, `log`, `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `and`, `or`,
`not`, `len`. Any other helper, a partial or a decorator rejects at
construction, in any branch. Rendering is strict: reading a field the change
does not carry (for example `after` on a delete) is a render failure, so guard
optional parts with `{{#if after}}…{{/if}}`. Values are not escaped; inside a
JSON body write `{{json x}}` so strings are quoted and escaped.

**Render failures:** a template that fails to render, a rendered URL off the
base origin, an invalid rendered header value, a batch item that is not JSON,
or a body over `maxRequestBytes` is a permanent failure of that change (the
same change fails the same way every time). Nothing falls back to a default
body or URL.

## Delivery failures

The sink follows the graph's [delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures). Each failed change is classified:

| Failure | Class |
|---|---|
| Connection failure, timeout, or a request whose outcome is unknown | transient |
| Status 5xx, 408, 409, 425, 429 | transient; a 429 or 503 `Retry-After` is the minimum delay before the graph retries |
| Any other non-success status (including 202 unless listed in `successStatusCodes`, and 3xx, which is never followed) | permanent |
| Render failure, oversized body or batch item | permanent |

A transient failure is first retried inside the sink (`maxRetries`,
`retryBudgetMs`, with `Retry-After`); if it persists, the input fails
transiently and the graph retries the whole input in place with backoff,
showing the sink as retrying (health `Degraded`, the last error as a
retryable failure) until it is delivered. A permanent failure follows
`onPermanentFailure`: `stall` stops at that change and records a terminal
failure on the sink; `skip` logs the change, still delivers the input's other
changes, and the graph counts the input as skipped. Every repeated request
carries the same `x-drasi-delivery-id`.

The `http-delivery` consumer uses the same classification and supports only
`stall`: its progress belongs to the host `DeliveryRunner`.

## Destination prerequisites

- **Ordinary webhook (sink):** answer a status in `successStatusCodes` only after
  the effect is complete; use 5xx, 408, 409, 425 or 429 for transient failures
  (with `Retry-After` on 429/503 to pace retries) and other 4xx for permanent
  ones. Deduplicate on `x-drasi-delivery-id` if repeats matter.
- **Confirming endpoint (consumer):** derive the batch identity from the
  envelope and your configured consumer scope, run all operations, then answer
  `200` with the identity. The reference implementation is
  `delivery::HttpDeliveryEndpoint` (feature `delivery-receiver`), served at
  `/drasi/delivery/v1`. See
  [NETWORK.md](../../NETWORK.md#opt-in-http-completion-service).

## Updates

Both factories support InPlace (Pause), Restart (Stop) and Recreate (Replace)
updates; the required boundary is the strongest among the changed fields (the
Update column). Neither owns processing state, so every update preserves data.

| Change | Sink | Consumer |
|---|---|---|
| Templates, headers, secret headers, token, timeout, request/message size, retries, retry budget, failure policy, success statuses, adaptive bounds, `maxBatchBytes` | Pause | Pause (token, timeout, size) |
| URL, base URL, batch endpoint, TLS, connect timeout, pool | Stop | Stop (URL, TLS, connect timeout) |
| `outputs`, input bindings | Replace | — |

An invalid proposed configuration (including template routes that name no
incoming output) is rejected during assessment and changes nothing. Apply
refuses a stale plan or the wrong boundary and leaves the object unchanged.

## Lifecycle and removal

- **Start** builds the client; a cancelled Start leaves the component startable.
- **Pause** succeeds without gating anything: the host stops calling `handle`
  and waits for the in-flight call (D-6).
- **Stop Drain** waits for the in-flight delivery, at most about
  `retryBudgetMs + timeoutMs`. **Stop Abort** drops it at once, including any
  backoff; the input is replayed later.
- **Removal with erase** succeeds and is idempotent: the components own no
  external state (the consumer's progress belongs to the host runner).

## Operations

- **Rotating a token or API key:** update `token` or `secretHeaders` (Pause).
- **Rotating TLS material:** update the TLS fields (Stop).
- **Long outages:** under `strict`, one `handle` retries for at most
  `retryBudgetMs` (plus one attempt) and then fails the input, which the host
  retries later. Lower `retryBudgetMs` to make Stop Drain finish sooner.
- **Logs:** target `drasi.network`. Retries and skips are warnings with
  component, output, delivery ID or sequence and the sanitized error (bodies
  and secrets are never logged).
- **Troubleshooting:**

  | Symptom | Cause |
  |---|---|
  | Every delivery fails with status 202 | Add `202` to `successStatusCodes` if queuing is acceptable |
  | Fails with status 301/302/307/308 | Redirects are not followed; configure the final URL |
  | `rendered URL rejected` | A template URL left the base origin |
  | Construction fails naming a header | A reserved header, a duplicate, or `authorization` with `token` |
  | Construction fails naming a helper | Only the listed helpers exist |
  | TLS errors | Missing `tlsCaPem` for a private CA, or a required client certificate |

## Legacy configuration mapping

From `drasi-reaction-http` (breaking; no automatic migration):

| Legacy | Native | Notes |
|---|---|---|
| `baseUrl` (default `http://localhost`) | `baseUrl` or `url` | No default; one is required. |
| `token` (`ConfigValue`) | `token` (secret field) | Use a host secret reference. |
| `timeoutMs` (default 5000) | `timeoutMs` (default 60000) | Per attempt. |
| fixed 3 attempts, 100 ms backoff | `maxRetries` (3), `retryBudgetMs`, jitter, `Retry-After` | Same retryable statuses. |
| any 2xx is success | `successStatusCodes` (default excludes 202) | |
| redirects followed | never followed | |
| `outputTemplates.routes` keyed by query ID | keyed by output alias | Unaliased outputs are named after their component. |
| template context `query_id`, `query_name`, `data`, `metadata` | `output.*`, `sequence`, `identity`, `before`, `after`, … | See [Templates](#templates). |
| render failure falls back to the default body/URL | permanent failure of that change under `onPermanentFailure` | |
| missing field renders empty; `{{x}}` HTML-escaped | missing field is a render failure; no escaping | The shared native template engine. Guard optional images with `{{#if …}}`; use `{{json x}}` inside JSON bodies. |
| static headers in templates rendered | `headers` are static; use template `headers` | |
| `adaptive` (`adaptiveMinBatchSize`, `adaptiveMaxBatchSize`, `adaptiveWindowSize` in 100 ms units, `adaptiveBatchTimeoutMs`), `batchEndpoint` | `adaptive` (`minBatchSize`, `maxBatchSize`, `minWaitMs`, `maxWaitMs`, `windowMs`), `batchEndpoint` | The shared native adaptive object; the old keys are rejected. Host-owned collection; `maxBatchBytes`, `maxRequestBytes`. |
| `priorityQueueCapacity` | removed | Host pipes bound queuing. |
| `recoveryPolicy` (strict / auto-skip-gap), poison drop | `onPermanentFailure` (`stall` / `skip`); outages are retried in place by the graph | `stall` never drops; `skip` drops only permanently rejected changes (4xx other than 408/409/425/429). |
| pool idle 90 s, 10 per host | `poolIdleTimeoutMs`, `poolMaxIdlePerHost` | Same defaults. |
| — | `secretHeaders`, TLS fields, `connectTimeoutMs`, `x-drasi-delivery-id` | New. |
| durable delivery | `drasi.network/http-delivery` | Requires a confirming endpoint. |
