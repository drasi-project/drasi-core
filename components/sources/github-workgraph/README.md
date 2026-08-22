# GitHub WorkGraph Source

Streams one organization's GitHub webhook into Drasi. Webhook conversion is
read-only, stateless, cache-free, and payload-only.

The one exception is the worker-queue configuration file. A `push` payload
carries only changed paths, never file content, so when `workerConfig` is set
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
workerConfig:
  repository: drasi-project/drasi-workgraph-demo
  ref: main
  path: .github/workgraph/workers.yaml
  token:
    kind: Secret
    name: github-workgraph-worker-config-token
  apiBaseUrl: https://api.github.com/graphql
leaseTrust:
  dispatchers:
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

`workerConfig` is optional. Omitting it turns the worker queue off entirely: no
`WorkGraphWorker` or `WorkGraphWorkerSlot` node is projected and `push`
deliveries are ignored. When it *is* present it is strictly required — see
[Worker queue configuration](#worker-queue-configuration). `repository` must be
one `owner/name` pair, `ref` an exact git ref, and `path` a normalized
repository-relative path (no leading `/`, no `.`/`..`/empty segments, no
whitespace). `token` must be a `SecretReference` and needs only `Contents: Read`
on that repository. `apiBaseUrl` defaults to `https://api.github.com/graphql`
and is overridden for GitHub Enterprise Server.

Configure one organization webhook for `repository`, `issues`, `sub_issues`,
`issue_comment`, `pull_request`, and `pull_request_review`. Add `push` only when
`workerConfig` is configured. Other event families are ignored.

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
| `WorkGraphTaskAssignment` | assignment comment node ID | selected supported agent profile |
| `WorkGraphTaskResult` | result comment node ID | strict Result fields and comment provenance |
| `WorkGraphTaskResultAcceptance` | acceptance comment node ID | accepted Result identity and body revision |
| `WorkGraphTaskLease` | lease comment node ID | one execution attempt holding one worker slot |
| `WorkGraphTaskLeaseAnchor` | `workgraph-lease:{task}:{leaseId}` | derived, task-scoped join point plus the derived lease lifecycle |
| `WorkGraphTaskLeaseExpiration` | expiration comment node ID | records that a lease reached its deadline |
| `WorkGraphWorker` | `workgraph-worker:{workerId}` | one configured worker and its capacity |
| `WorkGraphWorkerSlot` | `workgraph-worker-slot:{slotId}` | one unit of that worker's concurrency |
| `WorkGraphError` | deterministic error ID | malformed task/comment, status conflict, or worker config |

A typed child is emitted only as `WorkGraphTask`; it is never also emitted as
`GitHubIssue`.

The canonical `WorkGraphTaskLease/v1` DTO and body builder live in the shared
`drasi-github-workgraph` crate. The Source classifier and the stateful
`github-workgraph-dispatcher` Reaction consume that same wire type, with a
byte-exact classifier round-trip test preventing writer/parser drift.

| Relation | Direction | Stable ID |
|---|---|---|
| `IN_ORGANIZATION` | repository → organization | `IN_ORGANIZATION:{repository}:{organization}` |
| `IN_REPOSITORY` | Issue/PR/task → repository | `IN_REPOSITORY:{item}:{repository}` |
| `TASK_FOR` | task → parent Issue | `TASK_FOR:{task.databaseId}` |
| `COMMENT_ON` | ordinary/specialized comment → Issue/PR/task | `COMMENT_ON:{comment}:{parent}` |
| `ASSIGNMENT_FOR` | assignment → task | `ASSIGNMENT_FOR:{assignment}:{task}` |
| `RESULT_FOR` | result → task | `RESULT_FOR:{comment}:{task}` |
| `ACCEPTS_RESULT` | acceptance → result | `ACCEPTS_RESULT:{acceptance}:{result}` |
| `ASSIGNED_TO` | v2 assignment → worker | `ASSIGNED_TO:{assignment}:{worker}` |
| `HAS_SLOT` | worker → slot | `HAS_SLOT:{worker}:{slot}` |
| `LEASE_FOR` | lease → task | `LEASE_FOR:{lease}:{task}` |
| `LEASES_SLOT` | lease → slot | `LEASES_SLOT:{lease}:{slot}` |
| `LEASE_ANCHOR` | trusted lease → anchor | `LEASE_ANCHOR:{lease}:{anchor}` |
| `RESULT_FOR_LEASE` | trusted v2 result → anchor | `RESULT_FOR_LEASE:{result}:{anchor}` |
| `EXPIRES_LEASE` | trusted expiration → anchor | `EXPIRES_LEASE:{expiration}:{anchor}` |
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
two-space JSON containing only a non-empty supported `agentProfile`. It emits a
`WorkGraphTaskAssignment`, `COMMENT_ON`, and `ASSIGNMENT_FOR`.

A task Result comment has exactly this grammar:

````text
WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
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

### Worker queue comments

`WorkGraphTaskAssignment/v2` adds a worker queue to the historical v1 contract
and contains exactly `agentProfile` and `workerId`, in that order:

````text
WorkGraphTaskAssignment/v2

```json
{
  "agentProfile": "issue-validator",
  "workerId": "validator-1"
}
```
````

v1 remains readable for existing completed tasks. Every Assignment node exposes
an integer `version` and a `workerId` that is null for v1, so a queue query
selects v2 with `assignment.version = 2` rather than by absence. A v2 Assignment
additionally emits `ASSIGNED_TO`. The Source validates that `agentProfile` is
supported, but deliberately does **not** require the named worker to exist or to
be profile-compatible — that is a query-level assertion, and requiring it here
would make an Assignment silently disappear whenever a worker file is being
edited.

`WorkGraphTaskLease/v1` reserves one slot for one execution attempt:

````text
WorkGraphTaskLease/v1

```json
{
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "assignmentCommentNodeId": "IC_assignment",
  "workerId": "validator-1",
  "slotId": "validator-1/1",
  "acquiredAt": "2026-08-19T22:00:00Z",
  "expiresAt": "2026-08-19T22:15:00Z"
}
```
````

Every identifier must be a non-empty, bounded token with no whitespace or
control characters. Both instants must be the exact canonical UTC form
`YYYY-MM-DDTHH:MM:SSZ` — a space separator, a lowercase `t`/`z`, a `+00:00`
offset, and a fractional part are all rejected so two spellings of one instant
can never compare unequal in a query. `acquiredAt` must be strictly earlier than
`expiresAt`. Like every specialized comment, a Lease is only recognized on a
configured typed task.

`WorkGraphTaskResult/v2` adds `leaseId` immediately after `taskType` and is
otherwise identical to v1, including the task-specific `result` schemas. Result
nodes expose `version` and a `leaseId` that is null for v1.

`WorkGraphTaskLeaseExpiration/v1` contains exactly `leaseCommentNodeId`,
`leaseId`, `expiredAt`, and a non-empty bounded `reason`.

Assignment, Lease, Result, Lease Expiration, and Result Acceptance comments are
mutually exclusive and are never also emitted as `GitHubIssueComment`. Malformed
marked comments emit `WorkGraphError` and never a partial success-shaped
artifact. None of these comments closes or deletes a task; this Source is
read-only.

## Worker queue configuration

The configured worker set lives in a versioned repository file:

```yaml
version: 1
workers:
  - workerId: validator-1
    agentProfile: issue-validator
    slots: 2
    leaseDuration: PT15M
  - workerId: info-requester-1
    agentProfile: issue-info-requester
    slots: 1
    leaseDuration: PT15M
```

`version` must be exactly `1` and `workers` must be non-empty. Each worker needs
a stable `workerId` of 1–64 ASCII letters, digits, `-`, `.`, or `_` (no `/`, so
the derived slot IDs stay unambiguous), a supported `agentProfile`, an integer
`slots` between 1 and 16, and a positive `leaseDuration`. Durations are strict
ISO-8601 built only from whole days, hours, minutes, and seconds
(`P[nD][T[nH][nM][nS]]`); calendar-relative `Y`/`M`/`W` designators and
fractional components are rejected because a lease deadline must be an exact,
offset-independent number of seconds. A duration must be between 1 second and 24
hours. Duplicate worker IDs, duplicate derived slot IDs, unknown fields, wrong
types, more than 64 workers, and files larger than 256 KiB are all rejected.

Each worker becomes a `WorkGraphWorker` carrying `workerId`, `agentProfile`,
`configuredSlotCount`, `leaseDuration`, `leaseDurationSeconds` (so an expiry
Reaction computes `expiresAt` without re-parsing ISO-8601), and the
configuration provenance `configRepository`, `configRef`, `configPath`,
`configBlobOid`, and `configDigest`. Each unit of concurrency becomes a
`WorkGraphWorkerSlot` with the deterministic one-based `slotId`
`{workerId}/{slotNumber}`, plus `slotNumber`, `workerId`, `agentProfile`,
`enabled`, and `retiring`:

```text
(WorkGraphWorker { workerId: "validator-1" })
  -[:HAS_SLOT]->
(WorkGraphWorkerSlot { slotId: "validator-1/1" })
```

### Loading and convergence

The bootstrapper fetches, validates, and projects the worker file **before** any
Issue or task artifact, so a query that bootstraps capacity always sees workers
and slots ahead of the Assignments and Leases that reference them. The live
Source converges the same file:

- once at `start()`, so a restarted Source re-states configured capacity even if
  `push` deliveries were missed while it was down;
- on every `push` whose repository, ref, and path match the configuration
  exactly. GitHub truncates the `commits` array of a large push, so a push that
  cannot be *proven* irrelevant is converged rather than ignored — re-reading
  the file is idempotent, while missing a change would leave stale capacity.

Both paths call exactly the same validator and the same projection function, so
a file that bootstrap accepts one way can never be projected differently live.

A worker file that cannot be **read** (transport, authentication, or a 5xx) is
never treated as "no workers": `start()` fails, a `push` delivery answers `503`
so GitHub redelivers, and a bootstrap fails. A file that *is* read but is
deterministically **invalid** — no blob at the configured path, a non-text or
oversized blob, or a body failing the grammar above — projects a single
`WorkGraphError` node with the stable ID `workgraph-error:worker-config` and
leaves the previously projected workers untouched. Silently emptying the pool
would make a broken configuration look like "no capacity configured".

### Capacity changes and their bounded limitation

Removing a worker from the file deletes its `WorkGraphWorker`, its slots, and
its `HAS_SLOT` relations. Reducing an existing worker's `slots` does **not**
delete the excess slots: they are re-projected with `enabled = false` and
`retiring = true`. An in-flight Lease therefore keeps a valid `LEASES_SLOT`
target until it reaches a Result or an Expiration, while the retired slot is
never offered for a new Lease. A capacity query must select free capacity with
`slot.enabled = true`. Growing capacity back re-enables the same stable slot
identities; a Lease is never silently moved to a different slot.

Knowing which slots to retire requires knowing what was projected before, so the
live Source keeps a small retirement ledger (`workerId` → highest slot number
ever projected) in its own durable state store, next to the delivery dedupe
markers. **Bounded limitation:** that ledger is Source-local. A clean bootstrap
builds a fresh snapshot from the configured file alone, so slots retired before
that bootstrap are not re-materialized, and a Lease still naming one of them
produces no capacity row rather than a guessed binding. Recovering the ledger
after a clean bootstrap needs no action beyond the next convergence.

### Lease trust

The workflow defines an active Lease in terms of *trusted* artifacts: a Lease is
active until a **trusted** Result or a **trusted** Expiration ends it. A Source
that publishes a derived `isActive` therefore has to know which identities are
trusted, exactly as it already has to know the configured organization,
repository allowlist, and task Issue Type. `leaseTrust` states that explicitly:

```yaml
leaseTrust:
  dispatchers:            # may acquire a Lease
    - id: U_kgDOAbcDisp
      login: drasi-workqueue-dispatcher
  reporters:              # may end a Lease with a Result/v2 or an Expiration/v1
    - id: U_kgDOAbcRept
      login: drasi-result-reporter
```

Both lists must be non-empty and free of duplicate IDs, and each identity must
carry a non-empty node ID *and* login. Both are matched, mirroring how
`taskIssueType` requires the configured ID *and* name: a renamed or recreated
account stops matching rather than silently inheriting trust.

`leaseTrust` is optional and **fails closed**. With no configured trust nothing
is trusted, so no Lease binds an anchor, no Result or Expiration ends anything,
and no lease is ever counted active. A deployment running the worker queue must
configure it. The bootstrapper inherits it from the Source, so a snapshot and a
live delivery can never disagree about who is trusted.

Setting `leaseTrust` **requires `workerConfig`**: the same token and endpoint
that read the worker file also reconcile a task's current lifecycle comments
(below), so trust without that credential would leave the Source unable to
interpret its own deliveries. The token needs `Issues: Read` on the task
repositories in addition to `Contents: Read` on the worker file repository.

Every Lease, Result, and Lease Expiration node exposes a boolean `trusted`. An
untrusted artifact is still projected with its full provenance — so a forged
comment is visible rather than invisible — but it emits **no** `LEASE_ANCHOR`,
`RESULT_FOR_LEASE`, or `EXPIRES_LEASE` edge and contributes nothing to the fold.

**Trust covers the editor, not just the author, and is role-matched.** GitHub
preserves `comment.user` across an edit, so trusting the author alone would let
anyone with edit rights rewrite a trusted author's ordinary comment into a
Lease, a Result, or an Expiration. A lifecycle artifact is therefore trusted
only when the preserved author *and* the identity that last edited the comment
both hold **the role that artifact requires** — a `dispatcher` for a Lease, a
`reporter` for a Result or an Expiration.

Holding some lifecycle role is not enough. A configured reporter cannot edit a
comment into a trusted Lease, and a configured dispatcher cannot edit one into a
trusted Result or Expiration: a reporter is not authorized to acquire capacity,
and a dispatcher is not authorized to report an end.

The webhook reads the editor from the delivery `sender` on an `edited` action;
the bootstrapper reads GitHub's `editor` field on the fetched comment, and an
absent editor means the comment was never edited. Both are projected as
`editorLogin` and `editorId` on every comment node.

### Lease lifecycle and active-lease counting

A `WorkGraphTaskLease` node is keyed by its own comment node ID. **Every
acquisition fact lives there and nowhere else**: `leaseId`,
`assignmentCommentNodeId`, `workerId`, `slotId`, `taskNodeId`, `acquiredAt`,
`expiresAt`, the author and editor provenance, and `trusted`. That node has
exactly one writer for its whole lifetime.

A `WorkGraphTaskResult/v2` carries only `leaseId`, so it cannot address the
Lease node. A trusted Lease therefore also reaches a **lease anchor**,
`WorkGraphTaskLeaseAnchor`, keyed by `{taskNodeId}:{leaseId}`, and trusted
lifecycle artifacts bind to it. Because `leaseId` is free text in a comment body
while the task node ID is the GitHub-assigned Issue the comment was written on,
scoping the key by the task makes "the named task agrees" *structural*: a
comment on one task can never reach another task's anchor. GitHub node IDs
contain no colon, so the first separator always terminates the task ID.

#### The lifecycle is a fold, never a per-comment write

**No comment ever writes anchor state.** Each delivery states only that
comment's *current* contribution — it acquires a lease, it ends a lease, or it
contributes nothing — and the Source folds those contributions into a ledger
keyed by `(task, leaseId)`, then recomputes each affected anchor from every
artifact that currently survives. `isActive`, `endReason`, `endedAt`,
`endCommentNodeId`, `acquisitionCount`, and `endClaimCount` are all derived from
that fold.

That is what makes current-state semantics hold in every direction:

- **Re-observing an acquisition is a no-op.** A pin, an unpin, a body-preserving
  edit, or a redelivery restates the same set member, so it can never resurrect
  a lease that has been ended.
- **Removing an end releases its hold.** Deleting a Result, or editing it onto a
  different `leaseId`, recomputes from the ends that survive: with no end left
  the lease is active again, and with another end left that one takes over.
- **Duplicate and mixed ends apply once.** The end is chosen deterministically —
  earliest authoritative instant, then the stable comment node ID — so a Result
  and an Expiration for one lease, in either delivery order, produce the same
  result, and a second end of the same kind changes nothing.
- **Moving a comment updates both anchors.** A rekeyed Lease or end recomputes
  the anchor it left as well as the one it joined.

A Result's authoritative end instant is its own GitHub comment timestamp; an
Expiration's is its `expiredAt`. Both are canonical fixed-width UTC, so ordering
them lexicographically is ordering them chronologically.

An **Expiration only counts when its `leaseCommentNodeId` names the Lease
comment that currently survives on that anchor.** A stale or mismatched
reference stays projected as its own artifact, and is still counted in
`endClaimCount`, but cannot end anything.

**Duplicate acquisitions fail closed.** If two trusted Lease comments claim one
`(task, leaseId)`, the anchor is inactive with `endReason: "conflict"`, so the
ambiguous identity can neither be dispatched against nor silently rewritten —
each Lease keeps its own facts on its own node. Deleting or editing back down to
one acquisition restores the state the survivor implies, including any end that
still applies.

An anchor with **no** surviving trusted acquisition is not materialized at all;
an existing one is deleted. An end naming a lease that was never acquired — a
cross-task or unknown `leaseId` — therefore leaves nothing for a query to bind
to or count.

#### Reconciling a task the ledger has never seen

The bootstrapper's fold is transient — it is not handed to the Source, and the
Source's durable ledger starts empty. A ledger that has never seen a task
therefore cannot distinguish "this lease was never acquired" from "this lease
was acquired before I existed", which is exactly the state after a clean
bootstrap.

Reconciliation is triggered by the body the comment is moving *away* from as
well as the one it is moving to. Editing a bootstrapped Lease, Result, or
Expiration into ordinary, `v1`, or invalid content emits only a retraction, and
a ledger that had never seen the task would apply that to nothing and leave the
historical anchor stale. Any edit or delete of a comment on a configured task is
treated as lifecycle-relevant for the same reason — reconciliation happens at
most once per task, so erring wide costs one read while erring narrow loses
state. Separately, every anchor a delivery *names* — via its current or previous
body — is re-projected from the ledger afterwards, which is how deleting a
bootstrapped Lease removes an anchor the ledger itself never recorded.

Reconciliation only happens when the lease lifecycle is actually configured.
With no `leaseTrust` — and therefore no credential — nothing can ever be
trusted, so a lifecycle-shaped comment is simply projected as an ordinary
untrusted artifact with `trusted = false`, and no anchor is produced. It is
never an error: reaching for an API client that was never configured would turn
an ordinary untrusted comment into a failed delivery.

So before applying a lifecycle delivery for a task it has not seen, a
lifecycle-configured Source re-reads **that task's current comments** over the same GitHub GraphQL endpoint
and credential the worker file uses — no side service and no second transport —
and rebuilds that task's ledger entries by running each comment through the same
`Converter`, with the same task typing, trust, and grammar rules. Only then is
the delivery applied. The task is recorded as reconciled, so this costs one
extra read per task per Source lifetime, and the durable ledger carries that
across restarts.

That read is performed **outside the shared ingress gate**, like the worker-file
read on a `push`: a remote call can take tens of seconds, and holding the gate
across it would stall every other delivery for this Source. The pre-gate checks
that decide whether to fetch are deliberately dirty. Under the gate the Source
re-checks the delivery marker and `knows_task` authoritatively, and discards the
prefetched snapshot if a concurrent delivery reconciled the task first.
Concurrent duplicate reconciliation is idempotent either way — `reset_task` plus
the same comment set converges on the same ledger — so discarding is an
optimization, not a correctness requirement.

This is what makes a live `WorkGraphTaskResult/v2` against a historical Lease
*end* it rather than delete an anchor the Source thinks was never acquired, and
what lets a historical duplicate acquisition be discovered as a conflict.
Because reconciliation reads GitHub's *current* comments, a delete or an edit
converges on what GitHub actually holds after the event.

#### Durability ordering

The live Source keeps the ledger in its durable state store, updated under the
same delivery gate that guards the WAL append and the delivery marker, in this
order: **append every graph change, then persist the ledger, then record the
delivery marker.** Persisting the ledger first would let a failed append leave
the ledger advanced, so a redelivery would compute a smaller affected set and
permanently drop the anchor changes the first attempt never wrote — most
visibly for a delete or a rekey, which touch two anchors. Every contribution is
keyed by its comment node ID and states current state rather than a delta, so a
replayed delivery converges on the same ledger and the same anchors.

The bootstrapper folds the identical structure over its fetched comment
snapshot, so the same set of current comments yields the same anchors either
way, differing only in `effectiveFrom`.

#### Counting

Active-lease counting is one positive match, with no `OPTIONAL MATCH` and no
subtraction:

```cypher
MATCH (lease:WorkGraphTaskLease)-[:LEASE_ANCHOR]->(anchor:WorkGraphTaskLeaseAnchor)
WHERE anchor.isActive = true
RETURN lease.workerId AS workerId, count(lease) AS activeLeaseCount
```

It is exact because it counts surviving rows rather than subtracting ends: a
Result or an Expiration both clear `isActive`; both together clear it once;
duplicates change nothing; only a trusted Lease has a `LEASE_ANCHOR` edge; and
each lease contributes exactly one row through exactly one edge, so no
`DISTINCT` is needed.

Because `isActive` is a real recomputed boolean, the design's deadline query
works as written — the scheduled expiry is **cancelled** by a trusted completion
rather than firing unconditionally:

```cypher
MATCH (lease:WorkGraphTaskLease)-[:LEASE_ANCHOR]->(anchor:WorkGraphTaskLeaseAnchor)
WHERE drasi.trueLater(anchor.isActive, datetime(lease.expiresAt))
RETURN lease.sourceCommentNodeId AS leaseCommentNodeId, anchor.leaseId AS leaseId,
       anchor.taskNodeId AS taskNodeId
```

Those three values are exactly the binding a `WorkGraphTaskLeaseExpiration/v1`
comment needs. The one adaptation from the design draft is that `isActive` and
the end reason live on the anchor rather than directly on the Lease node,
because a Result/v2 can address the anchor and cannot address the Lease comment.

History is fully retained: the Lease, Result, and Expiration comments each
remain their own node with their own provenance, and nothing is overwritten.

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

- Worker queue comments follow the same create/edit/delete rules as every other
  specialized comment. Editing an Assignment from v1 to v2 adds `ASSIGNED_TO`
  in place; editing a Lease onto a different `leaseId` removes the identity node
  it previously owned; editing a Lease into an ordinary comment removes every
  lease element.

Ingress verifies `X-Hub-Signature-256`, converts the payload, appends each
change to the WAL, persists the delivery dedupe marker, and then acknowledges.
A `push` delivery follows the same order: it converges the worker graph, appends
the resulting changes, and only then records the dedupe marker, so a redelivered
push is absorbed. Stable IDs make redelivery idempotent at the graph identity
level, but consumers must still tolerate at-least-once change delivery.
