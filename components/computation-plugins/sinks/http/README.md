# Native HTTP sink

`HttpSinkConfig` and its defaults, URL/header checks and parsing live in this
crate's `config` module. Shared network code supplies only common validation
and delivery-policy helpers.

The normal sink and `delivery::HttpDeliveryHandler` do not depend on the graph
runtime. Applications that host `delivery::HttpDeliveryEndpoint` explicitly
enable `delivery-receiver`: that endpoint actually runs a host-owned delivery
runner and therefore needs `drasi-lib`. This feature is not needed to build or
load the HTTP sink plugin.

`drasi-computation-sink-http` provides only `drasi.network/http-sink`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-sink-http --features dynamic-plugin
```

Its `in` port consumes standard graph-change envelopes from any producer: a
source, a transformer or a query ([sink input contract](../../SINKS.md)). Supply
a complete `url` or `baseUrl`; the graph supplies each incoming output and optional
`outputs` (alias → `{component, port?}`) names them. With only `baseUrl`, the default destination is
`/changes/{alias}`. Defaults are
`timeoutMs=60000`, `maxRetries=3`, `failurePolicy="strict"`. The default header is
`content-type: application/json`; every request also carries `x-drasi-component`
and `x-drasi-port` naming the real producing output, and configuring either
rejects. Header names are normalized; duplicate names reject.
Explicit content types are allowed for custom bodies. Redirects are disabled.

`Handled` means the notification attempts completed successfully, or an explicit
`"skip"` policy logged and skipped exhausted failures. An ordinary HTTP success,
including 202, does not establish remote business completion.
See [configuration and delivery](../../NETWORK.md#http-sink-configuration).

The opt-in Rust `delivery` module is also packaged here; it supplies
`HttpDeliveryHandler` and `HttpDeliveryEndpoint` for the existing `DeliveryRunner`.
It is never enabled implicitly by the ordinary sink factory. See its
[completion contract](../../NETWORK.md#opt-in-http-completion-service).
No HTTP source or gRPC/SSE sink component is loaded.

## Templates and authentication

`token`, `outputTemplates.defaultTemplate` and `outputTemplates.routes` use the
same HTTP configuration and rendering helpers as `drasi-reaction-http`.
Full output aliases take precedence over their last dotted segment and then the
default operation template. Each `added`/`updated`/`deleted` entry supports a
body `template`, `url`, `method` and `headers`. URL and header values can use
Handlebars; absolute URLs must stay on the configured base origin.

`updated` covers both `REPLACE` and `PATCH`; the context's `operation` tells them
apart. The shared context is the default body: `operation`, `output` (`alias`,
`component`, `port`), `sequence`, `timestamp` (only when the envelope has one),
`identity` and only the `element`, `before`, `after` (a patch's `after` holds just
the properties it sets), `removedProperties` and `effectiveFrom` that the record
carries. A template that reads an absent field fails strictly. The `json` helper
and logged render-error fallbacks are unchanged. Invalid rendered destinations cannot become a successful strict
delivery. Bearer tokens and configured headers remain in privileged
configuration snapshots, as required for reconstruction.

```json
{
  "outputs": {"rooms": {"component": "rooms", "port": "out"}},
  "baseUrl": "https://receiver.example",
  "outputTemplates": {
    "routes": {"rooms": {
      "updated": {
        "method": "PUT",
        "url": "/rooms/{{after.id}}",
        "template": "{{json after}}",
        "headers": {"content-type": "application/json"}
      }
    }}
  }
}
```

## Batching and failure boundaries

Set `adaptive` and `batchEndpoint` to send ordered `{"batch":[...]}` requests.
The `adaptiveMinBatchSize`, `adaptiveMaxBatchSize`, `adaptiveWindowSize` and
`adaptiveBatchTimeoutMs` settings retain the legacy throughput policy. Only body
templates apply to individual batch items; the outer call is one POST to the
batch endpoint. Changes of another output split batches so each request's identity headers
remain correct.

The host owns collection and timers through the optional native batching
extension. It retains every input and receipt through delivery, cancellation and
acknowledgement. Snapshot/progress, input-port and byte boundaries do not get
combined. There is no private reaction processing queue. `maxBatchBytes`
(16 MiB by default, at most 64 MiB) bounds host collection; `maxRequestBytes`
(32 MiB by default) bounds the rendered body and splits coalesced requests.

Retries reuse the exact rendered request. The shared HTTP status classification
retries transport failures, 5xx, 408, 409, 425 and 429. Authentication and permanent
rejections fail immediately. Unlike the legacy poison-event checkpoint path,
native `strict` propagates every undelivered operation; only an explicit `skip`
policy permits logged loss. No input completes before all its request chunks
finish. Interrupted or lost replies can repeat effects on replay; neither
batching nor `Handled` makes a remote effect transactional.
