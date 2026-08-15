# drasi-source-github

Authorized GitHub source plugin for Drasi.

This source accepts **signed GitHub webhooks**, durably admits each delivery into the source WAL, then performs **authoritative GitHub GraphQL fetches** to hydrate and emit normalized graph changes.

## v1 Behavior

- PAT is configured through descriptor DTO as `SecretReference` (resolved before runtime).
- Webhooks require `X-Hub-Signature-256` HMAC-SHA256 and are validated in constant time.
- Delivery admission path is durable-first:
  1. validate signature + headers
  2. parse locator
  3. append admission record to WAL
  4. persist delivery GUID dedupe marker
  5. return `2xx`
- WAL full with reject policy returns `503`.
- A single sequential hydrator processes admitted deliveries FIFO.
- Transient failures are retained and retried with backoff and surfaced via `/health`.
- Body is treated as locator only; data changes come from GraphQL fetches.
- Reconcile loop paginates and converges state (including missed deletes).

## Delivery Semantics

- Runtime delivery is **at-least-once** from admitted webhook to emitted `SourceChange`.
- Crash window: if the process crashes after dispatch but before the root committed-marker snapshot write, the admitted WAL record can replay and emit duplicate `SourceChange` events on recovery.
- Consumers/queries must assume **idempotent/convergent processing** (duplicates are valid).
- Delivery GUID markers are retained while their admission remains replayable in the WAL, then
  compacted. A very old GitHub retry may be admitted again after both its WAL record and marker
  have been pruned; this is part of the at-least-once convergence contract.
- Receive order is FIFO by this source’s admitted WAL/hydrator sequencing, **not GitHub global causal order**.
- The webhook inbox/admission seam and the WAL/hydrator seam are intentionally durability-first, not exactly-once.
- Reconciliation stores generation-stamped absence observations with the WAL head they cover.
  A stale admission covered by such an observation is a terminal no-op. An unseen non-delete
  object returning `node: null` is retried three times for API lag, then durably classified
  `gone-before-hydration` so it cannot poison FIFO indefinitely.

## Graph Schema

- Node labels: `GitHubRepository`, `GitHubIssue`, `GitHubPullRequest`, `GitHubIssueComment`, `GitHubPullRequestReview`, `GitHubPullRequestReviewComment`, `GitHubProject`, `GitHubProjectItem`
- Relation labels: `IN_REPOSITORY`, `COMMENT_ON`, `REVIEW_OF`, `PART_OF_REVIEW`, `IN_PROJECT`, `TRACKS`

Full node/relation properties and IDs: [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md).

## Configuration

Descriptor schema name: `source.github.GitHubSourceConfig` (version `1.0.0`).

| Field | Type | Required | Notes |
|---|---|---|---|
| `token` | `SecretReference` | yes | GitHub PAT (descriptor-enforced secret reference) |
| `repositories` | `string[]` | no | canonical `owner/repo` |
| `projects` | `{ owner, number }[]` | no | project selectors |
| `webhook.host` | `string` | no | default `0.0.0.0` |
| `webhook.port` | `u16` | no | default `8080` |
| `webhook.path` | `string` | no | default `/webhook` |
| `webhook.secret` | `SecretReference` | yes | descriptor-enforced secret reference |
| `webhook.bodyLimitBytes` | `usize` | no | default `10485760` |
| `reconcileIntervalSecs` | `u64` | no | default `300` |
| `durability` | object | yes | must have `enabled=true` |
| `graphqlUrl` | `string` | no | default `https://api.github.com/graphql` |
| `skipInitialBootstrap` | `bool` | no | default `false` |

At least one of `repositories` or `projects` must be set.

## Bootstrap Provider

The source ships with an internal bootstrap provider that fetches a full reconcile snapshot and emits initial inserts/updates for subscribed labels.

See [BOOTSTRAP.md](BOOTSTRAP.md).

## Integration Test

`tests/integration_test.rs` uses a protocol/local harness:

- local Axum mock GraphQL server
- real webhook HTTP POSTs with HMAC signatures
- DrasiLib + ApplicationReaction subscriptions
- asserted create/update/delete flow plus project-item update/scope resolution, replay convergence, dedupe, bad signature, and WAL-full `503`

Run:

```bash
cargo test -p drasi-source-github
cargo test -p drasi-source-github --test integration_test -- --nocapture
```

## Troubleshooting

- `401` from webhook: verify `X-Hub-Signature-256` and shared secret.
- `503` from webhook: WAL capacity reached; inspect hydrator health and head poison delivery.
- No changes after webhook: verify GraphQL fetch for locator ID succeeds and `/health` is not degraded.
- Startup failure “requires durable state store”: configure `DrasiLib` with a state store provider and WAL provider.
