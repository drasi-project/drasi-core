# Native HTTP bootstrap

`drasi-computation-bootstrap-http` exports only the query-scoped
`drasi.network/http-bootstrap` factory (implementation `"1"`, configuration `1`).
It is a native bootstrap provider, not a source component, standalone service or
second graph. Static linking through `NativePlugin::from_definition(plugin()?)`
and separately built dynamic loading use the same SDK/Host contract.

```sh
cargo build -p drasi-computation-bootstrap-http --features dynamic-plugin
```

## Configuration

```json
{
  "settings": {
    "endpoints": [{
      "url": "https://api.example/rooms",
      "auth": {"type": "bearer", "tokenEnv": "ROOM_API_TOKEN"},
      "pagination": {"type": "offset-limit", "pageSize": 100},
      "response": {
        "itemsPath": "$.items",
        "mappings": [{
          "elementType": "node",
          "template": {
            "id": "{{item.id}}",
            "labels": ["Room"],
            "properties": {"value": "{{item.value}}"}
          }
        }]
      }
    }],
    "timeoutSeconds": 30,
    "maxRetries": 3,
    "retryDelayMs": 1000,
    "maxPages": 10000
  },
  "nodeLabels": ["Room"],
  "outputCapacity": 64,
  "maxResponseBytes": 16777216,
  "maxDurationMs": 300000,
  "shutdownTimeoutMs": 5000
}
```

`settings` is required. Other fields have the defaults shown, except absent
`nodeLabels`/`relationLabels` select all mapped labels. They are an upper
bound: each query receives only the mapped elements whose labels it declares
(the query's labels arrive with its bootstrap request).

The served source and stream are not configured. The provider declares a
quiescent completion in its served source's namespace; the graph relation
supplies that source: the HTTP or gRPC source whose `bootstrap` input the
provider node is connected to, or the single source a query reads when the
provider is that query's local provider. An edge that gives the provider no
single served source is refused when it binds. See the
[native bootstrap provider standard](../NATIVE-BOOTSTRAP-PROVIDER-GUIDE.md).
`settings` uses the existing [HTTP bootstrap configuration](../../../bootstrappers/http/README.md):
multiple endpoints, GET/POST/PUT, headers/body, bearer/API-key/basic/OAuth2 client
credentials, offset/page/cursor/Link/next-URL pagination, JSON/XML/YAML parsing
and node/relation response mapping. Unlike the legacy provider's descriptor, every
key is camelCase at every level (`tokenEnv`, `usernameEnv`, `clientIdEnv`,
`offsetParam`, `pageSizeParam`, `pageSize`, `cursorPath`, `nextUrlPath`, ...)
and an unknown key at any level is refused when the graph asks for the
provider's description, before any I/O. The published schema comes from
the same types (`definition()`), with the defaults shown above. The actual mapping and REST reader are
shared with the legacy provider, not copied into another transport.

Response bodies are parsed by the same content parser as the HTTP source's
webhook routes (`drasi_http_common::content`), and item templates render with
the same template engine (`drasi_http_common::template`), so one XML or JSON
document maps to the same values on both paths. XML elements that contain only
text are strings (`<id>7</id>` is `"7"`, never `{"$text": "7"}`), attributes
are `@name` fields, mixed text is `_text`, repeated elements are arrays and the
document element is unwrapped. A response with `Content-Type: text/plain` is a
string; set `response.contentType` to parse it as JSON, XML or YAML. Templates
have the source-mapping helpers (`lowercase`, `uppercase`, `concat`, `default`,
`json`, ...); a single-path template (`{{item.count}}`) keeps the value's JSON
type, and any other template's text becomes JSON when it parses as JSON.

## Hosting and lifetime

In a graph, connect the provider node's `snapshot` output to the live
source's `bootstrap` input; each session gets its own provider. Outside a
graph, create one provider for each query with
`NativeBootstrapFactory::create_query_provider`, passing the hosting
instance/graph scope, that query's component ID and a `BootstrapSession`
naming the served source and stream, and bind it with
`computation_pipeline().query_bootstrap(query_id, provider)`. Use the same
labels as the live HTTP/gRPC source.

Construction validates configuration but opens no socket, resolves no credential
environment variables and starts no worker. Snapshot polling owns the REST
reader and a bounded output channel. Dropping the stream cancels it; `stop`
awaits its worker and retains cleanup ownership after a cancelled/timed-out
stop. Old streams cannot cancel a replacement snapshot.

Both request/body failures and truncated JSON/XML/YAML responses retry the same
page. Native snapshots fail explicitly on mapping errors, pagination cycles or
exhausted page limits; they never report a truncated initial state as complete.
Response bodies and output admission are bounded, OAuth responses are limited
to 64 KiB, redirects are disabled, and followed pagination URLs stay on the
endpoint's origin. The whole snapshot has an independent deadline.

REST snapshots do **not** establish an atomic live-source/replay boundary and
return no invented watermark. The application must order initialization before
live changes, or provide an upstream-consistent snapshot protocol. Query-owned
completion/recovery state remains in the hosting graph.
