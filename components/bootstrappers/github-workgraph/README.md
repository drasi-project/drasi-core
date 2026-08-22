# GitHub WorkGraph Bootstrap Provider

`drasi-bootstrap-github-workgraph` snapshots the initial graph for a
`github-workgraph` Source using GitHub's read-only GraphQL API.

## Snapshot scope

When `agentConfig` is set, bootstrap first reads and validates the agent-capacity
configuration file and projects its `WorkGraphAgent`, `WorkGraphAgentSlot`,
and `HAS_SLOT` elements **before** any Issue or task artifact, so a capacity
query always sees agents and slots ahead of task artifacts.

For every repository allowed by the Source configuration, bootstrap retrieves:

- open generic Issues, excluding the configured exact task Issue Type;
- open Pull Requests;
- paginated OPEN and CLOSED Issues matching the configured task Issue Type;
- each task's authoritative parent Issue and parent repository;
- all comments on every task, including closed tasks; and
- comments/reviews on open generic Issues and PRs.

Task Issue Type identity (`taskIssueType.id` and `taskIssueType.name`),
organization, and repository allowlist are inherited from the Source
configuration. Both type fields must match exactly. Core does not hardcode a
deployment ID.

Every fetched object is reshaped into webhook JSON and passed through the same
`mapping::Converter` used by live deliveries. Consequently bootstrap and live
use identical:

- `WorkGraphTask`, `WorkGraphTaskAssignment`, `WorkGraphTaskResult`,
  `WorkGraphTaskFeedback`, `WorkGraphTaskResultAcceptance`, and `WorkGraphError`
  v1 parsing;
- agent file validation and agent/slot projection, which come from the same
  `agents` and `mapping` code the live Source uses;
- node and relation IDs/directions;
- generic open-only and task open/closed behavior;
- ordered `statusLabels`/`workgraphLabels` arrays, derived `currentStatus` and
  `workgraphInclude`, and lowercase issue/task state with boolean `isOpen`;
- repository allowlist decisions; and
- strict task and specialized-comment wire formats.

This includes `TASK_FOR:{child.databaseId}` relation identity. Bootstrap keeps
the task/parent GraphQL node IDs as relation endpoints, exactly like live
mapping.

Bootstrap never creates `GitHubIssue` for a typed task. It emits the task's
parent Issue only when the parent is open and its authoritative repository is
allowed. `TASK_FOR` still records the native parent identity from GitHub.

## Configuration

```yaml
bootstrapProviders:
  - id: github-workgraph-bootstrap
    kind: github-workgraph
    token:
      kind: Secret
      name: github-workgraph-api-token
    apiBaseUrl: https://api.github.com/graphql
    maxConcurrency: 4

sources:
  - id: github-workgraph
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
    webhook:
      secret:
        kind: Secret
        name: github-workgraph-webhook-secret
      leaseValidationToken:
        kind: Secret
        name: github-workgraph-lease-validation-token
    durability:
      enabled: true
      maxEvents: 10000
      capacityPolicy: RejectIncoming
    bootstrapProvider: github-workgraph-bootstrap
```

| Bootstrap field | Description |
|---|---|
| `token` | Read-only GitHub token; use a Secret reference |
| `apiBaseUrl` | GraphQL endpoint; defaults to `https://api.github.com/graphql` |
| `maxConcurrency` | Bound for request and repository concurrency; defaults to 4 |

The agent file location and protocol trust configuration are **not** bootstrap
fields. Like organization, repository allowlist, and task Issue Type, they are
inherited from the parent Source's `agentConfig` (`repository`, `ref`, `path`)
and `protocolTrust` (`assigners`, `reporters`), so bootstrap and streaming can
never disagree about which file is authoritative or which producers are
trusted. Only the read credential and endpoint above are the bootstrapper's own.

The token needs read access for Issues, Pull Requests, and repository metadata,
plus repository contents when `agentConfig` is set. The agent file is read
with this same token and `apiBaseUrl` — no separate credential, endpoint, or
service. The provider never writes to GitHub.

The Source's `agentConfig` is optional; omitting it snapshots no agent or slot
elements.
When present, a file that cannot be **read** (transport, authentication, or a
5xx) fails the bootstrap outright, because claiming an empty agent pool would
silently stop every dispatch. A file that *is* read but is deterministically
**invalid** — missing at the configured path, non-text, oversized, or failing
the strict `version: 1` grammar — snapshots a single `WorkGraphError` node with
the stable ID `workgraph-error:agent-config`, projects no agent, and lets the
rest of the snapshot complete.

A bootstrap builds a fresh snapshot, so it has no prior projection to retire
slots against: it projects exactly the configured slots, all `enabled`. Slot
retirement after a live capacity reduction is Source-local; see the Source
README's *Agent capacity configuration*.

Bootstrap never folds protocol comments into allocator state and never creates
synthetic `WorkGraphTaskLease` nodes. Prototype clean activation must be
externally preflighted to contain no open Assignment/v1 artifacts; normal
restart uses the Source's persistent allocator and query state instead of a new
bootstrap injection.

Use a top-level provider and string reference. The Source owns organization,
repository, and task Issue Type scope; the bootstrap descriptor reads those
fields from the parent Source configuration.

## Pagination and consistency

Repositories, open generic Issues/PRs, OPEN tasks, CLOSED tasks, labels,
comments, and reviews follow every GraphQL cursor. Tasks use the repository
`issues(states: ..., filterBy: {type: $issueType})` connection. GitHub's live
GraphQL schema defines `IssueFilters.type` as a `String`; bootstrap passes the
configured exact Issue Type name as that variable, then defensively requires
both the configured name and GraphQL node ID on every returned Issue. It does
not use GraphQL search, so the 1,000-result search cap cannot truncate a
snapshot. Open tasks selected by both the generic-open and task connections are
emitted once and never as generic Issues.

Parent identity and repository are selected with each task rather than inferred
from repository iteration. Embedded parent repositories select and reshape the
same default branch, topics, and metadata as organization repository
enumeration. Repository processing and HTTP requests are independently bounded
by `maxConcurrency`.

Repository tasks complete in arbitrary order, so their results are reassembled
by repository index before the snapshot is folded; the emitted snapshot never
depends on scheduling.

## Snapshot folding

The shared converter emits an ordered stream of creates, convergent updates, and
defensive deletes. A snapshot is that stream's *final* state, so bootstrap
replays it rather than approximating it: a repeated element converges by merging
exactly as Drasi merges an `Update` at query time, a `Delete` removes an element
the stream had already produced (and is a no-op otherwise), and first-appearance
order is preserved.

This is what keeps a repeatedly observed element — a repository reached both
directly and as a task parent, for example — in exactly the state the live
Source converges to.

Bootstrap deliberately does not fold protocol comments into allocator state or
project synthetic `WorkGraphTaskLease` nodes. The Source is the only allocation
authority. Prototype clean activation must therefore be externally preflighted
to contain no open Assignment/v1 artifacts; normal restart preserves the
Source's durable allocator and query state instead of injecting bootstrap state.

GitHub does not offer a transaction spanning these queries. Bootstrap fails on
any repository task error rather than intentionally returning a partial
snapshot, but GitHub state can change between successful pages.

Webhook delivery has no durable replay cursor, so `BootstrapResult` always has
`source_position: None`.

## Validation

```bash
make build
make test
make lint
```

Wiremock tests assert the `filterBy.type` query/variable contract, exact-ID
defense, initial and cursor pages (including more than 1,000 tasks), open generic
and open/closed task selection, complete order-independent parent repositories,
comments on closed tasks, strict task/specialized-comment errors, repository filtering,
requested-label filtering, and live-converter parity.

Agent-capacity tests additionally assert agent/slot snapshot ordering ahead of
every task artifact, malformed and missing agent files becoming an explicit
error with no agent pool, oversized/truncated/binary and non-Blob rejections,
an unreadable file failing the bootstrap, omitted `agentConfig` projecting
nothing, requested-label filtering of agent elements, shared Assignment/v1
`agentId` parsing and projection, bootstrap/live agent parity, and
byte-identical repeated snapshots.
