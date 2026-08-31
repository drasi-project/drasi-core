# GitHub WorkGraph source

The GitHub WorkGraph source accepts signed GitHub webhook deliveries and projects
the WorkGraph v1 protocol into Drasi. The prototype ingress is GitHub webhook ->
operator-managed ngrok -> this source. There is no admission bridge; the source
does not poll GitHub or synthesize webhook events.

## v1 boundary

Core recognizes these exact body prefixes:

- `WorkGraphTask/v1`
- `WorkGraphTaskAssign/v1`
- `WorkGraphTaskDispatch/v1`
- `WorkGraphTaskResult/v1`
- `WorkGraphTaskEvaluate/v1`
- `WorkGraphTaskRoute/v1`

An ordinary user-created Issue carrying the exact, case-sensitive `workgraph`
label and not matching the configured task Issue Type is a **Root Issue**.
Each continuous labeled period is one admission generation. Removing the label
retracts the Root Issue and its run from the internal projection. Re-adding it
starts a fresh generation. GitHub issue revisions are retained as admission
tombstones so delayed deliveries cannot resurrect a removed generation. The title
and body are frozen for the generation; changing either requires removing and
re-adding the label. The source does not perform GitHub cleanup writes.

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
owns agent-slot leasing, and exposes an object-safe projector boundary. The
definition and task body semantics are implemented by the injected v1 projector;
the Dogfooding `github-workgraph-v1` wrapper supplies that projector and owns the
`wg-` queries and runtime configuration.

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

Allocator state schema 16 durably records Issue-state fingerprints,
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
its GitHub node ID and exact login. The agent/definition read token also performs
authoritative Issue-label reads during ambiguous ordering transitions.

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

- Nodes: `GitHubIssue`, `WorkGraphRootIssue`, `WorkflowDefinition`, `TaskDefinition`,
  `WorkflowRun`, `WorkGraphTask`, `WorkGraphTaskAssign`,
  `WorkGraphTaskDispatch`, `WorkGraphTaskResult`,
  `WorkGraphTaskEvaluate`, `WorkGraphTaskRoute`, `WorkGraphTaskArtifact`,
  `WorkGraphTaskLease`, `WorkGraphAgent`, `WorkGraphAgentSlot`, and
  `WorkGraphError`.
- Relations: `HAS_ROOT`, `HAS_TASK`, `DECLARES_CHILD`, `USES_DEFINITION`,
  `INSTANCE_OF`, `IN_RUN`, `TASK_FOR`, `ROOT_TASK_FOR`, `RUN_FOR`,
  `ASSIGNS`, `DISPATCHES`, `RESULT_FOR`, `RESULT_FROM_LEASE`, `EVALUATES`, `ROUTES`,
  `ARTIFACT_FOR`, `HAS_SLOT`, `LEASE_FOR`, and `LEASES_SLOT`.

State is durable and fail-closed. Unsupported persisted state must be cleared
before starting this prototype revision; no protocol migration is provided.
