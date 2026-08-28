# WebSocket source

The WebSocket source connects to one remote endpoint and turns JSON text
messages into Drasi graph changes. It handles upgrade headers, fixed messages
after each handshake, mapping, bounded subscriber backpressure, and reconnects.

## Configuration

```yaml
url: "wss://feed.example.com/events"
headers:
  - name: Authorization
    value:
      kind: Secret
      name: feed-token

initialMessages:
  - '{"type":"subscribe","stream":"sensors"}'

itemsPath: events
mappings:
  - operationFrom: payload.op
    operationMap:
      insert: insert
      update: update
      delete: delete
    elementType: node
    effectiveFrom:
      value: "{{payload.ts}}"
      format: unix_millis
    template:
      id: "{{payload.id}}"
      labels: ["Sensor"]
      properties:
        value: "{{payload.value}}"

reconnect:
  enabled: true
  delayMs: 1000
  maxDelayMs: 30000

maxMessageSizeBytes: 1048576
bufferCapacity: 64
```

| Setting | Default | Valid values |
| --- | --- | --- |
| `url` | Required | Absolute `wss://` URL, or `ws://` when `allowInsecure` is `true`; 1–8,192 bytes, with a host and no user information or fragment |
| `allowInsecure` | `false` | Boolean |
| `headers` | `[]` | At most 64 entries with unique, valid HTTP names and valid values; WebSocket-managed names are prohibited |
| `connectTimeoutMs` | `10000` | 100–300,000 |
| `initialMessages` | `[]` | At most 32 valid JSON messages, each no larger than `maxMessageSizeBytes` |
| `reconnect.enabled` | `true` | Boolean |
| `reconnect.delayMs` | `1000` | 100–300,000 |
| `reconnect.maxDelayMs` | See below | 100–300,000 and not less than `delayMs` |
| `itemsPath` | `$` | `$` or one top-level field name |
| `mappings` | Required | 1–64 mappings |
| `maxMessageSizeBytes` | `1048576` | 1,024–16,777,216 |
| `bufferCapacity` | `64` | 1–1,024 |

Cleartext `ws://` endpoints are rejected unless `allowInsecure: true` is set.
`wss://` uses a source-owned, Ring-backed Rustls connector and the platform's
native trust roots. This version has no custom-CA, client-certificate, or mTLS
configuration. WebSocket-managed headers such as `Host`, `Upgrade`, and
`Sec-WebSocket-Protocol` cannot be overridden. `connectTimeoutMs` covers the
handshake and sending and flushing every `initialMessages` entry.

For dynamic-plugin configuration, these fields accept `ConfigValue`: `url`,
`allowInsecure`, every header value, `connectTimeoutMs`, every initial message,
all reconnect fields, `itemsPath`, `maxMessageSizeBytes`, and `bufferCapacity`.
Header names and mapping configuration are always plain values; they cannot be
expressed as `ConfigValue` (for example, secret or environment-variable
references). `properties()` intentionally preserves the complete configuration:
descriptor-created sources retain unresolved `ConfigValue` envelopes, while
embedded sources serialize the literal runtime configuration. This is
persistence behavior, not redaction.

Source-owned diagnostics do not include resolved URLs, header values, initial
messages, or raw malformed input. The shared `SourceBase` may log mapped graph
changes at `DEBUG`, so mapped property values must still be treated as log data.
Do not enable `TRACE` for the upstream `tungstenite` crate when wire data may be
sensitive: its trace records include handshake requests and WebSocket message
and frame data, and this source cannot redact records emitted inside that
dependency.

## Mapping

Each WebSocket text message must contain one JSON value. `itemsPath: $` selects
that complete value; an array is processed item by item in order, while any
other value is processed once. Any other `itemsPath` names one top-level array
field. A missing field selects no items, which lets acknowledgement and
heartbeat messages pass without producing changes. A selected field that exists
but is not an array is fatal. Nested `itemsPath` expressions are not supported.

For example, the configuration above accepts this message:

```json
{
  "events": [
    { "op": "insert", "id": "sensor-1", "value": 41, "ts": 1770000000000 },
    { "op": "update", "id": "sensor-1", "value": 42, "ts": 1770000001000 },
    { "op": "delete", "id": "sensor-2", "ts": 1770000002000 }
  ]
}
```

The first two items emit `Sensor` node insert/update changes with stable element
IDs and a `value` property. The last emits delete metadata for `sensor-2` with
the same source ID and label. A mapping can set a static `operation`, or use
`operationFrom` with `operationMap` to derive `insert`, `update`, or `delete`
from the selected item. Relation mappings use `elementType: relation` and must
provide `template.from` and `template.to`, for example:

```yaml
elementType: relation
template:
  id: "{{payload.id}}"
  labels: ["READS"]
  from: "{{payload.readerId}}"
  to: "{{payload.sensorId}}"
```

Templates receive the selected value as `payload`, the complete message value
as `envelope`, and the source ID as `source_id`. Mapping paths and templates may
address nested values. Mappings are checked in declaration order, and only the
first match is applied. A valid message with no matching mapping produces no
graph change. If the selected mapping cannot be applied, including a missing or
unmapped dynamic `operationFrom`, the source logs a fixed summary without input
values and skips the item.

When `effectiveFrom` is omitted or its rendered value is empty, the mapping
engine uses the current Unix time in milliseconds. A simple template string is
auto-detected; an explicit object supports `iso8601`, `unix_seconds`,
`unix_millis`, and `unix_nanos`.

Dynamic-plugin configuration rejects unknown fields in mapping, condition,
explicit `effectiveFrom`, and template objects. A `when` condition must specify
`field` and exactly one of `equals`, `contains`, or `regex`; header conditions
are not supported. Regular expressions are limited to 4,096 bytes and compiled
with explicit program-size limits; invalid or oversized patterns fail source
construction. Mapping configuration is trusted operator input and must not be
derived from upstream message data.

## Runtime behavior

For `wss://`, `start()` synchronously loads and validates the platform trust
roots before it creates a worker. It returns an error if no usable roots are
available. A successful start creates a cancellable worker and returns before
making a network connection. The worker waits for at least one query
subscription before its first connection attempt. The loaded roots are reused
for every reconnect; trust-store changes require stopping and starting the
source. The subscriber gate runs once per explicit start: losing every
subscriber after connection does not pause socket consumption, and reconnects
do not wait for a new subscriber. Messages consumed without a subscriber are
lost because this is a volatile source.

The lifecycle states are:

| State | Meaning |
| --- | --- |
| `Starting` | Waiting for the first subscriber, connecting, or retrying |
| `Running` | The handshake completed and all initial messages were sent and flushed locally |
| `Error` | A fatal failure occurred, or an unclean established connection ended while reconnect was disabled |
| `Stopping` / `Stopped` | Explicit shutdown is in progress/complete; a clean established close also becomes `Stopped` when reconnect is disabled |

`Running` does not confirm that the remote application accepted an initial
message. Each reconnect resends all initial messages.

Text frames enter a bounded 16-frame internal queue before mapping and dispatch.
This lets the socket continue handling Ping, Pong, and Close frames during
temporary subscriber backpressure. If downstream dispatch remains blocked long
enough to fill that queue, socket reads stop and TCP backpressure applies. An
inbound Ping queues and flushes the automatic Pong response; Pong messages are
ignored.

The source-owned malformed-JSON warning omits message contents. Malformed JSON
and unsupported binary messages are skipped. Oversized messages or frames,
invalid UTF-8, attack detection, a non-array selected field, and selection of
more than 1,000 items are fatal. Other local WebSocket parser/protocol and TLS
errors are also fatal.

When reconnect is enabled, retryable setup failures and established-connection
disconnects, including abrupt EOF without a Close handshake, use bounded
exponential backoff. `delayMs` is the first delay. Optional `maxDelayMs` caps
growth; when omitted, the effective cap is the greater of `delayMs` and 30
seconds. Backoff resets only after the handshake and all initial messages have
been sent and flushed. Setup timeouts, I/O failures, and handshake HTTP 408,
425, 429, 500, 502, 503, and 504 are retryable. Every other handshake HTTP
response, TLS failure, invalid URL, and invalid local request configuration is
fatal. Retry sleeps are shutdown-aware. With reconnect disabled, setup failures
are not retried.

`maxMessageSizeBytes` bounds both individual WebSocket frames and reassembled
messages before mapping. One message may select at most 1,000 items.

The source is volatile: `supports_replay()` returns `false`, events have no
`source_position`, and messages sent while the client is disconnected may be
lost. Persistent query recovery therefore requires re-bootstrap or reset; it
cannot resume from a WebSocket source position. There is no deduplication or
exactly-once guarantee: a server may resend events after reconnect, so use
stable mapped IDs and idempotent upstream or query semantics where duplicates
matter.

There is no built-in bootstrap provider, though an external provider can be
attached through the standard `SourceBase` path. Such a provider must not
return a source position, and there is no atomic boundary between its snapshot
and the live WebSocket stream.

During shutdown, queued and in-flight frames get a 250 ms dispatch grace period.
If a subscriber remains blocked, shutdown abandons the remaining work;
subscribers reached before the block may already have received the current
event. This partial-fanout possibility is limited to explicit shutdown of this
volatile source.

There is no client-initiated Ping or idle timeout, so a half-open connection can
remain `Running` until socket I/O reports the failure. Client liveness probing,
reconnect jitter, application-level heartbeat replies, readiness checks, binary
JSON, subprotocol selection, custom trust roots, mTLS, identity providers, and
WAL durability are intentionally left for later versions.

## Development

Run the package preflight from the repository root:

```bash
cargo fmt --all -- --check
cargo test -p drasi-source-websocket --all-features --locked
cargo clippy -p drasi-source-websocket --all-targets --all-features --locked -- -D warnings
cargo build -p drasi-source-websocket --release --features dynamic-plugin --locked
cargo package -p drasi-source-websocket --allow-dirty --no-verify --locked
cargo run -q -p xtask -- list-plugins
```

After the release build, inspect the produced dynamic library and verify that
`drasi_plugin_metadata` exports the Cargo package version and the exact
workspace `drasi-core` and `drasi-lib` versions.
