# GitHub Project Item Refresh Reaction

`drasi-reaction-github-project-item-refresh` hydrates authoritative GitHub
ProjectV2 item status and republishes normalized state to a standard-mode Drasi
HTTP source.

This reaction is intended for webhook-invalidated pipelines (WorkGraph Phase 2):

1. Dogfood webhook ingestion maps `projects_v2_item.edited` deliveries to
   `ProjectItemInvalidation` rows.
2. This reaction consumes those invalidations (ADD diffs only).
3. It fetches the current status from GitHub GraphQL.
4. It publishes a deterministic `ProjectItemStatus` node update to a separate
   HTTP source endpoint.

## Behavior Summary

- Processes **only `ResultDiff::Add`** rows.
- Ignores `Update`, `Delete`, aggregation, and noop diffs.
- Validates node IDs and project allowlist rules.
- Enforces a configured authoritative Status field node ID (`expectedStatusFieldNodeId`) before and after GraphQL hydration.
- Durably reserves each `(deliveryId, projectItemNodeId)` key.
- Detects stale/out-of-order refreshes using durable per-item version state.
- Publishes only after successful GraphQL hydration and destination ACK.
- Uses strict fail-stop recovery by default (checkpoint does not advance on
  ambiguous/failed delivery).

## Configuration

```yaml
stateStore:
  kind: redb
  path: "${STATE_STORE_PATH:-./data/github-project-refresh.redb}"

reactions:
  - id: refresh-github-project-items
    kind: github-project-item-refresh
    queries:
      - refresh-github-project-item
    autoStart: false
    githubToken: "${GITHUB_TOKEN}"
    graphqlUrl: https://api.github.com/graphql
    graphqlHeaders:
      X-GitHub-Api-Version: "2022-11-28"
    allowlistedProjectIds:
      - PVT_kwDOABC123
    statusFieldName: Status
    expectedStatusFieldNodeId: PVTSSF_lADOCX0YF84BgNE3zhaadbw
    destinationEventUrl: http://127.0.0.1:9001/sources/github-project-state/events
    destinationBearerSecret: "${PROJECT_STATUS_SOURCE_BEARER}"
    requestTimeoutMs: 10000
    deliveryRecordTtlSecs: 604800
    priorityQueueCapacity: 10000
    recoveryPolicy: strict
```

Dynamic reaction configuration fields are flat beside the base server fields
`id`, `kind`, `queries`, and `autoStart`; do not nest them under `config`.
The top-level durable `stateStore` is required because the reaction refuses to
start with an in-memory state store.

`ConfigValue` fields accept a direct static value, a POSIX environment
reference (`"${VAR}"` or `"${VAR:-default}"`), or a structured reference:

```yaml
githubToken:
  kind: EnvironmentVariable
  name: GITHUB_TOKEN
# Or, with a configured secret store:
destinationBearerSecret:
  kind: Secret
  name: project-status-source-bearer
```

### Config Fields

| Field | Required | Description |
|---|---|---|
| `githubToken` | Yes | GitHub token (prefer env reference) |
| `graphqlUrl` | No | GitHub GraphQL endpoint (default `https://api.github.com/graphql`) |
| `graphqlHeaders` | No | Extra GraphQL request headers |
| `allowlistedProjectIds` | No | Allowed project node IDs (`PVT_*`); empty means allow all |
| `statusFieldName` | No | Project field name read via `fieldValueByName` (default `Status`) |
| `expectedStatusFieldNodeId` | Yes | Expected authoritative GitHub ProjectV2 single-select status field node ID (`PVTSSF_*`); enforced as a security constraint |
| `destinationEventUrl` | Yes | Standard-mode HTTP source event endpoint |
| `destinationBearerSecret` | No | Optional bearer secret for destination source |
| `requestTimeoutMs` | No | Shared request timeout for GraphQL and destination HTTP calls |
| `deliveryRecordTtlSecs` | No | TTL for terminal per-delivery records (default 7 days) |
| `priorityQueueCapacity` | No | Inbound reaction queue capacity |
| `recoveryPolicy` | No | `strict` (default) or `auto_skip_gap` |

## Input Contract (query row)

The query row (`ResultDiff::Add.data`) should prefer the lower-camel fields
below. PascalCase dogfood fields and existing aliases remain supported.
Preferred contract field names are:
`invalidationNodeId`, `deliveryId`, `projectItemNodeId`, `projectNodeId`,
`statusFieldNodeId`, `invalidatedAt`, `stateSourceUrl`.

The canonical invalidation element/property identity is
`project-item-invalidation:{deliveryId}` and is accepted verbatim as
`invalidationNodeId`.

| Field | Preferred | Backward-compatible aliases | Required |
|---|---|---|---|
| Invalidation node ID | `invalidationNodeId` | `InvalidationNodeId`, `invalidation_node_id`, `id` | Yes |
| Delivery ID | `deliveryId` | `DeliveryId`, `xGitHubDelivery`, `xGithubDelivery`, `githubDeliveryId` | Yes |
| Project item node ID | `projectItemNodeId` | `ProjectItemNodeId`, `project_item_node_id` | Yes |
| Project node ID | `projectNodeId` | `ProjectNodeId`, `project_node_id` | No |
| Status field node ID | `statusFieldNodeId` | `StatusFieldNodeId` | No |
| Invalidation/webhook timestamp | `invalidatedAt` | `InvalidatedAt`, `webhookUpdatedAt`, `webhookUpdateTime`, `updatedAt`, `webhook_updated_at` | No |
| State source URL | `stateSourceUrl` | `StateSourceUrl` | No |
| Webhook action | `webhookAction` | `action` | No |

### Optional Input Validation

- The upstream query should filter invalidations to the configured changed
  Status field before invoking this reaction.
- `statusFieldNodeId` / `StatusFieldNodeId` (when present) must match
  configured `expectedStatusFieldNodeId`. A mismatch is persisted as a `failed`
  publication and is rejected before GraphQL/destination access.
- The authoritative GraphQL `status_field_node_id` must always match configured
  `expectedStatusFieldNodeId` (even if the row omits `statusFieldNodeId`). A
  mismatch is persisted as a `failed` publication and no destination publish
  occurs.
- For durable reservation consistency, any persisted row `StatusFieldNodeId`
  remains canonical on retries and is also validated against fetched GraphQL
  state.
- `StateSourceUrl` (when present) must match configured `destinationEventUrl`.
  The input URL is **never** used as a destination override.
  - Matching uses semantic URL checks when both values parse as URLs:
    `scheme`, `host`, `port`, `path` (with trailing slash normalization), and
    query string.
  - URL fragments are ignored for this comparison.
  - If parsing fails for either side, validation fails closed.
  - A mismatch is rejected before GraphQL hydration and before destination
    publish.

## Output Contract (destination HTTP source event)

The reaction posts a standard `HttpSourceChange` **update** payload:

```json
{
  "operation": "update",
  "element": {
    "type": "node",
    "id": "project-item-status:PVTI_xxx",
    "labels": ["ProjectItemStatus"],
    "properties": {
      "projectItemNodeId": "PVTI_xxx",
      "projectNodeId": "PVT_xxx",
      "statusFieldNodeId": "PVTSSF_xxx",
      "statusOptionId": "opt_xxx",
      "statusName": "In Progress",
      "updatedAt": "2026-08-13T20:00:00.000Z",
      "refreshedAt": "2026-08-13T20:00:05.123Z",
      "triggeringDeliveryId": "delivery-id"
    }
  }
}
```

Deterministic node ID format:

`project-item-status:{projectItemNodeId}`

## Durable State

Per-reaction durable state keys:

- `reservation:{deliveryId}::{projectItemNodeId}`
- `publication:{deliveryId}::{projectItemNodeId}`
- `version:{projectItemNodeId}`

Publication states:

- `reserved`
- `fetched`
- `published`
- `stale`
- `rejected`
- `failed`
- `ambiguous`

## Recovery & Idempotency

- `is_durable() == true`
- `needs_snapshot_on_fresh_start() == false`
- `default_recovery_policy() == strict`
- checkpoint advances only after destination ACK and durable publication state

Idempotency is achieved by:

- durable reservation key dedupe
- deterministic destination node ID (`project-item-status:{projectItemNodeId}`)
- deterministic source timestamp from the authoritative GitHub `updatedAt`
- an `Idempotency-Key` header derived from the delivery and project item IDs
- stale/version guard on `updatedAt`

Safe GraphQL reads retry automatically. Destination writes are attempted once per
processing attempt because a transport failure may occur after ingestion.
In-flight (`fetched`) and ambiguous writes retain the exact fetched state;
strict recovery replays that same deterministic update and requires a valid
HTTP Source acknowledgement before recording publication or advancing the
checkpoint.

Terminal publication/reservation pruning is internal and bounded: the reaction
attempts one prune pass on the first processed ADD after startup/restart, then
at most once every 5 minutes. If a prune pass fails, the failure is logged and
the next ADD remains eligible to retry pruning immediately (no public config
field controls this interval).

## Security Notes

- GitHub token, GraphQL header values, and destination bearer secret are never logged.
- Config `Debug` output exposes GraphQL header names but redacts all values.
- GraphQL and destination failures are logged without credential material.

## Testing

### Unit / focused tests

```bash
cargo test -p drasi-reaction-github-project-item-refresh
```

Covered scenarios include:

- success
- duplicate delivery
- GraphQL HTTP-200 `errors`
- null/missing status
- stale ordering
- destination failure + ambiguous transport
- retry + recovery replay
- allowlist rejection
- secret redaction

### Integration test (ignored)

```bash
cargo test -p drasi-reaction-github-project-item-refresh --test integration_test -- --ignored --nocapture
```

The integration test uses mock GraphQL and destination HTTP servers with a
durable test state store and verifies insert/update/delete behavior (only insert
triggers republish).

### Makefile targets

```bash
make -C components/reactions/github-project-item-refresh build
make -C components/reactions/github-project-item-refresh test
make -C components/reactions/github-project-item-refresh integration-test
make -C components/reactions/github-project-item-refresh lint
```

## Dogfood Integration Notes

- Point `destinationEventUrl` to the `github-project-state` standard-mode HTTP
  source endpoint. The Phase 2 dogfood loopback contract is
  `http://127.0.0.1:9001/sources/github-project-state/events`.
- Set `expectedStatusFieldNodeId` to the authoritative dogfood Status field:
  `PVTSSF_lADOCX0YF84BgNE3zhaadbw`.
- Configure `allowlistedProjectIds` for each dogfood project board.
- Keep `recoveryPolicy: strict` to avoid silent drops on ambiguous publication.
