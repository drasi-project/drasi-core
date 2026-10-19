# Native HTTP source

`drasi-computation-source-http` provides only `drasi.network/http-source`
(implementation `"1"`, configuration `1`).

`HttpSourceConfig` and its validation live in this crate's `config` module.
Use `HttpSourceConfig::from_value` for JSON settings; `SourceConfig` remains an
import-compatible name.

```sh
cargo build -p drasi-computation-source-http --features dynamic-plugin
```

Configuration requires `stream`. Defaults are `sourceId="facilities-db"`,
`host="0.0.0.0"`, `port=9000`, `timeoutMs=60000`, `ingressCapacity=1024`,
`maxMessageBytes=2097152`, `maxBatchEvents=1024`, `adaptiveEnabled=false`.
Its `out` port emits `GraphChangeCodec` envelopes; construction binds no socket.

The crate includes ordinary and durable HTTP routes. Ordinary event acceptance
is volatile; durable acceptance requires an actual host admission channel and
the separate session/sequence protocol. See
[source configuration](../../NETWORK.md#source-configuration) and
[durable admission](../../NETWORK.md#optional-durable-admission) for wire shapes,
limits and acknowledgement semantics. No gRPC component is loaded.

## Webhooks

`webhooks` selects the published HTTP webhook contract instead of the standard
volatile event routes. Authentication, routing, parsing and mapping reuse
`drasi-http-common` and `drasi-source-mapping`, including the legacy regression
tests. Configuration uses the published camelCase field names; timestamp format
values remain `iso8601`, `unix_seconds`, `unix_millis` and `unix_nanos`.

```json
{
  "stream": "webhooks/out",
  "sourceId": "webhooks",
  "host": "127.0.0.1",
  "port": 9000,
  "webhooks": {
    "errorBehavior": "reject",
    "routes": [{
      "path": "/rooms/:tenant",
      "methods": ["POST"],
      "auth": {"bearer": {"tokenEnv": "ROOM_WEBHOOK_TOKEN"}},
      "mappings": [{
        "operation": "update",
        "elementType": "node",
        "effectiveFrom": {"value": "{{payload.time}}", "format": "unix_millis"},
        "template": {
          "id": "{{payload.id}}",
          "labels": ["Room"],
          "properties": {"value": "{{payload.value}}", "tenant": "{{route.tenant}}"}
        }
      }]
    }]
  }
}
```

Routes support path parameters, HTTP methods, header/payload conditions,
operation mappings, node/relation mappings and typed properties. The payload
parser supports JSON, XML, YAML and text. HMAC-SHA1/HMAC-SHA256 signatures support
hex/base64 and an optional prefix; when signature and bearer checks are both
configured, both must pass. Optional `cors` retains the published configuration,
with invalid header/origin settings rejected during construction.

`errorBehavior` retains explicit reject/accept-and-log/accept-and-skip handling
for webhook mapping errors. Capacity failures, interrupted admission and durable
mapping failures are never turned into success. All selected mappings are
converted before admission, and a failed admission reports its accepted prefix.
Retried volatile requests can repeat that prefix.

Durable webhooks use the same host admission service as the standard durable
routes. Register a producer through `/sources/{sourceId}/admission/v1`, then send
its JSON session in `x-drasi-producer-session` and the first sequence in
`x-drasi-first-sequence`. Every mapping must specify `effectiveFrom`. Successful
admission returns the usual receipt list, with one consecutive producer sequence
per mapped event. A successful HTTP status alone is not a durable receipt.

## Adaptive ingress

`adaptiveEnabled: true` enables bounded prefetch using the legacy throughput
tiers. Optional settings are `adaptiveMaxBatchSize` (default the smaller of
1000 and `ingressCapacity`), `adaptiveMinBatchSize` (the smaller of 10 and that
maximum), `adaptiveMaxWaitMs` (100), `adaptiveMinWaitMs` (1), and
`adaptiveWindowSecs` (5). The prefetch maximum cannot exceed `ingressCapacity`;
waits are positive and ordered, with a 60-second maximum.

The volatile ingress queue and the explicitly bounded prefetch buffer are
separate bounds. Cancellation retains prefetched events on the component;
each still leaves `next` as a separate envelope and query evaluation. There is
no new processor task. Durable admission is already scheduled by the hosting
graph and rejects this volatile-prefetch option rather than creating another
queue or delaying producer receipts.
