# GitHub WorkGraph Source

Streams one organization's GitHub webhook into Drasi. Conversion is read-only,
stateless, cache-free, and payload-only. The Source never calls GitHub; the
separate `github-workgraph` bootstrap provider owns initial API reads.

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
webhook:
  host: 0.0.0.0
  port: 8080
  path: /webhook
  secret:
    kind: Secret
    name: github-workgraph-webhook-secret
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
secret must be a `SecretReference`, the path must be static, and durability must
use `RejectIncoming`.

Configure one organization webhook for `repository`, `issues`, `sub_issues`,
`issue_comment`, `pull_request`, and `pull_request_review`. Other event families
are ignored.

## Graph contract

All IDs are GitHub GraphQL node IDs from webhook payload `node_id` fields.
Properties are camelCase.

| Node | ID | Notes |
|---|---|---|
| `GitHubOrganization` | organization node ID | organization metadata |
| `GitHubRepository` | repository node ID | repository metadata |
| `GitHubIssue` | Issue node ID | open, non-task Issue |
| `GitHubPullRequest` | PR node ID | open PR |
| `GitHubIssueComment` / `GitHubPullRequestComment` | comment node ID | ordinary conversation comment |
| `GitHubPullRequestReview` | review node ID | PR review |
| `WorkGraphTask` | typed child Issue node ID | native Issue metadata plus strict Assignment fields |
| `WorkGraphTaskResult` | result comment node ID | strict Result fields and comment provenance |
| `WorkGraphError` | deterministic error ID | malformed task, malformed marked Result, or status conflict |

A typed child is emitted only as `WorkGraphTask`; it is never also emitted as
`GitHubIssue`.

| Relation | Direction | Stable ID |
|---|---|---|
| `IN_ORGANIZATION` | repository → organization | `IN_ORGANIZATION:{repository}:{organization}` |
| `IN_REPOSITORY` | Issue/PR/task → repository | `IN_REPOSITORY:{item}:{repository}` |
| `TASK_FOR` | task → parent Issue | `TASK_FOR:{task.databaseId}` |
| `COMMENT_ON` | ordinary comment/result → Issue/PR/task | `COMMENT_ON:{comment}:{parent}` |
| `RESULT_FOR` | result → task | `RESULT_FOR:{comment}:{task}` |
| `REVIEW_OF` | review → PR | `REVIEW_OF:{review}:{pr}` |
| `ERROR_ON` | comment/status error → subject | deterministic from source and subject IDs |

The parent edge has one identity per task keyed by GitHub's numeric child Issue
database ID. Its endpoints remain the child and parent GraphQL node IDs. This
keeps Cypher topology unchanged while allowing an asymmetric
`sub_issue_removed` payload to tombstone the relation from required
`sub_issue_id` without a child object or cache. Reparenting updates that same
identity to the new parent endpoint.

## Task and Result wire formats

A configured typed Issue body is exactly the existing strict
WorkGraphAssignment JSON object serialized with two-space indentation. There is
no marker, Markdown fence, details element, summary, surrounding prose, or final
newline:

```json
{
  "assignmentId": "fixture-701-validation",
  "agentProfile": "issue-validator",
  "priority": 10,
  "taskType": "issue-validation",
  "task": {
    "validationProfile": "new-issue-default"
  }
}
```

`taskType` is `issue-validation` or `issue-risk-profile`. Validation tasks
require `task.validationProfile`. Risk tasks require `task.riskProfile` and a
non-empty `task.dimensions` string list. Unknown fields, compact/reordered JSON,
invalid values, whitespace outside the object, and all legacy comment envelopes
are rejected. A malformed typed task emits `WorkGraphError`.

A task Result comment has exactly this grammar:

````text
WorkGraphTaskResult/v1

```json
{
  "assignmentId": "fixture-701-validation",
  "taskType": "issue-validation",
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
  native sub-issue removal tombstones `TASK_FOR`.
- Untyping explicitly writes null for every task-only property before the
  shared node becomes `GitHubIssue`, because Drasi Update merges omitted
  properties.
- Task delete removes the task, repository edge, parent edge, and task error.
- Task comments continue to be processed while the task is closed. Result and
  ordinary comment create/edit/delete transitions are deterministic and use
  only current and `changes.body.from` payload content.

Ingress verifies `X-Hub-Signature-256`, converts the payload, appends each
change to the WAL, persists the delivery dedupe marker, and then acknowledges.
Stable IDs make redelivery idempotent at the graph identity level, but consumers
must still tolerate at-least-once change delivery.
