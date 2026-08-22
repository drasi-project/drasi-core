# GitHub WorkGraph Dispatcher Reaction

`drasi-reaction-github-workgraph-dispatcher` turns a WorkGraph capacity query
into durable `WorkGraphTaskLease/v1` comments. It is the only component that
grants worker slots.

The Reaction subscribes to exactly one query. For each current worker row it
pairs `freeSlotIds` and `dispatchableTasks` in their supplied order, reserves
both identities durably, then writes the canonical Lease comment. A reservation
continues to mask its task and slot until a later row includes the exact
`leaseId` in `activeLeaseIds`.

## Configuration

```yaml
kind: github-workgraph-dispatcher
queries:
  workqueue-capacity: {}
config:
  apiUrl: https://api.github.com
  token:
    kind: Secret
    name: github-token
  userAgent: drasi-github-workgraph-dispatcher
  apiVersion: "2022-11-28"
  requestTimeoutMs: 30000
  maxAttempts: 4
  initialRetryDelayMs: 500
  priorityQueueCapacity: 10000
```

`token` must be a named `Secret`; a static token is rejected on the dynamic
plugin path. Optional `headers` values also accept `ConfigValue` references.
Dispatcher-owned authentication, content negotiation, API-version, and
connection headers cannot be overridden.

`apiUrl` may point at GitHub Enterprise Server and can include its REST prefix,
for example `https://github.example/api/v3`.

## Capacity-row contract

Every added, updated, aggregation, and deleted row is parsed with
`deny_unknown_fields`. Numbers that are mathematically integral are accepted
whether the query engine encodes them as `2` or `2.0`.

```json
{
  "repositoryOwner": "drasi-project",
  "repositoryName": "drasi-workgraph-demo",
  "workerId": "validator-1",
  "agentProfile": "issue-validator",
  "leaseDurationSeconds": 900,
  "configuredSlotCount": 2,
  "activeLeaseCount": 0,
  "activeLeaseIds": [],
  "freeSlotIds": ["validator-1/1", "validator-1/2"],
  "dispatchableTaskIds": ["I_task_41"],
  "dispatchableTasks": [
    {
      "taskNodeId": "I_task_41",
      "taskNumber": 41,
      "repositoryOwner": "drasi-project",
      "repositoryName": "drasi-workgraph-demo",
      "assignmentCommentNodeId": "IC_assignment_41",
      "workerId": "validator-1",
      "taskType": "validate-issue",
      "queuePriority": 10,
      "assignmentCreatedAt": "2026-08-19T22:00:01Z"
    }
  ]
}
```

The query is responsible for these semantics:

- one row per configured worker;
- `activeLeaseCount` is the number of distinct active Leases and equals
  `activeLeaseIds.length`;
- `freeSlotIds` contains only enabled, non-retiring slots with no active Lease;
- `dispatchableTaskIds` and `dispatchableTasks` are aligned in the same stable
  order;
- task order is queue priority, Assignment creation time, then task node ID;
- each complete task record contains all immutable operands required to write
  the Lease without a second lookup.

A capacity query should project the Source properties directly:

```cypher
RETURN
  repository.ownerLogin AS repositoryOwner,
  repository.name AS repositoryName,
  worker.workerId AS workerId,
  worker.agentProfile AS agentProfile,
  worker.leaseDurationSeconds AS leaseDurationSeconds,
  worker.configuredSlotCount AS configuredSlotCount,
  activeLeaseCount,
  activeLeaseIds,
  freeSlotIds,
  dispatchableTaskIds,
  dispatchableTasks
```

Compute the active-Lease collection separately from the dispatchable-task
collection before producing this projection. Joining both collections in one
aggregate path can cross-multiply them and overstate capacity.

Deleted rows never create Leases. They may only confirm a reservation when they
still carry its exact `activeLeaseIds` value.

## Durable ledger and recovery

The Reaction returns `is_durable() = true`; Drasi rejects startup without a
durable `StateStoreProvider`. One durable JSON record represents each logical
reservation:

```text
reserved
  -> write_in_flight
  -> awaiting_projection
  -> confirmed -> deleted
```

An invariant violation moves the record to `reconcile_required` and stops the
Reaction. Records include the generated Lease ID, task and slot ownership,
repository, exact canonical body and digest, attempt count, last error, and
originating query sequence and row signature.

`start()` completes ledger loading and GitHub reconciliation before it starts
the result loop. Every replay or live `QueryResult` is also written to a durable
inbox before `enqueue_query_result` acknowledges it, then removed only after the
row effects and existing query checkpoint are durable. Startup drains that inbox
before Drasi opens the query replay/live-event gate. A crash in every
pre-confirmation or enqueue/processing phase therefore retains the claim or
input needed to resume it. An inbox write error fail-stops ingestion until
restart so the host cannot advance past a missing sequence.

On the first start, the Reaction requests and synchronously processes the
query's keyed current snapshot before the host establishes its checkpoint. A
durable query-wide snapshot watermark suppresses buffered rows already covered
by that snapshot, including rows no longer present in it. Ordinary restarts
recover the ledger and inbox, then use strict outbox catch-up; they do not
depend on the bootstrap hook.

Durable state is bound to the configured query ID and normalized `apiUrl`.
Changing either while reusing the same Reaction ID fails startup before any
recovery request can be sent to the new target. Deprovision the old Reaction
state before intentionally moving that ID.

The state-store interface does not provide cross-process compare-and-swap. Run
**exactly one active dispatcher instance** for a WorkGraph. Multiple active
replicas are unsupported until the deployment supplies explicit leader
election or a shared atomic reservation primitive.

## Ambiguous GitHub writes

The dispatcher never blindly retries a timed-out or retryable-server-error
POST. It first pages through the task's authoritative GitHub comments:

- one exact `leaseId` and byte-identical body moves to
  `awaiting_projection`;
- absence on two authoritative reads separated by the configured backoff
  retries the same Lease ID and body within `maxAttempts`;
- duplicate or conflicting comments move to `reconcile_required`;
- a definitive 4xx response also fail-stops with the reservation retained.

GitHub 429, 5xx, and documented rate-limited 403 responses use the same
reconciliation path. `Retry-After` or `X-RateLimit-Reset` extends the configured
backoff; an ordinary permission-denied 403 remains definitive.

Only a later non-stale query row containing that exact Lease ID confirms the
write. A task or slot disappearing without the expected ID is an error, not
implicit success.

## Embedded construction

```rust,ignore
use drasi_reaction_github_workgraph_dispatcher::GitHubWorkGraphDispatcher;

let reaction = GitHubWorkGraphDispatcher::builder("dispatch-workgraph-workqueue")
    .with_query("workqueue-capacity")
    .with_token(github_token)
    .build()?;
```

The host must also configure a durable state store such as
`drasi-state-store-redb`.
