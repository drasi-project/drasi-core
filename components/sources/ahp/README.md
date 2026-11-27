# AHP Source Plugin

`drasi-source-ahp` connects Drasi to an
[Agent Host Protocol (AHP)](https://github.com/microsoft/agent-host-protocol)
host and turns its agent sessions, chats, pending input requests, agents, and
automations into a live graph. Continuous queries can then react to agent
activity — for example "a session is waiting for my input", "a session
failed", or "an agent changed more than 20 files".

It pairs with the [`drasi-bootstrap-ahp`](../../bootstrappers/ahp) bootstrap
provider. Both share their graph mapping through `drasi-ahp-common`.

## How it works

- Opens a WebSocket to the host and speaks AHP JSON-RPC 2.0 (`initialize`,
  `listSessions`, `subscribe`, `unsubscribe`, `authenticate`, `ping`).
- Subscribes to `ahp-root://` (agents and the session catalogue), to each
  selected `ahp-session:/<id>` channel (chats and input requests), and to
  `ahp-automations://` when the host advertises automations.
- Applies every `action` envelope to a local state mirror with the official
  `ahp` crate reducers, applies `root/sessionAdded|sessionRemoved|sessionSummaryChanged`
  notifications to the session catalogue, and re-projects only the affected
  part of the graph. The difference between the previous and new projection
  is emitted as `Insert`/`Update`/`Delete` changes, so no AHP action type is
  mapped by hand.
- High-frequency chat content (`ahp-chat:/` channels, `chat/delta` text,
  turns, tool calls) is deliberately not mirrored.

## Graph model

```text
(:Session)-[:USES_AGENT]->(:Agent)
(:Session)-[:HAS_CHAT]->(:Chat)
(:Session)-[:NEEDS_INPUT]->(:InputRequest)-[:IN_CHAT]->(:Chat)
(:Automation)-[:HAS_RUN]->(:AutomationRun)-[:CREATED_SESSION]->(:Session)
```

| Label | Element ID | Properties |
|-------|------------|------------|
| `Agent` | `ahp-agent:<provider>` | `provider`, `displayName`, `description`, `modelCount`, `requiresAuth` |
| `Session` | session URI (`ahp-session:/<id>`) | `uri`, `id`, `provider`, `title`, `status`, `state`, `isIdle`, `isInProgress`, `needsInput`, `hasError`, `isRead`, `isArchived`, `activity`, `lifecycle`, `creationError`, `createdAt`, `modifiedAt`, `additions`, `deletions`, `filesChanged`, `projectName`, `projectUri`, `chatCount`, `activeClientCount`¹, `pendingInputCount`¹ |
| `Chat` | chat URI (`ahp-chat:/<id>`) | `uri`, `id`, `sessionUri`, `title`, `status`, `state` and status flags, `activity`¹, `modifiedAt`¹, `provider`, `isDefault`, `additions`, `deletions`, `filesChanged` |
| `InputRequest` | `<session URI>#input:<request id>` | `id`, `kind` (`chatInput`, `toolConfirmation`, `toolClientExecution`, `toolAuthentication`), `sessionUri`, `chatUri`, `turnId`, `message`, `questionCount`, `toolName`, `toolDisplayName` |
| `Automation` | automation URI | `uri`, `id`, `title`, `enabled`, `triggerCount`, `nextRunAt`, `runCount`, `createdAt`, `modifiedAt` |
| `AutomationRun` | run URI (`ahp-automation-run:/<id>`) | `uri`, `id`, `automationUri`, `status` (`pending`, `running`, `completed`, `failed`, `cancelled`), `originKind`, `error`, `primarySessionUri`, `sessionCount` |

Relation IDs are `<from>|<TYPE>|<to>`. Relations have no properties.

¹ Only populated while the session channel is subscribed
(`subscribeSessions: true`); otherwise `null`.

`status` is the raw AHP `SessionStatus` bitset (Idle=1, Error=2,
InProgress=8, InputNeeded=24, IsRead=32, IsArchived=64). `state` is a
convenience string: `error`, `input-needed`, `in-progress`, `idle`, or
`unknown`. Timestamps are ISO-8601 strings as sent by the host; use
`datetime(s.modifiedAt)` in queries.

## Configuration

| Field | Default | Description |
|-------|---------|-------------|
| `url` | — (required) | WebSocket URL of the host (`ws://` or `wss://`) |
| `bearerToken` | none | Sent as `Authorization: Bearer <token>` on the upgrade request. A 401/403 is fatal. |
| `clientId` | `drasi-<source id>` | `initialize` client ID |
| `resourceTokens` | `[]` | `{ "resource", "token" }` pairs pushed with the AHP `authenticate` command for agents that declare `protectedResources` |
| `providers` | `[]` (all) | Only include sessions of these agent providers |
| `includeArchived` | `true` | Include archived sessions; when `false`, archiving a session deletes it from the graph |
| `subscribeSessions` | `true` | Subscribe to each selected session channel for full chat and input-request detail. When `false`, only catalogue summaries are used (no `InputRequest` nodes) |
| `includeAutomations` | `true` | Mirror `ahp-automations://` when the host supports it |
| `pageSize` | `100` | `listSessions` page size |
| `connectTimeoutMs` | `10000` | WebSocket connect timeout |
| `requestTimeoutMs` | `30000` | JSON-RPC request timeout |
| `keepaliveIntervalMs` | `30000` | Interval between `ping`s; `0` disables. A failed ping triggers a reconnect |
| `reconnectInitialDelayMs` | `1000` | Initial reconnect backoff |
| `reconnectMaxDelayMs` | `30000` | Maximum reconnect backoff |
| `startupSyncTimeoutMs` | `15000` | How long `start()` waits for the initial snapshot (see below) |

`url`, `bearerToken`, and `resourceTokens[].token` accept `ConfigValue`
references (environment variables, secrets).

Plugin configuration (the object passed to the `ahp` source descriptor):

```json
{
  "url": "ws://localhost:4321",
  "bearerToken": { "kind": "Secret", "name": "ahp-token" },
  "providers": ["copilot"],
  "includeArchived": false
}
```

The `ahp` bootstrap descriptor accepts `{}` and inherits the connection and
selection settings from the source config.

Rust:

```rust,ignore
let source = AhpSource::builder("agents")
    .with_url("ws://localhost:4321")
    .with_include_archived(false)
    .with_bootstrap_provider(
        AhpBootstrapProvider::builder()
            .with_url("ws://localhost:4321")
            .with_include_archived(false)
            .build()?,
    )
    .build()?;
```

## Bootstrap

Bootstrap is supported through `drasi-bootstrap-ahp`, which reads the same
snapshot over its own connection. Keep the selection settings (`providers`,
`includeArchived`, `subscribeSessions`, `includeAutomations`) identical on
both; the plugin bootstrap descriptor inherits them from the source config
when omitted.

`start()` waits up to `startupSyncTimeoutMs` for the initial snapshot. That
snapshot becomes the silent streaming baseline: queries receive initial data
from the bootstrap provider, and the source emits only later changes. If the
host is not reachable within the timeout (or the timeout is `0`), the source
keeps retrying in the background and emits its first snapshot as inserts —
useful when running without a bootstrap provider.

## Replay, durability, and delivery

This is a **volatile** source (`supports_replay() == false`): it emits no
`source_position` and cannot resume persistent queries from a checkpoint;
they re-bootstrap after a restart.

On connection loss (socket error, close, or failed keepalive `ping`) the
source reconnects with exponential backoff, reloads full snapshots (AHP does
not replay `root/session*` notifications), and emits the diff between the
last emitted graph and the new snapshot. Changes made while disconnected are
therefore delivered as their net effect; intermediate states may be skipped.
Because element IDs are stable, emitted changes are idempotent upserts and
deletes.

Unlike the upstream `ahp::Client` (which drops messages for lagging
subscribers), the plugin's JSON-RPC client delivers every notification in
order through an unbounded queue, so the mirror never silently diverges.

## Status and shutdown

- `start()` returns an error and the status becomes `Error` for fatal
  failures: rejected credentials (HTTP 401/403, `authenticate` errors) or an
  `initialize` error such as an unsupported protocol version.
- Network failures are retried indefinitely while the status stays `Running`.
- `stop()` signals the streaming task, closes the WebSocket, and clears
  channel dispatchers.

## Testing

Unit tests cover config, mapping, and diffing. `tests/integration_tests.rs`
runs the source, bootstrap provider, and real continuous queries against the
in-process mock AHP host from `drasi-ahp-common` (feature `mock-host`), which
serves the real AHP wire format over WebSocket and uses the official
reducers. No external services are needed:

```bash
cargo test -p drasi-source-ahp
```

See [`examples/lib/ahp-getting-started`](../../../examples/lib/ahp-getting-started)
for a runnable demo.
