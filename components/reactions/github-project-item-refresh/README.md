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
- Durably reserves each `(deliveryId, projectItemNodeId)` key.
- Detects stale/out-of-order refreshes using durable per-item version state.
- Publishes only after successful GraphQL hydration and destination ACK.
- Uses strict fail-stop recovery by default (checkpoint does not advance on
  ambiguous/failed delivery).

## Configuration

```yaml
reactions:
  - id: gh-project-item-refresh
    kind: github-project-item-refresh
    queries:
      - project-item-invalidations
    config:
      githubToken:
        env: GITHUB_TOKEN
      graphqlUrl: https://api.github.com/graphql
      graphqlHeaders:
        X-GitHub-Api-Version:
          value: "2022-11-28"
      allowlistedProjectIds:
        - PVT_kwDOABC123
      destinationEventUrl: http://localhost:8080/changes
      destinationBearerSecret:
        env: PROJECT_STATUS_SOURCE_BEARER
      requestTimeoutMs: 10000
      deliveryRecordTtlSecs: 604800
      recoveryPolicy: strict
```

### Config Fields

| Field | Required | Description |
|---|---|---|
| `githubToken` | Yes | GitHub token (prefer env reference) |
| `graphqlUrl` | No | GitHub GraphQL endpoint (default `https://api.github.com/graphql`) |
| `graphqlHeaders` | No | Extra GraphQL request headers |
| `allowlistedProjectIds` | No | Allowed project node IDs (`PVT_*`); empty means allow all |
| `destinationEventUrl` | Yes | Standard-mode HTTP source event endpoint |
| `destinationBearerSecret` | No | Optional bearer secret for destination source |
| `requestTimeoutMs` | No | Shared request timeout for GraphQL and destination HTTP calls |
| `deliveryRecordTtlSecs` | No | TTL for terminal per-delivery records (default 7 days) |
| `recoveryPolicy` | No | `strict` (default) or `auto_skip_gap` |

## Input Contract (query row)

The query row (`ResultDiff::Add.data`) must include:

- `invalidationNodeId`
- `deliveryId` (e.g., `X-GitHub-Delivery`)
- `projectItemNodeId` (`PVTI_*`)
- `projectNodeId` (`PVT_*`, optional but recommended)
- `webhookAction` (optional)
- `webhookUpdatedAt` or `webhookUpdateTime` (optional RFC3339 timestamp)

## Output Contract (destination HTTP source event)

The reaction posts a standard `HttpSourceChange` **update** payload:

```json
{
  "operation": "update",
  "element": {
    "type": "node",
    "id": "ProjectItemStatus:PVTI_xxx",
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

`ProjectItemStatus:{projectItemNodeId}`

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
- deterministic destination node ID (`ProjectItemStatus:{projectItemNodeId}`)
- stale/version guard on `updatedAt`

## Security Notes

- GitHub token and destination bearer secret are never logged.
- Config `Debug` output redacts secrets.
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

- Point `destinationEventUrl` to the standard-mode HTTP source endpoint used by
  the ProjectItemStatus stream.
- Configure `allowlistedProjectIds` for each dogfood project board.
- Keep `recoveryPolicy: strict` to avoid silent drops on ambiguous publication.
