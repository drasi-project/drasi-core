# GitHub WorkGraph Bootstrap Provider

`drasi-bootstrap-github-workgraph` snapshots the initial graph for a
`github-workgraph` Source using GitHub's read-only GraphQL API.

## Snapshot scope

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

- `WorkGraphTask`, `WorkGraphTaskResult`, and `WorkGraphError` parsing;
- node and relation IDs/directions;
- generic open-only and task open/closed behavior;
- repository allowlist decisions; and
- strict raw task and Result wire formats.

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
    webhook:
      secret:
        kind: Secret
        name: github-workgraph-webhook-secret
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

The token needs read access for Issues, Pull Requests, and repository metadata.
The provider never writes to GitHub.

Use a top-level provider and string reference. The Source owns organization,
repository, and task Issue Type scope; the bootstrap descriptor reads those
fields from the parent Source configuration.

## Pagination and consistency

Repositories, open generic Issues/PRs, OPEN tasks, CLOSED tasks, labels,
comments, and reviews follow every GraphQL cursor. Parent identity and
repository are selected with each task rather than inferred from repository
iteration. Repository processing and HTTP requests are independently bounded by
`maxConcurrency`.

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

Wiremock tests cover initial and cursor pages, open generic and open/closed task
selection, parents, comments on closed tasks, strict task/Result errors,
repository filtering, requested-label filtering, and live-converter parity.
