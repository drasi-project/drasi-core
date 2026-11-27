# drasi-ahp-common

Shared building blocks for the Drasi
[Agent Host Protocol (AHP)](https://github.com/microsoft/agent-host-protocol)
source (`drasi-source-ahp`) and bootstrap provider (`drasi-bootstrap-ahp`).

- `model` — graph labels, relation types, element-ID generation, and the
  projected element type. Both plugins MUST build elements only through this
  module so their IDs agree.
- `mirror` — `AhpMirror` (local copy of root, session, and automation state,
  updated with the official `ahp` reducers and root catalogue
  notifications) and `GraphProjection` (per-scope projection that diffs state
  into ordered inserts/updates/deletes).
- `client` — a lossless JSON-RPC 2.0 client over WebSocket built on the
  generated `ahp-types` wire types and the workspace `tokio-tungstenite`.
- `sync` — `AhpClientConfig` and `AhpSession`, which performs `initialize`,
  `authenticate`, paginated `listSessions`, and channel subscriptions, then
  keeps the mirror current.
- `mock` (feature `mock-host`) — `MockAhpHost`, an in-process AHP host
  served over WebSocket for tests and demos. State changes go through
  `MockAhpHost::dispatch`, which runs the official reducers, broadcasts
  `action` envelopes, and emits `root/sessionSummaryChanged` notifications.

The `ahp`/`ahp-types` crates (MIT) are used for wire types and reducers only;
`ahp-ws` is not used because it pins a different `tokio-tungstenite` major
version than this workspace.
