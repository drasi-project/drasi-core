# GitHub WorkGraph source

The GitHub WorkGraph source accepts signed GitHub webhook deliveries and projects
the WorkGraph v1 protocol into Drasi. The prototype ingress is GitHub webhook ->
operator-managed ngrok -> this source. There is no admission bridge; the source
does not poll GitHub or synthesize webhook events.

## v1 boundary

Core recognizes these exact body prefixes:

- `WorkGraphTask/v1`
- `WorkGraphTaskAssignment/v1`
- `WorkGraphTaskFork/v1`
- `WorkGraphTaskJoin/v1`
- `WorkGraphTaskDispatch/v1`
- `WorkGraphTaskResult/v1`
- `WorkGraphTaskEvaluation/v1`
- `WorkGraphTaskRoute/v1`
- `WorkGraphTaskError/v1`

The Assignment, Fork, Join, Dispatch, Result, Evaluation, Route, and Error
comment markers are the **WorkGraphTaskActions**. Assignment, Fork, Join, and
Dispatch are authored under the assigner trust role; Result, Evaluation, Route,
and Error are authored under the reporter trust role. Fork and Join are recorded
as lifecycle artifacts and do not directly affect lease allocation. Core
recognizes the exact marker prefixes and enforces the trust role, but never
parses a WorkGraphTaskAction JSON body.

An ordinary user-created Issue carrying the exact, case-sensitive `workgraph`
label and not matching the configured task Issue Type is a **Root Issue**.
Each continuous labeled period is one admission generation. Removing the label
retracts the Root Issue and its run from the internal projection. Re-adding it
starts a fresh generation. GitHub issue revisions are retained as admission
tombstones so delayed deliveries cannot resurrect a removed generation. The title
and body are frozen for the generation; changing either requires removing and
re-adding the label. The source does not perform GitHub cleanup writes.

Every WorkGraph-owned ID has the exact grammar
`urn:drasi:workgraph:id:v1:<type>:sha256:<64 lowercase hex>`. The digest is
SHA-256 over the UTF-8 sequence
`["urn:drasi:workgraph:id:v1", type, ...semanticInputs]`, with each part
preceded by its unsigned 64-bit big-endian byte length. Core derives admission
IDs from `(rootIssueId, deliveryId)` and lease IDs from
`(taskId, assignmentId, decimalAttempt)`.
At typed protocol boundaries Core requires the corresponding `task`,
`assignment`, `lease`, `result`, `evaluation`, `route`, or `workflow-run`
identifier. GitHub node IDs, configured agent/slot IDs, and human
`workflowDefinitionId` values remain external and unchanged.

The hierarchy is:

```text
Root Issue -> Root Task -> child tasks
```

Root Tasks are sub-Issues of the Root Issue and all tasks carry the same top-level
`rootIssueId`. Task Issues must have the configured Issue Type and must pass the
configured exact creator and webhook-actor trust checks.
Generated WorkGraphTask Issues are never Root Issues, even if they carry the
admission label.

Every ordinary Issue is also projected directly as a normalized `GitHubIssue`.
Its sorted `workgraphLabels` namespace is case-sensitive. Only exact
`workgraph:ignore` and `workgraph:error` labels set `workgraphInclude` to false;
similarly spelled labels do not.

Core normalizes authenticated GitHub documents, persists delivery deduplication,
owns agent-slot leasing, and exposes an object-safe projector boundary. Task body
semantics are implemented by the injected v1 projector; the Dogfooding
`github-workgraph-v1` wrapper supplies that projector and owns the `wg-` queries
and runtime configuration.

The source never fetches or projects a workflow definition. The pinned
`WorkGraphWorkflowDefinition/v1` body is loaded by the Reaction, which owns every
definition-dependent decision (declared children, task metadata, transition
reachability, route authorization, wait and terminal interpretation). The
`workflowDefinition` block is retained only to pin the same immutable definition
location the Reaction uses and to supply the read-only credential for
authoritative Issue-label reads; a `push` touching that file is acknowledged with
no content. The configured agent inventory is still loaded, because Core owns
agent slots and leases.

Human-authored comments on an admitted Root Issue are supplied to that projector
as `UpsertRootIssueComment` evidence. The document carries the comment source
key, direct `rootIssueId`, `admissionId`, repository locator, issue number,
author ID/type/login, body, and immutable creation/current update revisions.
Edits replace the document and deletes emit `DeleteRootIssueComment` with
`sourceKey`, `rootIssueId`, `admissionId`, `repositoryOwner`, `repositoryName`,
`repositoryNodeId`, `issueNumber`, and `updatedAtRevision`. Core persists
locator-bearing revision tombstones and retracts comments edited by bots.
Qualification as wait/resume evidence remains entirely projector-owned.

### Projector consumer update required

Consumers of this Wave 1 projector API must make these source changes before
they compile:

- handle `UpsertGitHubIssue` and `DeleteGitHubIssue` when exhaustively matching
  `ProjectionInput` (Core consumes these before calling `prepare`, so the arms
  are defensive);
- handle `UpsertRootIssueComment` and `DeleteRootIssueComment` as
  generation-bound wait/resume evidence inputs;
- consume `workgraphLabels` and `workgraphInclude` on `TaskDocument` and
  `RootIssueDocument`, retaining the Root Issue and its admission generation
  while excluded;
- populate `rootIssueId` and `workflowRunId` on task, assignment, and dispatch
  bindings;
- populate the `results` and `evaluations` allocator projection collections
  with the corresponding identity-bearing bindings.
- populate `routes` with the canonical selected Result/Evaluation chain,
  action, attempt, and configured attempt bound.
- preserve `createdAtRevision` on lifecycle artifacts and
  `stateFingerprint`/`authorizationTransition` on issue revision records when
  matching projection inputs.

This is a single v1 contract change. No alternate wire format or compatibility
projection is provided.

Allocator state schema 19 durably records Issue and lifecycle-comment revisions,
authorization generations/cutoffs, generation-bound applied Route decisions,
and Root Issue comment revision tombstones.
Older state is rejected explicitly rather
than migrated.

## Configuration

```yaml
kind: github-workgraph
organization: example-org
repositories:
  - example-repo
taskIssueType:
  id: IT_kwDOExample
  name: Task
protocolTrust:
  taskCreators:
    - id: U_example
      login: github-actions[bot]
  assigners:
    - id: U_example
      login: github-actions[bot]
  reporters:
    - id: U_example
      login: github-actions[bot]
agentConfig:
  repository: example-org/example-repo
  ref: main
  path: .github/workgraph/agents.yaml
  token:
    kind: Secret
    name: github-workgraph-v1-read-token
workflowDefinition:
  repository: example-org/example-repo
  ref: main
  path: .github/workgraph/workflows/issue-lifecycle-v1.body
  token:
    kind: Secret
    name: github-workgraph-v1-read-token
webhook:
  host: 0.0.0.0
  port: 9000
  path: /github/workgraph
  secret:
    kind: Secret
    name: github-workgraph-v1-webhook-secret
  leaseValidationToken:
    kind: Secret
    name: github-workgraph-v1-lease-token
durability:
  enabled: true
  capacityPolicy: RejectIncoming
```

`protocolTrust` requires `agentConfig`. Every trusted identity is matched by both
its GitHub node ID and exact login. `workflowDefinition.repository`, `ref`, and
`path` are validated and pin the Reaction's definition identity, but the file is
never read; its token performs authoritative Issue-label reads during ambiguous
ordering transitions.

Sparse `sub_issue_removed` deliveries may identify the child only by numeric
`sub_issue_id`. The source durably indexes that database ID when it first sees the
task and uses it to retract the parent relation after restart.

The lease validation endpoint is:

```text
POST {webhook.path}/lease/validate
Authorization: Bearer <leaseValidationToken>
```

Its request uses the canonical v1 allocation fields: `taskId`, `leaseId`,
`assignmentId`, `executorId`, `slotId`, and `claimId`. Source-owned task,
Assign, Lease, Dispatch, Result, Evaluate, and Route representations additionally
carry `rootIssueId` and `workflowRunId` alongside `taskId`, so root and run
lookups do not require a graph traversal.
The first claim durably reserves that active Dispatch Lease for one Result writer;
a competing claim is rejected. A successful response contains `leaseId`,
`taskId`, `assignmentId`, `attempt`, `executorId`, `slotId`, `claimId`,
`acquiredAt`, and `expiresAt`. `attempt` is the authoritative one-based active
Lease attempt (bounded to 64, and therefore a safe JSON integer). JSON object
field ordering is not part of the contract.
An active `WorkGraphTaskLease` remains active and occupies its slot after
Dispatch acceptance. Its `hasDispatch` property records that exact transition;
expiry makes the Lease historical and allocates a fresh retry attempt.

## Projected graph

The source advertises only the current v1 schema:

- Nodes: `GitHubIssue`, `WorkGraphRootIssue`, `WorkGraphRootIssueComment`,
  `WorkflowRun`, `WorkGraphTask`, `WorkGraphTaskAssign`,
  `WorkGraphTaskFork`, `WorkGraphTaskJoin`,
  `WorkGraphTaskDispatch`, `WorkGraphTaskResult`,
  `WorkGraphTaskEvaluate`, `WorkGraphTaskRoute`, `WorkGraphTaskError`,
  `WorkGraphTaskArtifact`, `WorkGraphTaskLease`, `WorkGraphAgent`,
  `WorkGraphAgentSlot`, and `WorkGraphError`.
- Relations: `IN_RUN`, `TASK_FOR`, `ROOT_TASK_FOR`, `PRECEDES`, `RUN_FOR`,
  `ACTION_FOR`, `ASSIGNS`, `FORK_CHILD`, `JOINS_FORK`,
  `JOIN_RESULT`, `JOIN_EVALUATION`, `DISPATCHES`, `RESULT_FOR`,
  `RESULT_FROM_LEASE`, `EVALUATES`, `ROUTES`, `ROUTE_FOR`, `ERROR_FOR`,
  `ARTIFACT_FOR`, `HAS_SLOT`, `LEASE_FOR`, and `LEASES_SLOT`.

Every advertised label is observed runtime state. The retired static definition
labels (`WorkflowDefinition`, `TaskDefinition`, `HAS_ROOT`, `HAS_TASK`,
`DECLARES_CHILD`, `USES_DEFINITION`, `INSTANCE_OF`, `FORK_CHILD_DEFINITION`) and
the wait/terminal labels (`WorkGraphWait`, `WorkGraphTerminal`, `ENTERS_WAIT`,
`WAIT_IN_RUN`, `CONCLUDES`, `RESUMES`) have no producer. `WorkGraphTaskArtifact`
and `ARTIFACT_FOR` remain only for the Core lease ledger's lease-artifact detail
projection.

State is durable and fail-closed. Unsupported persisted state must be cleared
before starting this prototype revision; no protocol migration is provided.
