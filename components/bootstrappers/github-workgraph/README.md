# GitHub WorkGraph Bootstrap Provider

`drasi-bootstrap-github-workgraph` is a [Drasi bootstrap provider](../README.md)
that snapshots one GitHub organization's currently-open Issues and Pull
Requests — plus their conversation comments and PR reviews — as the initial
state for a query subscribed to the
[`drasi-source-github-workgraph`](../../sources/github-workgraph/README.md)
source.

## Reuse, not duplication

This crate owns **all** GitHub API access; the streaming source itself makes
zero GitHub API calls (it only receives webhook deliveries). To keep that
separation without duplicating any WorkGraph domain logic, this crate:

1. Fetches data over the GitHub GraphQL v4 API, aliasing fields so the JSON
   shape matches GitHub's REST/webhook resource representation.
2. Performs a small amount of purely syntactic reshaping (flattening
   `defaultBranchRef`/`repositoryTopics` connections, nesting
   `head`/`base` ref+sha pairs, lowercasing a few enum fields) — no
   WorkGraph node/relation/status semantics are touched here.
3. Wraps each entity in a synthetic webhook-delivery envelope (e.g.
   `{"action": "opened", "organization": ..., "repository": ..., "issue": ...}`)
   and hands it to
   `drasi_source_github_workgraph::mapping::Converter::convert(...)` — the
   **exact same converter** the streaming source uses for live webhook
   deliveries.

Every node label, relation ID/direction, status-label derivation rule, and
`WorkGraphAssignment`/`WorkGraphResult`/`WorkGraphError` comment-parsing rule
therefore comes from the source crate, unchanged. This crate never
re-implements any of that.

## Scope (prototype)

- One configured GitHub organization; every repository the token can see.
- Only currently **open** Issues and Pull Requests (no closed history).
- Issue/PR conversation comments and submitted or dismissed PR reviews.
- **Excluded**: GitHub Projects and Project Items, inline diff/review
  comments, closed-item history, reactions, and workflow-run execution
  state. None of these map to a WorkGraph node/relation the source
  understands.

## Configuration

| Field             | Type                 | Description                                                                 |
| ----------------- | -------------------- | ---------------------------------------------------------------------------- |
| `token`           | `ConfigValue<string>`| A **read-only** GitHub token. Use a `Secret` reference in production.       |
| `apiBaseUrl`      | `ConfigValue<string>`| GraphQL endpoint. Default `https://api.github.com/graphql`; override for GHE.|
| `maxConcurrency`  | `ConfigValue<usize>` | Bound on concurrently in-flight GraphQL requests and concurrent repository tasks. Default `4`. |

### Example

```yaml
bootstrap_provider:
  type: github-workgraph
  token:
    kind: Secret
    name: github-readonly-token
  maxConcurrency: 4
```

The organization is read from the parent `github-workgraph` Source
configuration, ensuring bootstrap and webhook streaming cannot target different
organizations.

### Authentication

The token must be **read-only**: a fine-grained personal access token scoped
to `Issues: Read`, `Pull requests: Read`, and `Metadata: Read` on the target
organization is sufficient, or an equivalent read-only GitHub App
installation token. This bootstrapper never requests or uses any write
scope, and never mutates GitHub state.

## `source_position` is always `None`

> **This bootstrapper never sets `source_position`.**

The GitHub WorkGraph source is driven entirely by webhook deliveries. Unlike
a database WAL LSN or a Kafka partition offset, a missed webhook delivery
cannot be re-requested from GitHub by position — there is no durable,
replayable cursor to snapshot against. Because of this, every
`BootstrapResult` this provider returns carries `source_position: None`, and
the framework cannot seed a bootstrap-to-streaming replay checkpoint for this
source. This is a fundamental limitation of GitHub's webhook delivery model,
not an oversight.

## Bounded concurrency

Two independent bounds, both derived from `max_concurrency`:

- A `tokio::sync::Semaphore`-gated `JoinSet` limits how many repositories are
  processed concurrently.
- The GraphQL client itself holds a semaphore bounding the number of
  concurrently in-flight HTTP requests, regardless of how many repository
  tasks are running.

Within one repository, issues, pull requests, and their comments/reviews are
fetched sequentially — cross-repository parallelism plus the client's
request-level semaphore already provide genuine bounded concurrency without
the added complexity of per-repository fan-out.

## Known prototype limitations

- GitHub does not provide a transaction spanning the GraphQL requests used to
  enumerate the organization. The bootstrapper fails rather than returning a
  known partial snapshot when a request or repository task fails, but upstream
  state can still change between successfully fetched pages.
- Issue/PR labels, comments, reviews, repositories, issues, and pull requests
  are cursor-paginated in full. Assignees and repository topics fit within
  GitHub's resource limits and are fetched inline.
- Enum casing (`state`, `state_reason`, `visibility`, review `state`) is
  lowercased to match the REST/webhook convention `Converter` expects; this
  covers every enum value currently used by `mapping.rs`.

## Testing

```bash
make build   # cargo build
make test    # cargo test (wiremock-backed, no network access required)
make lint    # cargo clippy --all-targets --all-features -- -D warnings
```

The test suite in `src/tests.rs` runs a fake GitHub GraphQL API via
[`wiremock`](https://docs.rs/wiremock) and verifies: multi-page cursor
pagination, `BootstrapRequest` node/relation label filtering, exact
`event_count`, `source_position` always being `None`, and that a
WorkGraph Assignment comment is reconstructed into a `WorkGraphAssignment`
node purely through the shared `Converter`/`workgraph::classify` — proving
this crate does not duplicate that parsing logic.
