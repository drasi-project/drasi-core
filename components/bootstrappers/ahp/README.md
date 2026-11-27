# AHP Bootstrap Provider

`drasi-bootstrap-ahp` loads the initial graph for queries over an
[Agent Host Protocol (AHP)](https://github.com/microsoft/agent-host-protocol)
host. It is the bootstrap companion of
[`drasi-source-ahp`](../../sources/ahp) and uses the same mapping
(`drasi-ahp-common`), so element IDs and properties match what the source
streams afterwards. See the source README for the graph model.

For each bootstrap request it opens a new WebSocket connection, calls
`initialize`, pages through `listSessions`, subscribes to each selected
session (and `ahp-automations://` when supported) to read their snapshots,
emits every element as an `Insert` (nodes before relations, filtered by the
requested labels), and closes the connection. No `source_position` is
reported (the AHP source is volatile).

## Configuration

| Field | Default | Description |
|-------|---------|-------------|
| `url` | — (required) | WebSocket URL of the host |
| `bearerToken` | none | `Authorization: Bearer` token for the upgrade request |
| `clientId` | `drasi-<source id>-bootstrap` | `initialize` client ID |
| `resourceTokens` | `[]` | `{ "resource", "token" }` pairs pushed with `authenticate` |
| `providers` | `[]` (all) | Only include sessions of these agent providers |
| `includeArchived` | `true` | Include archived sessions |
| `subscribeSessions` | `true` | Read session channels for chats and input requests |
| `includeAutomations` | `true` | Include the automation catalogue |
| `pageSize` | `100` | `listSessions` page size |
| `connectTimeoutMs` | `10000` | Connect timeout |
| `requestTimeoutMs` | `30000` | Request timeout |

When loaded as a plugin, any of `url`, `bearerToken`, `resourceTokens`,
`providers`, `includeArchived`, `subscribeSessions`, `includeAutomations`,
`pageSize`, `connectTimeoutMs`, and `requestTimeoutMs` omitted from the
bootstrap config are inherited from the AHP source config, so an empty `{}`
bootstrap config is enough. Keep selection settings equal to the source's.

```rust,ignore
let provider = AhpBootstrapProvider::builder()
    .with_url("ws://localhost:4321")
    .with_providers(vec!["copilot".to_string()])
    .build()?;
```

## Testing

`tests/bootstrap_tests.rs` runs the provider against the in-process mock AHP
host (pagination, label and archive filters, bearer token):

```bash
cargo test -p drasi-bootstrap-ahp
```
