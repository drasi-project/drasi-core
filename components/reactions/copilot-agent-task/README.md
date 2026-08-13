# Drasi Copilot Agent Task Reaction

`drasi-reaction-copilot-agent-task` is a Drasi reaction plugin ([`drasi-lib`](../../../lib))
that launches GitHub Copilot coding-agent tasks from WorkGraph routing decisions. It
subscribes to a single continuous query — the **launch query** — and, for every row newly
**added** to that query's result set, it:

1. Validates the row against configured allowlists (fail-closed).
2. Runs GitHub-state **preflight checks** (issue open, issue content unchanged, Project
   status unchanged, agent-profile file pinned by blob SHA).
3. Durably **reserves** the launch attempt before making any external call.
4. Calls `POST /agents/repos/{owner}/{repo}/tasks` to launch the Copilot agent task, with
   exactly one fallback-model retry on a clearly unsupported-model `422`.
5. Posts exactly one pure-JSON `workgraph.execution/v1` issue comment recording the launch.

`Update` and `Delete` diffs (and aggregation/no-op results) are never acted on — only rows
newly added to the query's result set can trigger a launch.

---

## Table of contents

1. [Why "launch query" rows only add](#why-launch-query-rows-only-add)
2. [Quick start](#quick-start)
3. [Launch row schema](#launch-row-schema)
4. [Configuration reference](#configuration-reference)
5. [Preflight checks](#preflight-checks)
6. [Reservation, idempotency, and recovery](#reservation-idempotency-and-recovery)
7. [Model fallback](#model-fallback)
8. [The prompt and `WorkGraphEvent/v1`](#the-prompt-and-workgrapheventv1)
9. [The `workgraph.execution/v1` comment](#the-workgraphexecutionv1-comment)
10. [Failure-state observability](#failure-state-observability)
11. [Ambiguous creation and reconciliation](#ambiguous-creation-and-reconciliation)
12. [Security](#security)
13. [Testing](#testing)
14. [Known limitations / integration caveats](#known-limitations--integration-caveats)

---

## Why "launch query" rows only add

The launch query is expected to express "this responsibility is ready to be launched" as a
row appearing in its result set (e.g. a Cypher query joining an issue, its routing decision,
and its Project status). The reaction only reacts to `Add`: an `Update` means the underlying
state changed (handled by re-running preflight against live GitHub state, not by reacting to
the diff), and a `Delete` means the row is no longer a launch candidate — neither should
trigger a new launch attempt.

## Quick start

```rust
use drasi_reaction_copilot_agent_task::CopilotAgentTaskReaction;

let reaction = CopilotAgentTaskReaction::builder("copilot-launcher")
    .with_query("launch-query")
    .with_token(std::env::var("GITHUB_TOKEN").unwrap())
    .with_allowed_repositories(vec!["my-org/my-repo".to_string()])
    .with_allowed_profiles(vec!["issue-validator".to_string()])
    .with_allowed_models(vec!["gpt-5".to_string(), "gpt-4".to_string()])
    .build()?;
```

The reaction requires a **durable** `StateStoreProvider` (`is_durable() == true`) — reservation
and execution records must survive restarts. Configure one on `DrasiLib::builder()`
(e.g. `drasi-state-store-redb`); the reaction fails to start otherwise.

## Launch row schema

Each row returned by the launch query must contain the following fields (camelCase, as
`RETURN ... AS <name>` aliases in Cypher):

| Field | Type | Notes |
|---|---|---|
| `repository` | string | `"owner/repo"` |
| `issueNumber` | integer | |
| `issueUrl` | string | |
| `issueNodeId` | string | GitHub GraphQL node ID of the issue (comment target) |
| `projectItemNodeId` | string | GitHub Projects (v2) item node ID |
| `projectNodeId` | string | GitHub Project (v2) node ID |
| `projectOwner` | string | GitHub organization or user that owns the Project |
| `projectNumber` | integer | GitHub Project number |
| `subjectType` | string | WorkGraph target subject type (`Issue` for this reaction) |
| `actorType` | string | WorkGraph actor type |
| `actorId` | string | WorkGraph actor identifier; must equal `agentProfile` |
| `routeId` | string | WorkGraph routing-decision identifier |
| `responsibilityId` | string | WorkGraph responsibility identifier |
| `issueContentVersion` | string | Normalized `lastEditedAt ?? createdAt` RFC 3339 instant |
| `agentProfile` | string | Must be in `allowedProfiles`; sent as `custom_agent` |
| `profileRef` | string | `"<agentProfile>@<blobSha>"` — see [preflight checks](#preflight-checks) |
| `requestedModel` | string | Must be in `allowedModels` |
| `fallbackModel` | string (optional) | Must be in `allowedModels` if present |
| `requiredEventType` | string | Must be `CompletedIssueValidation` |
| `baseRef` | string | Git ref the task runs against and the profile file is read from |
| `expectedProjectStatus` | string | See [preflight checks](#preflight-checks) — an adaptation, see below |

The reaction derives `subjectNodeId` from `issueNodeId`, generates stable
`executionId = "execution:<uuid-v5>"`, and generates
`expectedEventId = "event:<executionId>:<requiredEventType>"`. These values are not
query-row inputs.

> **Adaptation note:** `expectedProjectStatus` is not in the reaction's original field list
> but is required to make "relevant Project status expected by input" concrete — see
> [Known limitations](#known-limitations--integration-caveats).

## Configuration reference

| Field | Type | Default | Notes |
|---|---|---|---|
| `githubApiBaseUrl` | string | `https://api.github.com` | REST API base (use `https://GHE_HOST/api/v3` for GHE) |
| `githubGraphqlUrl` | string | `https://api.github.com/graphql` | GraphQL endpoint |
| `agentTasksApiVersion` | string | `2026-03-10` | Sent as `X-GitHub-Api-Version` |
| `token` | string (secret) | — | **Required.** Fine-grained PAT or GitHub App user token; see [Security](#security) |
| `expectedGithubUserId` | string | — | Optional numeric user ID; startup fails if `GET /user` reports a different token owner |
| `allowedRepositories` | string[] | — | **Required, non-empty** (fail-closed) |
| `allowedProfiles` | string[] | — | **Required, non-empty** |
| `allowedModels` | string[] | — | **Required, non-empty** |
| `requestTimeoutMs` | u64 | `30000` | Per-HTTP-request timeout |
| `commentApi.maxAttempts` | u32 | `3` | In-process retry attempts for the comment step within one run |
| `commentApi.retryBackoffMs` | u64 | `500` | Backoff between comment retry attempts |
| `strictRecovery` | bool | `true` | Must be `true` — see [Reservation, idempotency, and recovery](#reservation-idempotency-and-recovery) |
| `priorityQueueCapacity` | u64 | framework default | Optional reaction input queue capacity |

The Drasi Server reaction object is flat (there is no `config:` or `properties:`
wrapper); only `commentApi` is nested. Durable state-store wiring is server-wide:

```yaml
stateStore:
  kind: redb
  path: ./data/workgraph-state.redb

plugins:
  - ref: reaction/copilot-agent-task

reactions:
  - kind: copilot-agent-task
    id: copilot-launcher
    autoStart: true
    queries:
      - launch-issue-validation
    token: ${GITHUB_AGENT_TOKEN}
    expectedGithubUserId: ${TRUSTED_LAUNCHER_USER_ID}
    githubApiBaseUrl: https://api.github.com
    githubGraphqlUrl: https://api.github.com/graphql
    agentTasksApiVersion: "2026-03-10"
    allowedRepositories:
      - my-org/my-repo
    allowedProfiles:
      - issue-validator
    allowedModels:
      - gpt-5.6-sol
      - gpt-5.4
    requestTimeoutMs: 30000
    commentApi:
      maxAttempts: 3
      retryBackoffMs: 500
    strictRecovery: true
    priorityQueueCapacity: 10000
```

Generated dynamic-plugin schema name:
`reaction.copilot_agent_task.CopilotAgentTaskReactionConfig`, config version
`1.1.0`. Unknown fields are rejected.

In declarative (dynamic-plugin) config, `token` is a `ConfigValue<String>` and is expected to
be supplied as a `${ENV_VAR}` reference or a `{"kind":"Secret","name":"..."}` reference —
**never** a literal token in the config file.

## Preflight checks

Before every launch (and before every reconciled re-launch), the reaction re-verifies GitHub
state live, rejecting (permanently, fail-closed) if any of the following do not hold:

1. **Issue is open** — `GET /repos/{owner}/{repo}/issues/{number}`, `state == "open"`.
2. **`issueNodeId` matches the resolved issue** — the REST response's `node_id` must equal
   the row's `issueNodeId`, so a row cannot point its correlation IDs (and therefore the
   comment target) at an issue node ID unrelated to the `repository`/`issueNumber` it claims.
3. **Issue content unchanged** — the Project-item GraphQL lookup reads the linked Issue's
   `lastEditedAt` and `createdAt`. The reaction selects `lastEditedAt ?? createdAt` and
   requires it to represent the same RFC 3339 instant as `issueContentVersion`.
4. **Project identity and status unchanged** — a GraphQL query requires the item's parent
   Project node ID, owner login, and number to equal `projectNodeId`, `projectOwner`, and
   `projectNumber`, then requires its `Status` single-select value to equal
   `expectedProjectStatus`, which must be `AwaitingValidation` for this Phase 2 reaction.
5. **Project item is linked to this issue** — the same GraphQL query also reads the item's
   `content { ... on Issue { id } } ` and requires it to equal the row's `issueNodeId`, so
   `projectItemNodeId` cannot name an unrelated project item that merely happens to have a
   matching `Status` value.
6. **Agent profile pinned** — `profileRef` must be `<agentProfile>@<blobSha>`. The reaction
   derives `.github/agents/<agentProfile>.agent.md`, fetches it at `baseRef`, and requires the
   returned blob `sha` to equal the SHA encoded in `profileRef`.

## Reservation, idempotency, and recovery

Every launch attempt is tracked by a durable `ExecutionRecord`
(`execution:{routeId}:{responsibilityId}:{attempt}`, `attempt` is always `1` in this version)
written to the state store **before** any GitHub call:

```text
Reserved -> Starting -> Started (comment_posted=false) -> Started (comment_posted=true)
                \-> Ambiguous (creation outcome unknown; needs reconciliation)
                \-> Failed (permanent: validation/preflight rejected the row)
```

- **Duplicate delivery** (the same `routeId`/`responsibilityId` observed again, e.g. a
  duplicate upstream emission) is detected by looking up the existing record: `Started` with
  `comment_posted=true` is skipped entirely.
- **Crash / restart between reservation and confirmed creation** leaves a `Starting` record.
  On the next delivery of the same row, the reaction runs
  [reconciliation](#ambiguous-creation-and-reconciliation) before doing anything else — it
  never blindly retries task creation.
- The reaction's checkpoint (the query-outbox position) is advanced **only after** the task
  is confirmed created *and* the `workgraph.execution/v1` comment is recorded as posted.
  A transient failure at any step returns an error that stops
  the reaction (`ReactionRecoveryPolicy::Strict`) so the batch replays from the outbox on
  restart, without ever recreating an already-confirmed task.
- `strictRecovery` must be `true`: an ambiguous or failed launch always requires
  reconciliation or operator intervention, never a silent skip or auto-reset.

## Model fallback

`requestedModel` is tried first. If — and only if — the response is HTTP `422` **and** the
response body clearly names an unsupported-model condition (see
`github::is_unsupported_model_error`), the reaction retries **exactly once** with
`fallbackModel` (if configured, allowlisted, and different from `requestedModel`). Any other
`422`, or a `422` with no usable fallback, is a permanent failure — never retried.

## The prompt and `WorkGraphEvent/v1`

The `prompt` sent to the Agent Task supplies exactly one immutable target with these fields:
`eventId`, `projectItemNodeId`, `projectOwner`, `projectNumber`, `subjectType`,
`subjectNodeId`, `repository`, `number`, `actorType`, `actorId`, `routeId`,
`responsibilityId`, `executionId`, `contentVersion`, and `profileRef`. It also embeds the
literal `WorkGraphEvent/v1` JSON Schema (`schema/workgraph-event-v1.schema.json`) and the
exact `WorkGraphEvent/v1` marker plus fenced-JSON issue-comment format. The completion
event uses `contentVersion` and must be posted before any transition to `AwaitingRouting`.

## The `workgraph.execution/v1` comment

Immediately after a task is confirmed created, the reaction posts **one** GitHub issue
comment via the `addComment` GraphQL mutation, whose body is **pure JSON** (no markdown
fencing) matching `schema/workgraph-execution-v1.schema.json`. Its exact fields are
`schemaVersion`, `messageType`, `routeId`, `responsibilityId`, `executionId`,
`expectedEventId`, `requiredEventType`, `taskId`, `taskUrl`, `agentProfile`,
`profileRef`, `requestedModel`, `actualModel`, `state`, and `startedAt`.

An HTTP `200` response from GraphQL that carries a non-empty top-level `errors` array is
always treated as a **failure**, even though the GraphQL spec allows `data` and `errors` to
coexist — the reaction never treats a partial GraphQL response as success.

## Failure-state observability

The `workgraph.execution/v1` GitHub comment is success-only: it is posted only after GitHub
confirms that a task started. Failed or ambiguous reservations never write a success-shaped
issue comment.

After every durable `Failed` or `Ambiguous` execution-record write, the reaction emits one
single-line JSON log to the `workgraph.execution_state` log target. The body matches
`schema/workgraph-execution-state-v1.schema.json`:

```json
{
  "schema": "workgraph.execution-state/v1",
  "reactionId": "copilot-launcher",
  "executionId": "<stable execution ID>",
  "routeId": "<route ID>",
  "responsibilityId": "<responsibility ID>",
  "attempt": 1,
  "status": "failed",
  "repository": "owner/repo",
  "issueNumber": 123,
  "errorPresent": true,
  "observedAt": "2026-08-13T19:00:00Z"
}
```

`status` is exactly `ambiguous` or `failed`. The envelope deliberately excludes `lastError`,
token values, prompts, and GitHub response bodies. Dogfood monitors should collect structured
logs and filter on log target `workgraph.execution_state` or JSON field
`schema=workgraph.execution-state/v1`. `ComponentStatus::Error` remains the coarse health
surface for transient/ambiguous pipeline stops, while the durable `ExecutionRecord` remains
the restart/idempotency source of truth.

## Ambiguous creation and reconciliation

If the create-task HTTP call fails at the **transport** level (timeout, connection reset —
no HTTP response received at all), the outcome is unknown: the task may or may not have been
created. The reaction marks the record `Ambiguous` and, on the next processing pass, calls
`GitHubClient::reconcile`, which lists recent tasks
(`GET /agents/repos/{owner}/{repo}/tasks`) and searches for **exactly one** whose
prompt/body contains the attempt's `executionId`:

- **Zero matches** → stays `Ambiguous`; absence from a recent-task listing is not proof that
  creation did not succeed.
- **Exactly one match** → adopt it (mark `Started` with its task ID/URL) and proceed to the
  comment step.
- **More than one match** → stays `Ambiguous` and the reaction stops; it never guesses.

## Security

- `token` must be a **fine-grained personal access token** or a **GitHub App user-to-server
  token** scoped to Agent Tasks, issue read/comment, and Project read permissions on the
  repositories in `allowedRepositories`. Broad classic PATs are discouraged.
- The token is never logged, and never appears in any `Debug` output
  (`CopilotAgentTaskReactionConfig` and `GitHubClient` both redact it explicitly). It **is**
  included in `Reaction::properties()` — this is a framework contract (config persistence
  must be lossless so the reaction restarts correctly), not a log/display surface.
- Prompt text is never logged, since it may contain sensitive repository or issue context.
- Set `expectedGithubUserId` to the immutable numeric GitHub user ID trusted by the
  launch query (`TRUSTED_LAUNCHER_USER_ID`). Startup probes `GET /user` and fails closed if
  the token belongs to another identity. Logins remain display-only metadata.
- All input (`repository`, `agentProfile`, `requestedModel`, `fallbackModel`) is validated
  against explicit allowlists; empty allowlists allow nothing (fail-closed).

## Testing

This is a **protocol-target** reaction (it calls the GitHub REST/GraphQL API directly, with
no target system to run in a container). Tests use `wiremock` to stand in for GitHub and an
in-memory `StateStoreProvider` to stand in for durable state.

```sh
make test              # unit tests
make integration-test  # end-to-end tests (wiremock + DrasiLib), ignored by default
make lint              # clippy -D warnings + fmt --check
make schema            # verify schema/*.schema.json matches src/prompt.rs
make update-schema     # regenerate schema/*.schema.json from src/prompt.rs
```

Integration coverage (`tests/integration_test.rs`) includes: validation/fail-closed rejection,
the full success path, exactly-once model fallback, no-fallback on an unrelated `422`,
duplicate delivery, a crash/recovery boundary (pre-seeded `Starting` record + reconciliation),
ambiguous reconciliation (never guesses), GraphQL errors treated as failures (both for the
Project-status preflight query and for `addComment`), and secret redaction/wiring.

## Dynamic plugin build

```sh
cargo build -p drasi-reaction-copilot-agent-task --release --features dynamic-plugin
```

The plugin ID is `copilot-agent-task-reaction`. The macOS cdylib is
`libdrasi_reaction_copilot_agent_task.dylib`; Linux produces
`libdrasi_reaction_copilot_agent_task.so`.

## Known limitations / integration caveats

- **The GitHub "Agent Tasks" API is a preview/evolving surface.** This reaction implements
  the shape given in its requirements (`custom_agent`, `model`, `prompt`, `base_ref`,
  `create_pull_request` on `POST /agents/repos/{owner}/{repo}/tasks`) and makes documented,
  best-effort adaptations for the parts not pinned down publicly — see the module docs in
  `src/github.rs` for specifics (task-listing endpoint shape, "clearly unsupported model"
  detection heuristic, and the transport-error-only definition of "ambiguous").
- **`issueContentVersion` is the canonical issue edit instant.** Producers must supply the
  normalized UTC RFC 3339 value of `lastEditedAt ?? createdAt`.
- **`expectedProjectStatus` is an added field** beyond the reaction's literal requirements
  list, needed to make the "Project status expected by input" preflight check concrete — see
  [Launch row schema](#launch-row-schema).
- **`profileRef` is `"<agentProfile>@<blobSha>"`.** The repository path is derived as
  `.github/agents/<agentProfile>.agent.md`.
- **Attempts are always `attempt=1`** in this version — the state key includes an `attempt`
  number for forward compatibility, but there is no multi-attempt retry loop yet. A
  permanently failed attempt requires a new row (e.g. a new `routeId`) to relaunch.
- **Reconciliation's "exact match" is a substring search** for `executionId` in a task's
  prompt/body. If GitHub's real Agent Tasks API does not surface the prompt on the listing
  endpoint, this seam needs a metadata-based match instead — the ambiguous/no-match/adopt
  state machine in `src/state.rs` and `src/reaction.rs` is unaffected by that change.
- **Standalone publication still depends on TODO #574.** The workspace build and cdylib are
  ready, but publishing this reaction independently requires replacing workspace
  `drasi-lib` and `drasi-plugin-sdk` dependencies with versions compatible with the target
  dynamic loader.
