# Drasi AHP Reaction

`drasi-reaction-ahp` turns Drasi continuous-query changes into agent work on an
[Agent Host Protocol (AHP)](https://github.com/microsoft/agent-host-protocol) host.
It connects to the host over JSON-RPC 2.0 on WebSocket using the
[`ahp`](https://crates.io/crates/ahp) Rust SDK and correlates each query result row
(by `resultKeyFields`) with one AHP session or automation run.

## Modes

### `session` (default)

| Query change | AHP operation |
|---|---|
| `ADD` | `createSession` on a deterministic `ahp-session:/drasi-…` URI, wait for `session/ready`, optionally set the title (`session/titleChanged`), then start a turn on the session's default chat with `chat/turnStarted` and the rendered `added` message. |
| `UPDATE` | `chat/pendingMessageSet` on the correlated chat with the rendered `updated` message (`followUpKind`: `queued` starts a new turn when the current one finishes; `steering` is injected into the active turn). If the chat no longer exists, a new session is started. An `UPDATE` with no correlated session starts one. |
| `DELETE` | If a `deleted` template is configured it is sent as a closing follow-up. Then `onDelete` applies: `cancel` (default) dispatches `chat/turnCancelled` for an active turn; `archive` also dispatches `session/isArchivedChanged`; `dispose` also calls `disposeSession`; `none` leaves the session alone. The correlation is forgotten. |

### `automation`

| Query change | AHP operation |
|---|---|
| `ADD` | `runAutomation` for `automation` with a deterministic `requestId` (hosts deduplicate retries). The change envelope is passed in `_meta.drasi`. |
| `UPDATE` | Starts a run when none is correlated; otherwise dropped (runs take no follow-up input). |
| `DELETE` | Unless `onDelete` is `none`, dispatches `automationRun/cancelRequested` on the run. |

Every dispatched action waits for the host's echo; a `rejectionReason` (other than
for best-effort cancellations) fails the diff. `AuthRequired` (`-32007`) responses
re-push the configured token with `authenticate` and retry once.

## Configuration

```yaml
kind: ahp
endpoint: wss://agent-host.example.com/ahp   # ws:// or wss://, required
token: ${AHP_TOKEN}                          # optional bearer token
authResource: https://agent-host.example.com # push token via `authenticate`
clientId: drasi-incident-agents              # default: drasi-reaction-<id>
timeoutMs: 10000                             # request + action-ack timeout
sessionReadyTimeoutMs: 60000
mode: session                                # session | automation
resultKeyFields: [id]                        # required
provider: copilot                            # session mode: agent provider
workingDirectories: [file:///repo]
sessionConfig: { model: gpt-5 }
titleTemplate: "{{after.id}}: {{after.title}}"
defaultTemplate:
  added:   { template: "Investigate {{after.id}}: {{after.title}}" }
  updated: { template: "Severity is now {{after.severity}}" }
  deleted: { template: "{{before.id}} is resolved; wrap up." }
routes:                                      # per query id (or last dotted segment)
  critical-incidents:
    added: { template: "SEV{{after.severity}}: {{json after}}" }
followUpKind: queued                         # queued | steering
onDelete: cancel                             # none | cancel | archive | dispose
automation: ahp-automation:/triage           # required in automation mode
durable: false
priorityQueueCapacity: 10000
recoveryPolicy: strict                       # strict | auto_skip_gap
```

- `endpoint` is trusted operator configuration; loopback/private addresses are allowed.
- `token` is sent as `Authorization: Bearer` on the WebSocket handshake. With `authResource`
  it is also pushed with `authenticate` after `initialize`.
- Setting `automation` in the plugin DTO implies `mode: automation` unless `mode` is set.

### Templates

Templates use Handlebars with the standard context: `query_id`, `query_name`,
`operation`, `timestamp`, `sequence`, `result_key`, `metadata`, `before`, `after`,
`data`, plus the `{{json value}}` helper. Lookup order is route by query id → route by
the query id's last dotted segment → `defaultTemplate`.

- No template (or empty output) → a built-in Markdown message describing the change.
- Message template render failure → logged, then the built-in message is used.
- `titleTemplate` render failure → logged, and the host's default title is kept.
- A `deleted` template that is missing or fails to render sends no closing message.

Every message also carries the canonical change envelope in `Message._meta.drasi`:
`queryId`, `sequenceId`, `timestamp`, `operation`, `resultKey`, `before`/`after`, and
`metadata` (omitted when empty).

## Identity, idempotency and durability

- Work ids are `drasi-<sha256(len-prefixed reactionId, queryId, resultKey)[..24 hex]>-<sequence>-<diffIndex>`.
  They name the session (`ahp-session:/<id>`), the first turn, follow-up pending-message ids,
  and the automation `requestId`.
- Replaying an `ADD` is safe: `SessionAlreadyExists` (`-32003`) reuses the session and a turn
  whose id already exists on the chat is not restarted. Repeated pending-message upserts reuse
  the same id.
- Correlations (`activation:<len-prefixed queryId, resultKey>` → session/chat or run) are kept
  in memory and, when a state store is configured, persisted to it. Query checkpoints use
  `ReactionBase::run_standard_loop`.
- `durable: true` makes `is_durable()` return `true` so the host requires a persistent state
  store and persistent queries. It defaults to `false`.
- `default_recovery_policy()` returns the per-instance `recoveryPolicy` (default `Strict`).
  Each diff is attempted up to 3 times with a short backoff (safe because every operation is
  idempotent). If it still fails, `Strict` stops the reaction with `ComponentStatus::Error`;
  `AutoSkipGap` logs and skips. Any failed operation drops the WebSocket connection; the next
  operation reconnects.

## Mock host

The `mock-host` feature exposes `drasi_reaction_ahp::mock_host::MockAgentHost`, an in-process
AHP host for tests and demos. It implements the subset of AHP this reaction uses and simulates
an agent that completes each turn after a delay and then consumes queued follow-ups. It is not
a production host.

## Example

See [`examples/lib/ahp-incident-agents`](../../../examples/lib/ahp-incident-agents/README.md).

## Tests

```bash
cargo test -p drasi-reaction-ahp
```

Tests run against the mock host and cover ADD/UPDATE/DELETE in both modes, archive/dispose,
replay idempotency, restart with a state store, authentication, and the plugin descriptor.
