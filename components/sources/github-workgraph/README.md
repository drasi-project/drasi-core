# GitHub WorkGraph Source

Streams one organization's GitHub webhook into Drasi. The Source is the
single-instance authority for agent queues and active task allocations.

The one exception is the agent-capacity configuration file. A `push` payload
carries only changed paths, never file content, so when `agentConfig` is set
the Source reads that one blob back over the same GitHub GraphQL endpoint and
credential mechanism the bootstrapper uses. No other GitHub read, no side
service, and no second transport is introduced. All other initial API reads
still belong to the separate `github-workgraph` bootstrap provider.

## Configuration

```yaml
id: github-workgraph
kind: github-workgraph
organization: drasi-project
taskIssueType:
  id: IT_CONFIGURED_GRAPHQL_NODE_ID
  name: WorkGraphTask
repositories:
  - drasi-workgraph-demo
agentConfig:
  repository: drasi-project/drasi-workgraph-demo
  ref: main
  path: .github/workgraph/agents.yaml
  token:
    kind: Secret
    name: github-workgraph-agent-config-token
  apiBaseUrl: https://api.github.com/graphql
protocolTrust:
  assigners:
    - id: U_kgDOAbcDisp
      login: drasi-workqueue-dispatcher
  reporters:
    - id: U_kgDOAbcRept
      login: drasi-result-reporter
webhook:
  host: 0.0.0.0
  port: 8080
  path: /webhook
  secret:
    kind: Secret
    name: github-workgraph-webhook-secret
  leaseValidationToken:
    kind: Secret
    name: github-workgraph-lease-validation-token
  bodyLimitBytes: 26214400
durability:
  enabled: true
  maxEvents: 10000
  capacityPolicy: RejectIncoming
```

`taskIssueType.id` is the exact GitHub GraphQL Issue Type node ID and
`taskIssueType.name` is its exact, case-sensitive name. Both are required and
deployment-configurable; Core has no built-in Issue Type ID or name.

`organization` is one login. `repositories` is optional; empty means all
repositories in the organization. Entries may be bare names or matching
`owner/name` values and are normalized to sorted lowercase names. The webhook
secret and lease validation token must be separate `SecretReference` values,
the path must be static, and durability must use `RejectIncoming`.

`agentConfig` is optional. Omitting it turns agent allocation off entirely: no
`WorkGraphAgent` or `WorkGraphAgentSlot` node is projected and `push`
deliveries are ignored. When it *is* present it is strictly required — see
[Agent capacity configuration](#agent-capacity-configuration). `repository` must be
one `owner/name` pair, `ref` an exact git ref, and `path` a normalized
repository-relative path (no leading `/`, no `.`/`..`/empty segments, no
whitespace). `token` must be a `SecretReference` and needs only `Contents: Read`
on that repository. `apiBaseUrl` defaults to `https://api.github.com/graphql`
and is overridden for GitHub Enterprise Server.

Configure one organization webhook for `repository`, `issues`, `sub_issues`,
`issue_comment`, `pull_request`, and `pull_request_review`. Add `push` only when
`agentConfig` is configured. Other event families are ignored.

## Graph contract

All IDs are GitHub GraphQL node IDs from webhook payload `node_id` fields.
Properties are camelCase.

Every `GitHubIssue` retains its complete `labels` and `labelDetails` properties
and additionally exposes ordered `statusLabels` and `workgraphLabels` arrays.
These contain the original label names beginning with the exact, case-sensitive
`status:` and `workgraph:` prefixes respectively, and are present as empty
arrays when there are no matches. Existing single-value `status` and
`statusLabel` properties remain available. `currentStatus` is always a string:
`none` for zero status labels, the exact label for one, and `error` for more
than one. `workgraphInclude` is always a boolean and is false exactly when
`workgraphLabels` contains `workgraph:ignore` or `workgraph:error`.

Issue-derived `state` and string `stateReason` values are normalized to
lowercase on `GitHubIssue` and `WorkGraphTask` nodes. A null or absent
`stateReason` remains null or absent. The boolean `isOpen` is true exactly when
normalized `state` is `open`; unknown and non-open states produce false.

| Node | ID | Notes |
|---|---|---|
| `GitHubOrganization` | organization node ID | organization metadata |
| `GitHubRepository` | repository node ID | repository metadata |
| `GitHubIssue` | Issue node ID | open, non-task Issue |
| `GitHubPullRequest` | PR node ID | open PR |
| `GitHubIssueComment` / `GitHubPullRequestComment` | comment node ID | ordinary conversation comment |
| `GitHubPullRequestReview` | review node ID | PR review |
| `WorkGraphTask` | typed child Issue node ID | native Issue metadata plus strict work definition |
| `WorkGraphTaskAssignment` | assignment comment node ID | selected custom-agent profile name in `agentId` |
| `WorkGraphTaskResult` | result comment node ID | strict Result fields and comment provenance |
| `WorkGraphTaskFeedback` | feedback comment node ID | exact feedback bound to one Result revision |
| `WorkGraphTaskResultAcceptance` | acceptance comment node ID | accepted Result identity and body revision |
| `WorkGraphTaskLease` | `workgraph-lease:{task}:{leaseId}` | synthetic active allocation owned by the Source |
| `WorkGraphAgent` | `workgraph-agent:{agentId}` | one configured custom agent and its capacity |
| `WorkGraphAgentSlot` | `workgraph-agent-slot:{slotId}` | one unit of that agent's concurrency |
| `WorkGraphError` | deterministic error ID | malformed task/comment, status conflict, or agent config |

A typed child is emitted only as `WorkGraphTask`; it is never also emitted as
`GitHubIssue`.

| Relation | Direction | Stable ID |
|---|---|---|
| `IN_ORGANIZATION` | repository → organization | `IN_ORGANIZATION:{repository}:{organization}` |
| `IN_REPOSITORY` | Issue/PR/task → repository | `IN_REPOSITORY:{item}:{repository}` |
| `TASK_FOR` | task → parent Issue | `TASK_FOR:{task.databaseId}` |
| `COMMENT_ON` | ordinary/specialized comment → Issue/PR/task | `COMMENT_ON:{comment}:{parent}` |
| `ASSIGNMENT_FOR` | assignment → task | `ASSIGNMENT_FOR:{assignment}:{task}` |
| `ASSIGNED_TO` | assignment → agent | `ASSIGNED_TO:{assignment}:{agent}` |
| `RESULT_FOR` | result → task | `RESULT_FOR:{comment}:{task}` |
| `FEEDBACK_FOR` | feedback → result | `FEEDBACK_FOR:{feedback}:{result}` |
| `ACCEPTS_RESULT` | acceptance → result | `ACCEPTS_RESULT:{acceptance}:{result}` |
| `HAS_SLOT` | agent → slot | `HAS_SLOT:{agent}:{slot}` |
| `LEASE_FOR` | lease → task | `LEASE_FOR:{lease}:{task}` |
| `LEASES_SLOT` | lease → slot | `LEASES_SLOT:{lease}:{slot}` |
| `REVIEW_OF` | review → PR | `REVIEW_OF:{review}:{pr}` |
| `ERROR_ON` | comment/status error → subject | deterministic from source and subject IDs |

The parent edge has one identity per task keyed by GitHub's numeric child Issue
database ID. Its endpoints remain the child and parent GraphQL node IDs. This
keeps Cypher topology unchanged while allowing an asymmetric
`sub_issue_removed` payload to tombstone the relation from `sub_issue_id`
without a child object or cache. GitHub permits both `sub_issue` and
`sub_issue_id` to be absent; that schema-valid variant is acknowledged as a
no-op because no edge identity is derivable payload-only. Reparenting updates
the same identity to the new parent endpoint.

## Task and specialized comment wire formats

A configured typed Issue body has an exact `WorkGraphTask/v1` marker and one
fenced YAML document:

````text
WorkGraphTask/v1

```yaml
taskType: validate-issue
inputs:
  validationProfile: new-issue-default
```
````

The other supported definition is `request-info`, whose only input is a
non-empty `validationResultCommentNodeId`. Unknown fields/types, assignment
fields, malformed or multi-document YAML, prose, and envelope deviations are
rejected. A malformed typed task emits `WorkGraphError`.

An Assignment comment uses `WorkGraphTaskAssignment/v1` with canonical
two-space JSON containing exactly `agentId`:

````text
WorkGraphTaskAssignment/v1

```json
{
  "agentId": "issue-validator"
}
```
````

`agentId` is the exact, case-sensitive GitHub custom-agent profile name. It is
1-64 ASCII letters, digits, `-`, `.`, or `_`; Core never lowercases it. The
comment emits `WorkGraphTaskAssignment`, `COMMENT_ON`, `ASSIGNMENT_FOR`, and
`ASSIGNED_TO`. A trusted Assignment naming an unknown agent remains visible
with that intent relation but has `trusted = false` and never enters allocator
state.

A task Result comment has exactly this grammar:

````text
WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
  "leaseId": "EXACT_ACTIVE_LEASE_ID",
  "outcome": "succeeded",
  "summary": "Validated the issue.",
  "result": {
    "criteria": [
      {
        "criterion": "Acceptance criteria",
        "passed": true,
        "evidence": "Present."
      }
    ]
  }
}
```
````

The marker is the first byte, separators are LF, JSON is strict canonical
two-space typed JSON, and the closing fence has exactly one final LF. No details
wrapper, prose, or extra bytes are allowed. A marked malformed Result on a typed
task emits `WorkGraphError`. The same marker on an ordinary Issue is an ordinary
`GitHubIssueComment`. Unmarked task comments are also ordinary comments.
Multiple valid Results are allowed.

A Result Acceptance comment uses `WorkGraphTaskResultAcceptance/v1` with
canonical two-space JSON containing exactly `resultCommentNodeId`,
`resultBodyDigest`, and non-empty `summary`. The digest is
`sha256:<64 lowercase hex>` over the exact accepted Result comment body. It
emits `WorkGraphTaskResultAcceptance`, `COMMENT_ON`, and `ACCEPTS_RESULT`; the
last edge targets the Result comment node ID. Result nodes expose their own
`bodyDigest` so a query can reject stale acceptances.

### Feedback

`WorkGraphTaskFeedback/v1` contains exactly `resultCommentNodeId`,
`resultBodyDigest`, and non-empty `feedback`. The digest is
`sha256:<64 lowercase hex>` over the exact current Result comment body. A trusted
Feedback makes that Result's Assignment queue-eligible again unless an exact
Acceptance exists.

All specialized protocols are v1-only. Unknown versions, fields, or noncanonical
JSON emit `WorkGraphError`; they are never interpreted through a compatibility
schema.

## Agent capacity configuration

The configured agent set lives in one strict repository file, normally
`.github/workgraph/agents.yaml`:

```yaml
version: 1
agents:
  - agentId: issue-validator
    slots: 2
    leaseDuration: PT15M
```

`version` must be `1` and `agents` must contain 1-64 entries. Each agent has a
unique `agentId` using the Assignment grammar, 1-16 slots, and a whole-unit
ISO-8601 duration from one through 86,400 seconds. The file must be LF UTF-8 and
at most 256 KiB. Legacy fields and unknown fields are rejected. The Source
reads the file at startup and on matching `push` deliveries. A deterministic
invalid file projects `workgraph-error:agent-config` without changing the last
accepted pool; an unreadable file fails startup or returns `503`.

Every configured slot has the stable ID `{agentId}/{slotNumber}`. Capacity
reductions retain occupied excess slots as disabled and retiring until their
Lease ends. Unoccupied excess slots are deleted immediately. Removing an agent
uses the same rule, and growing capacity reuses the same slot identities.

## Source-owned allocation

A trusted Assignment/v1 is the durable queue entry for its named configured
agent. Queue admission is independent of available capacity and task type:
any configured agent may receive any `taskType`.
Assignments are ordered by GitHub `createdAt`, then task node ID. Free slots are
ordered by numeric slot number, then slot ID. The Source enforces one active
Lease per task and per slot and fills all available capacity deterministically.

`WorkGraphTaskLease` is synthetic Source graph state, not a GitHub comment.
Existence means active. It carries exactly:

- `leaseId`, `taskNodeId`, and `assignmentCommentNodeId`
- `agentId`, `slotId`, and `taskType`
- `acquiredAt` and `expiresAt`

It has `LEASE_FOR` to the task and `LEASES_SLOT` to the slot. A trusted exact
Result/v1 releases it and any replacement allocation is emitted in the same
transition. A stale or mismatched Result remains visible with `trusted = false`
and cannot release capacity. Exact Feedback requeues the same Assignment;
Acceptance suppresses that requeue. Closing/deleting the task or deleting/editing
away the Assignment cancels queued and active work. The existing 500 ms Source
dispatch tick expires due Leases and refills capacity.

Agent nodes expose `queueDepth`, `activeLeaseCount`, and `availableSlotCount`
from the same allocator state that owns the queue and Leases.

### Protocol trust

```yaml
protocolTrust:
  assigners:
    - id: U_kgDOAbcAssign
      login: drasi-workgraph-assigner
  reporters:
    - id: U_kgDOAbcReport
      login: drasi-workgraph-reporter
```

Both lists are non-empty and match the exact GitHub node ID and login.
Assigners alone can queue Assignment/v1. Reporters alone can produce trusted
Result/v1, Feedback/v1, and Acceptance/v1. The preserved author and, on edits,
the editor must both hold the required role. Without `protocolTrust`, protocol
artifacts are visible but untrusted and cannot change allocation state.

### Durability and restart

The allocator persists one Source-local `AllocationState` containing the agent
snapshot, queue, active Leases, outcomes, deadlines, retirement state, and a
narrow pending synthetic-projection list. For each allocator transition it
persists state with pending changes, appends those stable idempotent changes to
the Source WAL in dependency-safe order, then clears pending. Restart replays a
pending crash prefix and re-states every active Lease with the same IDs. Corrupt
or unsupported allocator state fails closed.

The bootstrapper does not fold comments into allocation state and never creates
synthetic Leases. Clean activation requires external preflight to find no open
Assignment/v1 artifacts; normal restart preserves Source and query state.

### Exact active-Lease validation

`POST {webhook.path}/lease/validate` accepts:

```json
{
  "taskNodeId": "I_task",
  "leaseId": "LEASE_ID",
  "assignmentCommentNodeId": "IC_assignment",
  "agentId": "issue-validator",
  "slotId": "issue-validator/1"
}
```

Authenticate with `Authorization: Bearer <webhook.leaseValidationToken>`.
The token is separate from the webhook HMAC secret. The endpoint returns the
exact active Lease snapshot on `200`, `409` for stale, mismatched, or locally
expired input, `401` for failed authentication, and `503` when allocator state
cannot be read. It is point-in-time and read-only.

## Live projection behavior

Generic Issues and PRs retain open-only behavior: opening/reopening uses
Update-on-missing idempotent materialization, closing deletes, and other
closed-item activity is ignored. Existing repository filtering and
close/delete tombstone exceptions remain unchanged.

Configured typed tasks are different:

- OPEN and CLOSED tasks are retained. Close updates `state`, `stateReason`, and
  `closedAt`; reopen updates the same task identity.
- `issues` events handle body, Issue Type, state, transfer, and repository
  transitions. A transition away from the configured exact type removes the
  task and allows an open generic Issue; a transition into the type removes the
  generic Issue and creates the task or task error. GitHub `typed` and `untyped`
  deliveries identify the assigned/removed type in their required top-level
  `type` object; they do not carry `changes.type.from`. Core combines that
  object with the current `issue.type` and requires both configured ID and name
  to match.
- `sub_issues` add/remove actions create or delete the native `TASK_FOR` edge.
  Either the `issues` or `sub_issues` delivery may arrive first; stable IDs and
  payload-complete updates converge without delivery-order state. GitHub's
  asymmetric payload guarantee is honored: `parent_issue_*` requires only
  `sub_issue`, while `sub_issue_*` requires only `parent_issue`; an omitted
  optional counterpart is accepted and any derivable node/relation change is
  still emitted. `TASK_FOR` needs only child and parent node IDs and is not
  gated by optional `sub_issue_repo`/`parent_issue_repo` objects. The top-level
  repository is reused only for the Issue it authoritatively describes, or when
  `repository_url`/embedded repository identity proves a same-repository
  fallback; cross-repository metadata is never invented.
- Task nodes, `TASK_FOR`, and task `IN_REPOSITORY` use Update-based idempotent
  upserts for open, typed, edit, close, reopen, transfer, and `sub_issues`
  observations. Drasi Update-on-missing produces the first Add, while repeats
  load prior state and do not create duplicate Add/removal deltas. A
  generic↔task type transition updates the shared Issue node ID in place and
  cleans only representation-specific relations/errors rather than deleting
  and reinserting the shared node.
- A malformed body replaces only the still-typed task node with
  `WorkGraphError`; its native `TASK_FOR` relation is retained. Repairing the
  body rematerializes the same task node and immediately reconnects the
  existing parent path. Only untyping, task deletion/transfer, or an explicit
  native sub-issue removal tombstones `TASK_FOR`. A `sub_issues` add/reparent
  updates the native relation before Assignment parsing, so even a malformed
  child tracks its latest parent and a later parent-less `issues.edited` repair
  reconnects only that edge.
- Untyping explicitly writes null for every task-only property before the
  shared node becomes `GitHubIssue`, because Drasi Update merges omitted
  properties.
- Task delete removes the task, repository edge, parent edge, and task error.
- Task comments continue to be processed while the task is closed. Result and
  ordinary comment create/edit/delete transitions are deterministic and use
  only current and `changes.body.from` payload content.

- Assignment comments follow the same create/edit/delete rules as every other
  specialized comment. Editing or deleting an Assignment retracts its prior
  intent and cancels queued or active work. Recreating or revising it advances a
  persisted attempt so it cannot reuse a prior Lease ID. Leases are synthetic
  Source state; there is no Lease comment protocol.

Ingress verifies `X-Hub-Signature-256`, converts the payload, appends each
change to the WAL, persists the delivery dedupe marker, and then acknowledges.
A `push` delivery follows the same order: it converges the agent graph, appends
the resulting changes, and only then records the dedupe marker, so a redelivered
push is absorbed. Stable IDs make redelivery idempotent at the graph identity
level, but consumers must still tolerate at-least-once change delivery.
