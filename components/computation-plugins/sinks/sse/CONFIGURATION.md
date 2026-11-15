# Native SSE sink: configuration and management

Implementation `drasi.network/sse-sink`, version `1`, configuration version
`1`. For how the sink works, see [DESIGN.md](DESIGN.md).

```sh
cargo build -p drasi-computation-sink-sse --features dynamic-plugin
```

## Fields

All keys use camelCase. Unknown keys are rejected. Nothing is required. The
published configuration schema carries the same descriptions and defaults,
and `configuration()` returns every field, secrets included (it is the
persistence hook).

| Field | Type | Default | Range / rule | Update |
|---|---|---|---|---|
| `outputs` | object | `{}` | Alias → `{component, port?}`. Optional names for incoming outputs; an unaliased output is named after its producing component. Aliases name routes and snapshot URLs. | Replace |
| `host` | string | `127.0.0.1` | Literal IP address. A non-loopback address needs `authToken` or `allowUnauthenticated: true` (see [Exposure](#exposure)). | Stop |
| `port` | integer | `8080` | `0..=65535`. `0` picks a free port, shown only in the log. | Stop |
| `ssePath` | string | `/events` | Absolute path; no `?`, `#` or control characters; not `/snapshot` or below it. | Stop |
| `heartbeatIntervalMs` | integer | `30000` | ≥ 1, representable. JSON heartbeat data frames; suspended while paused. | Pause (new connections) |
| `keepAliveIntervalMs` | integer | `15000` | ≥ 1, representable. `: keep-alive` comments; continue while paused. | Pause (new connections) |
| `eventIds` | boolean | `false` | Adds `id:` with a per-path counter from 1 per activation. Volatile; `Last-Event-ID` is ignored. | Pause |
| `broadcastCapacity` | integer | `1024` | `1..=1048576`. Frames buffered per path; a subscriber further behind is disconnected. | Stop |
| `routes` | object | `{}` | Alias → `{added?, updated?, deleted?}`, each `{template?, path?}`. Every key must name an incoming output alias (full alias or its last dotted segment). See [Templates and paths](#templates-and-paths). | Pause |
| `defaultTemplate` | object | none | Same shape as one route; used for outputs without a route. | Pause |
| `maxPaths` | integer | `1024` | `1..=65536`. Static plus dynamic paths, including `ssePath`. Idle dynamic paths are pruned at the bound. | Stop |
| `maxSubscribers` | integer | `1024` | `1..=65536`. Connected event streams across all paths; more get `503`. Lowering it disconnects no one. | Pause |
| `maxConnections` | integer | `1024` | `1..=65536`. Open TCP connections; excess connections are closed at accept. | Stop |
| `headerReadTimeoutMs` | integer | `10000` | `1..=600000`. Deadline for request headers and the TLS handshake. | Stop |
| `authToken` | string, **secret** | none | 16–4096 visible ASCII characters. Required on event-stream and snapshot routes as `Authorization: Bearer <token>` or `?access_token=<token>`. | Pause |
| `authTokenSecondary` | string, **secret** | none | A second accepted token, for rotation. Requires `authToken`; must differ from it. | Pause |
| `allowQueryToken` | boolean | `true` | Accept the token as the `access_token` query parameter (percent-decoded). Has no effect without `authToken`. | Pause |
| `allowUnauthenticated` | boolean | `false` | Permit a non-loopback listener without `authToken`, for use behind an authenticating proxy. | Pause |
| `corsAllowedOrigins` | array of strings | `[]` | At most 64. `["*"]` alone allows any origin; otherwise distinct `http(s)://host[:port]` origins without a path. Empty sends no CORS headers. | Pause |
| `rateLimitPerSecond` | integer | none (off) | `1..=1000000` per client IP, for new subscriptions, snapshot reads and preflights. | Pause |
| `rateLimitBurst` | integer | `rateLimitPerSecond` | `1..=1000000`. Requires `rateLimitPerSecond`. | Pause |
| `tlsCertificatePem` | string | none | PEM certificate chain, ≤ 256 KiB. Requires `tlsPrivateKeyPem`. | Stop |
| `tlsPrivateKeyPem` | string, **secret** | none | PEM private key matching the certificate. | Stop |
| `tlsClientCaPem` | string | none | PEM CA bundle. When set, clients must present a certificate it issued (mutual TLS). Requires TLS. | Stop |

"Pause (new connections)" means the change is applied in place, but streams
that are already connected keep their old interval until they reconnect.

## Examples

Local development (loopback, no authentication):

```json
{
  "outputs": {"alerts": {"component": "warehouse.alerts", "port": "out"}},
  "port": 8080
}
```

A browser dashboard on another origin, exposed on the network:

```json
{
  "outputs": {"alerts": {"component": "warehouse.alerts", "port": "out"}},
  "host": "0.0.0.0",
  "port": 8443,
  "authToken": "<secret reference>",
  "corsAllowedOrigins": ["https://dashboard.example.com"],
  "tlsCertificatePem": "-----BEGIN CERTIFICATE-----\n…",
  "tlsPrivateKeyPem": "<secret reference>",
  "maxSubscribers": 500,
  "rateLimitPerSecond": 5,
  "rateLimitBurst": 20
}
```

The browser connects with
`new EventSource("https://sse.example.com:8443/events?access_token=" + encodeURIComponent(token))`
and reads the snapshot with `fetch(url, {headers: {Authorization: "Bearer " + token}})`.

Behind an authenticating reverse proxy on the same host:

```json
{"host": "0.0.0.0", "allowUnauthenticated": true, "allowQueryToken": false}
```

## Connecting

Clients open an `EventSource` (or any SSE client) at a configured path. A
data frame without a route is:

```json
{"output": {"alias": "alerts", "component": "…", "port": "out"},
 "sequence": 42, "publishedAt": 1700000000000, "changes": [ … ]}
```

Each change states its real `operation` (`ADD`, `REPLACE`, `PATCH`,
`DELETE`), its canonical `identity` (`namespace`, `id`) and only the
`element`, `before`, `after` (a patch's `after` holds only the properties it
sets), `removedProperties` and `effectiveFrom` that the record carries. An
unknown before image is absent, never `null`. `publishedAt` is the sink's
wall clock in milliseconds, not an event time. Heartbeats are
`{"type":"heartbeat","ts":<ms>}`; keep-alives are SSE comments that
`EventSource` ignores. An input with no changes publishes nothing; a
scheduling control or recovery frame fails the input.

Responses:

| Status | Meaning |
|---|---|
| `200` | Event stream (`text/event-stream`, `cache-control: no-cache`, `x-accel-buffering: no`) or snapshot. |
| `401` | Missing or wrong token (`WWW-Authenticate: Bearer`). |
| `404` | Unknown path (a dynamic path before its first frame or after pruning), or unknown alias. |
| `405` | A method other than GET or OPTIONS on an event path. |
| `429` | Client rate limit exceeded (`Retry-After: 1`), or snapshot readers busy. |
| `501` | No snapshot provider for the output, or no snapshot service bound. |
| `503` | Paused, stopping, subscriber cap reached (`Retry-After: 1`), or snapshot not ready. |

## Templates and paths

`routes` and `defaultTemplate` use the shared SSE renderer and template types
from `drasi-reaction-common`:

```json
{
  "outputs": {"alerts": {"component": "warehouse.alerts", "port": "out"}},
  "routes": {
    "alerts": {
      "added": {"template": "{{json after}}", "path": "zones/{{after.zone}}"},
      "updated": {"template": "{{json after}}", "path": "zones/{{after.zone}}"},
      "deleted": {"template": "{{json before}}", "path": "/removed"}
    }
  }
}
```

- A full output alias takes precedence over its last dotted segment, which
  takes precedence over `defaultTemplate`.
- `updated` covers `REPLACE` and `PATCH`. A selected route with no template for
  an operation filters that operation out.
- With no selected route, one frame carries the input's whole `changes` array.
  An explicitly empty `template` uses the per-change frame
  `{output, sequence, publishedAt, change}`.
- The template context is the change notification: `operation`, `output`,
  `sequence`, `timestamp` (when the envelope has one), `identity`, and the
  `element`, `before`, `after`, `removedProperties` and `effectiveFrom` the
  record carries. The `json` helper renders a value as JSON.
- A body that fails to render falls back to the per-change frame; a path that
  fails to render falls back to the configured path. Both log a warning. A
  rendered path that is invalid fails the input.
- Relative paths are appended to `ssePath`; absolute paths are used as they are.
  Paths without `{{` are static and available at once. Templated paths are
  dynamic: available after their first frame, pruned when idle at the
  `maxPaths` bound, and cleared by restart.
- Carriage returns in rendered bodies are normalised to `\n`.

## Output snapshots

`GET /snapshot/:alias` streams the current state of the aliased output as a
JSON array of canonical `ADD` changes, with `x-drasi-output-epoch` and
`x-drasi-output-position` headers. It is authenticated, paused and rate
limited like the event routes. See
[DESIGN.md](DESIGN.md#snapshot-handover) for using it with a live stream.

Bind the factory's optional `output_snapshots` dependency to a
`NativeOutputSnapshotsResource`, an immutable allow-list and limits policy:

```json
{
  "kind": "nativeOutputSnapshots",
  "outputs": [{"component": "warehouse.alerts", "port": "out"}],
  "timeoutMs": 300000,
  "maxReaders": 16
}
```

Declare it with role `OutputCatalogue` (no dependencies) and bind it to the
SSE component's `output_snapshots` slot. A selector without a port allows the
component's primary output. A sink output outside the allow-list gets `404`;
a sink with no resource bound gets `501`.

## Exposure

Recommended deployments, from most to least protected:

1. **Loopback** (default) behind a reverse proxy on the same host that
   terminates TLS and authenticates users (cookies or SSO). Use
   `allowUnauthenticated: true` only if the proxy must reach a non-loopback
   address, and firewall the port so only the proxy can reach it.
2. **Direct exposure with TLS and a token.** Set `host`, `authToken`, TLS and
   `corsAllowedOrigins` for the dashboard origin only. Rotate tokens by adding
   the new one as `authTokenSecondary`, moving clients, then promoting it to
   `authToken` (each step is an in-place update).
3. **Mutual TLS** (`tlsClientCaPem`) for service clients that can hold
   certificates.

The sink refuses a non-loopback `host` with neither `authToken` nor
`allowUnauthenticated: true`, at construction and when assessing an update.

**Query-string tokens.** Browsers' `EventSource` cannot send headers, so the
token is accepted as `?access_token=`. Query strings may be recorded in proxy,
load-balancer and server access logs, in browser history, and leak through
`Referer`. Use TLS, keep access logs private or strip the parameter, give
browser clients a token separate from service clients, and rotate it.
Set `allowQueryToken: false` when every client can send a header.

**CORS** controls which web origins may *read* responses in a browser. It is
not authentication: a non-browser client ignores it. Never use `["*"]` with a
token that browsers on arbitrary origins could obtain.

**Limits.** `maxConnections` and `headerReadTimeoutMs` protect the accept
loop; `maxSubscribers` protects memory and file descriptors; the rate limit
slows reconnect storms and token guessing. Behind a proxy, the rate limit
sees the proxy's address.

## Updates

| Boundary | Changes |
|---|---|
| Pause (InPlace) | `routes`, `defaultTemplate`, `heartbeatIntervalMs`, `keepAliveIntervalMs`, `eventIds`, `authToken`, `authTokenSecondary`, `allowQueryToken`, `allowUnauthenticated`, `corsAllowedOrigins`, `rateLimitPerSecond`, `rateLimitBurst`, `maxSubscribers` |
| Stop (Restart) | `host`, `port`, `tlsCertificatePem`, `tlsPrivateKeyPem`, `tlsClientCaPem`, `ssePath`, `broadcastCapacity`, `maxPaths`, `maxConnections`, `headerReadTimeoutMs` |
| Replace (Recreate) | `outputs`, or a change to the incoming edges |

The strongest boundary of any changed field wins. An invalid proposal (for
example, a route that names no incoming output, or unsafe exposure) is an
assessment error, never an implicit Replace. A Restart closes every stream;
clients reconnect and reload snapshots.

## Lifecycle and removal

- **Pause** refuses new subscriptions and snapshot reads with `503`, ends
  in-flight snapshot reads, and stops heartbeats. Connected streams stay open
  with keep-alive comments only.
- **Stop (Drain)** stops accepting, ends streams gracefully, and waits up to the
  host's deadline (30 s without one). On timeout the error is retryable and
  the listener is retained until a later Stop finishes it.
- **Stop (Abort)** closes every connection at once.
- **Removal** with `StateIntent::Erase` succeeds and removes nothing: the sink
  owns no state. Retrying it is safe.

## Operations

- A stream that ends with no error during normal running usually means the
  client lagged by more than `broadcastCapacity`; look for the "lagged" warning
  and raise the capacity or speed up the client.
- `rendered SSE paths exceed maxPaths` means every dynamic path has a
  subscriber and the bound is reached: raise `maxPaths` or route to fewer
  paths.
- `503` with `Retry-After` during an update is a pause; clients should retry.
- Set the `drasi.network` log target to `debug` to see connection refusals,
  pruning and TLS handshake failures.

## Legacy configuration mapping

From the legacy `reactions/sse` reaction:

| Legacy | Native | Notes |
|---|---|---|
| `host` (default `0.0.0.0`) | `host` (default `127.0.0.1`) | **Breaking.** Non-loopback needs `authToken` or `allowUnauthenticated: true`. |
| `port` (8080) | `port` (8080) | Unchanged. |
| `ssePath` (`/events`) | `ssePath` (`/events`) | `/snapshot` and below are now reserved. |
| `heartbeatIntervalMs` (30000) | `heartbeatIntervalMs` (30000) | Heartbeats now stop while paused. |
| fixed 30 s `keep-alive` comment | `keepAliveIntervalMs` (15000) | Configurable. |
| `routes` keyed by query ID | `routes` keyed by output alias | Context is the native change notification; `query_id`, `query_name` and `data` are gone. |
| `defaultTemplate` | `defaultTemplate` | Same shape. |
| CORS `Any` on all routes | `corsAllowedOrigins` (default none) | **Breaking.** Set `["*"]` to restore the old behaviour. |
| query subscriptions | incoming graph edges + `outputs` | The graph supplies bindings. |
| `/snapshot/:query_id` | `/snapshot/:alias` | Reads output snapshots of any output; needs the `output_snapshots` resource. |
| — | `broadcastCapacity`, `maxPaths`, `maxSubscribers`, `maxConnections`, `headerReadTimeoutMs`, `eventIds`, auth, rate limit and TLS fields | New. |
