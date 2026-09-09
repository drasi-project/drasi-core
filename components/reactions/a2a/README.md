# Drasi A2A Reaction

`drasi-reaction-a2a` sends Drasi continuous query diffs to an A2A-compatible JSON-RPC 2.0 endpoint.

## Endpoint and agentgateway

The reaction has one transport mode: HTTP POST to `endpoint`.  
`agentgateway` is treated as the same thing as any other A2A endpoint URL.

## Behavior

- `ADD` starts work with `SendMessage` and no `taskId`.
- `UPDATE` follows up with `SendMessage` and the current activation `taskId`.
- `DELETE` cancels active tasks with `CancelTask`.
- A `SendMessage` response parsed as `Message` is treated as one-shot and receives no future follow-up/cancel.
- A `SendMessage` response parsed as `Task` is mapped by task state:
  - Active: `WORKING`, `SUBMITTED`, `INPUT_REQUIRED`, `AUTH_REQUIRED`
  - Terminal: `COMPLETED`, `FAILED`, `CANCELED`, `REJECTED`

## Configuration

```yaml
kind: a2a
endpoint: https://agent.example.com/
token: secret
timeoutMs: 5000
resultKeyFields: [invoiceId]
instructionTemplate: "Investigate overdue invoice {{after.invoiceId}}"
terminalUpdatePolicy: replace
returnImmediately: true
priorityQueueCapacity: 10000
recoveryPolicy: strict
```

- `endpoint` is required.
- `resultKeyFields` is required.
- `terminalUpdatePolicy` defaults to `replace`.
- `returnImmediately` defaults to `true`.
- `recoveryPolicy` supports `strict` and `auto_skip_gap`.

## Data mapping

Each `SendMessage` includes a data part:

- mediaType: `application/vnd.drasi.change+json`
- data:
  - `queryId`
  - `operation`
  - `sequence`
  - `resultKey`
  - `before`/`after`/`data` (as applicable)
  - `metadata` (when query metadata exists)

`instructionTemplate` is optional. If configured, it renders a text part from
`query_id`, `operation`, `sequence`, `result_key`, `before`, `after`, and `metadata`.

## Recovery and durability

- `is_durable()` returns `true`.
- `needs_snapshot_on_fresh_start()` returns `false`.
- `default_recovery_policy()` returns `Strict`.
- The reaction persists activation state in the configured state store and uses query checkpoints (`checkpoint:{queryId}`) through `CheckpointState`.
- There is no second outbox; replay comes from the query outbox.

## Delivery policy

- Retry then fail-stop: transport/connect failures, HTTP `5xx`, `408`, `409`, `425`, `429`, `401`, `403`, `407`.
- Drop and continue: most other `4xx`.
- JSON-RPC application errors fail-stop so the query outbox can replay.

## Idempotency note

`messageId` is deterministic: `{reactionId}-{queryId}-{resultKey}-{sequence}`.  
Endpoints that deduplicate by message id converge on replay.  
If a crash happens after `SendMessage` succeeds but before activation state persistence, a duplicate task can still occur.

## Integration tests

```bash
cargo test -p drasi-reaction-a2a --test integration_tests -- --nocapture
```

They verify ADD create, UPDATE follow-up, DELETE cancel, one-shot messages, terminal replace/ignore, replay, and activation survival across restart.
