# Drasi Copilot Agent Task Reaction

`drasi-reaction-copilot-agent-task` is a Drasi reaction plugin ([`drasi-lib`](../../../lib))
that launches GitHub Copilot coding-agent tasks for assigned WorkGraph runs. It is the
first of the two in-repo WorkGraph reactions:

```text
reaction/http       -> ResponsibilityAssigned    -> Project status AwaitingValidation
copilot-agent-task  -> ExecutionStarted
issue-validator     -> CompletedIssueValidation     (posted by an external JS reporter)
workgraph-router    -> RoutingDecided             -> AwaitingIssueRiskProfiling | NeedsMoreInformation
```

The assignment step is the generic [`reaction/http`](../http) reaction driven by a
query, so only this reaction and [`workgraph-router`](../workgraph-router) are
WorkGraph-specific components.

Every event is the shared `WorkGraphEvent/v1` format defined by
[`drasi-workgraph-common`](../../workgraph-common). This reaction subscribes to a single
continuous query — the **launch query** — and, for every row newly **added** to that query's
result set, it:

1. Validates the row against configured allowlists (fail-closed).
2. Re-reads the **authoritative issue** from GitHub and binds the run to the current body
   digest — a body edited since the assignment aborts with zero side effects.
3. Verifies the **Project item** binding and that its status is exactly `AwaitingValidation`.
4. Adopts the **trusted `ResponsibilityAssigned` assignment** comment (trusted author user ID,
   unedited, coalesced by deterministic event ID) and requires its content digest to match.
5. Pins the exact **agent-profile blob** named by the assignment's `profileRef`.
6. Durably **reserves** the run before any external write.
7. **Creates or adopts exactly one** agent task (with one fallback-model retry on a clearly
   unsupported-model `422`).
8. Posts **exactly one** shared `ExecutionStarted` `WorkGraphEvent/v1` issue comment.

The launch row only *nominates* a run: everything the reaction trusts is re-read from GitHub
before any write. `Update` and `Delete` diffs (and aggregation/no-op results) are never acted
on — only rows newly added to the query's result set can trigger a launch.

---

## Table of contents

1. [Quick start](#quick-start)
2. [Launch row schema](#launch-row-schema)
3. [Configuration reference](#configuration-reference)
4. [The launch flow](#the-launch-flow)
5. [Reservation, idempotency, and recovery](#reservation-idempotency-and-recovery)
6. [Model fallback](#model-fallback)
7. [The reporter prompt](#the-reporter-prompt)
8. [The `ExecutionStarted` comment](#the-executionstarted-comment)
9. [Failure-state observability](#failure-state-observability)
10. [Ambiguous creation and reconciliation](#ambiguous-creation-and-reconciliation)
11. [Security](#security)
12. [Testing](#testing)
13. [Dynamic plugin build](#dynamic-plugin-build)
14. [Known limitations](#known-limitations)

---

## Quick start

```rust
use drasi_reaction_copilot_agent_task::{ActorType, CopilotAgentTaskReaction};

let reaction = CopilotAgentTaskReaction::builder("copilot-launcher")
    .with_query("launch-query")
    .with_token(std::env::var("GITHUB_TOKEN").unwrap())
    .with_allowed_repositories(vec!["my-org/my-repo".to_string()])
    .with_allowed_profiles(vec!["issue-validator".to_string()])
    .with_allowed_models(vec!["gpt-5.6-sol".to_string(), "gpt-5.4".to_string()])
    .with_trusted_assignment_author_database_id(4_021_243)
    .with_trusted_assignment_author_type(ActorType::Bot)
    .with_trusted_execution_author_database_id(90_210)
    .with_trusted_execution_author_type(ActorType::Bot)
    .with_expected_project_status_field_node_id("PVTSSF_examplestatusfield")
    .build()?;
```

The reaction requires a **durable** `StateStoreProvider` (`is_durable() == true`) — reservation
and execution records must survive restarts. Configure one on `DrasiLib::builder()`
(e.g. `drasi-state-store-redb`); the reaction fails to start otherwise.

## Launch row schema

Each row **is** one authoritative `ResponsibilityAssigned` comment, exactly as the GitHub
Source projected it. The reaction never scans an issue thread for "something that looks like an
assignment": the row names the event, and the shared
[`accept_event_row`](../../workgraph-common/src/row.rs) seam proves it.

Rows must contain **exactly** the following fields (camelCase, as `RETURN ... AS <name>`
aliases in Cypher). Unknown fields are rejected (`deny_unknown_fields`).

| Field | Source origin | Type | Notes |
|---|---|---|---|
| `repository` | `GitHubIssue.repositoryNameWithOwner` | string | `"owner/repo"`; must be in `allowedRepositories` |
| `subjectNumber` | `GitHubIssue.number` | integer | Subject issue number (`> 0`) |
| `subjectNodeId` | `GitHubIssue` node ID | string | GitHub issue node ID (`I_…`) |
| `projectNodeId` | `GitHubProject` node ID | string | GitHub Projects (v2) node ID (`PVT_…`) |
| `projectItemNodeId` | `GitHubProjectItem` node ID | string | GitHub Projects (v2) item node ID (`PVTI_…`) |
| `projectStatus` | `GitHubProjectItem.statusName` | string | Must equal `AwaitingValidation` |
| **`bodyDigest`** | `GitHubIssue.bodyDigest` | string | **Exact Source field name.** `sha256:<64-hex>` of the subject issue body |
| `eventCommentNodeId` | `GitHubIssueComment` node ID | string | The comment carrying the assignment |
| `eventBody` | `GitHubIssueComment.body` | string | The strict `WorkGraphEvent/v1` comment body |
| **`authorDatabaseId`** | `GitHubIssueComment.authorDatabaseId` | integer | **Exact Source field name.** Half the trust key |
| **`authorType`** | `GitHubIssueComment.authorType` | string | **Exact Source field name.** `User` / `Bot` / `Organization` |
| **`isEdited`** | `GitHubIssueComment.isEdited` | boolean | **Exact Source field name.** Must be `false` |
| `requestedModel` | query policy | string | Must be in `allowedModels` |
| `fallbackModel` | query policy | string (optional) | If present, must be in `allowedModels` and differ from `requestedModel` |
| `baseRef` | query policy | string | Git ref the task runs against and the profile blob is read from |

There is **no `runId` row field**. The run is derived from the row's own binding —
`run_id(projectItemNodeId, bodyDigest)` — and the assignment event must name
exactly that run, so a row can never nominate a run its binding does not produce. `bodyDigest`
is the *issue* body digest because that is the only `bodyDigest` the Source contract defines
(it is projected on `GitHubIssue`/`GitHubPullRequest`, never on a comment node), and it is the
same value that binds the run.

The reaction still re-reads the issue before any write and requires the **current** digest to
equal the row's `bodyDigest`, so a body edited since the row was emitted aborts the launch with
zero side effects rather than proceeding on stale information.

The `executionId` (`execution:<runId>`) is derived deterministically from the run alone — it
is not a query-row input, and there is exactly one execution per run.

A launch query therefore looks like:

```cypher
MATCH (c:GitHubIssueComment)-[:COMMENT_ON]->(i:GitHubIssue),
      (pi:GitHubProjectItem)-[:TRACKS]->(i),
      (pi)-[:IN_PROJECT]->(p:GitHubProject)
WHERE pi.statusName = 'AwaitingValidation' AND c.isEdited = false
RETURN i.repositoryNameWithOwner AS repository, i.number AS subjectNumber,
       elementId(i) AS subjectNodeId, elementId(p) AS projectNodeId,
       elementId(pi) AS projectItemNodeId, pi.statusName AS projectStatus,
       i.bodyDigest AS bodyDigest, elementId(c) AS eventCommentNodeId,
       c.body AS eventBody, c.authorDatabaseId AS authorDatabaseId,
       c.authorType AS authorType, c.isEdited AS isEdited,
       'gpt-5.6-sol' AS requestedModel, 'main' AS baseRef
```

## Configuration reference

| Field | Type | Default | Notes |
|---|---|---|---|
| `githubApiBaseUrl` | string | `https://api.github.com` | REST API base (use `https://GHE_HOST/api/v3` for GHE) |
| `githubGraphqlUrl` | string | `https://api.github.com/graphql` | GraphQL endpoint |
| `agentTasksApiVersion` | string | `2026-03-10` | Sent as `X-GitHub-Api-Version` |
| `token` | string (secret) | — | **Required.** Fine-grained PAT or GitHub App user token; see [Security](#security) |
| `expectedGithubUserId` | string | — | Optional numeric user ID; startup fails if `GET /user` reports a different token owner. When set it must equal `trustedExecutionAuthorDatabaseId` — both name this reaction's own account |
| `allowedRepositories` | string[] | — | **Required, non-empty** (fail-closed) |
| `allowedProfiles` | string[] | — | **Required, non-empty** |
| `allowedModels` | string[] | — | **Required, non-empty** |
| `trustedAssignmentAuthorDatabaseId` | u64 | — | **Required, > 0.** Numeric GitHub database ID whose `ResponsibilityAssigned` comments are trusted (the assigning reaction's identity) — see [Author trust](#author-trust) |
| `trustedAssignmentAuthorType` | string | `Bot` | `User`, `Bot`, or `Organization` — the other half of the assignment trust key |
| `trustedExecutionAuthorDatabaseId` | u64 | — | **Required, > 0.** Numeric GitHub database ID **this** reaction posts as, used only to adopt its own `ExecutionStarted` comment. Must be the account `token` authenticates as |
| `trustedExecutionAuthorType` | string | `Bot` | `User`, `Bot`, or `Organization` — the other half of the execution trust key |
| `expectedProjectStatusFieldNodeId` | string | — | **Required, `PVTSSF_` prefix.** The Project single-select status field the item's status must be read from |
| `requestTimeoutMs` | u64 | `30000` | Per-HTTP-request timeout |
| `commentApi.maxAttempts` | u32 | `3` | Authoritative reconciliation reads after an ambiguous task or comment write |
| `commentApi.retryBackoffMs` | u64 | `500` | Backoff between authoritative reconciliation reads |
| `strictRecovery` | bool | `true` | Must be `true` — see [Reservation, idempotency, and recovery](#reservation-idempotency-and-recovery) |
| `priorityQueueCapacity` | u64 | framework default | Optional reaction input queue capacity |

Non-`https` endpoints are rejected except for loopback hosts (`localhost`/`127.0.0.1`/`::1`),
and URLs that embed credentials are rejected outright.

The Drasi Server reaction object is flat (there is no `config:` or `properties:` wrapper);
only `commentApi` is nested. Durable state-store wiring is server-wide:

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
    trustedAssignmentAuthorDatabaseId: 4021243
    trustedAssignmentAuthorType: Bot
    trustedExecutionAuthorDatabaseId: 90210
    trustedExecutionAuthorType: Bot
    expectedProjectStatusFieldNodeId: PVTSSF_examplestatusfield
    requestTimeoutMs: 30000
    commentApi:
      maxAttempts: 3
      retryBackoffMs: 500
    strictRecovery: true
    priorityQueueCapacity: 10000
```

Generated dynamic-plugin schema name:
`reaction.copilot_agent_task.CopilotAgentTaskReactionConfig`, config version `2.0.0`.
Unknown fields are rejected.

In declarative (dynamic-plugin) config, `token` is a `ConfigValue<String>` and is expected to
be supplied as a `${ENV_VAR}` reference or a `{"kind":"Secret","name":"..."}` reference —
**never** a literal token in the config file.

## The launch flow

For each added row the reaction executes the following steps in order. Every semantic mismatch
is a **permanent** rejection (logged and skipped, the reaction stays healthy); transient and
ambiguous failures stop the reaction so the batch replays on restart.

1. **Validate** the row against the allowlists and the frozen identifier grammar, then
   **accept its assignment event**: `isEdited` must be `false`, `authorDatabaseId` +
   `authorType` must be exactly the configured trusted assignment identity, `eventBody` must
   parse under the strict `WorkGraphEvent/v1` grammar into a `ResponsibilityAssigned`, and that
   event must name this row's item, subject, and `run_id(projectItemNodeId, bodyDigest)`.
2. **Read the authoritative issue** — `GET /repos/{owner}/{repo}/issues/{number}`; require
   `state == "open"`, `node_id == subjectNodeId`, and `body_digest(body) == bodyDigest`. A
   mismatch means the issue body changed since the assignment and aborts with **zero side
   effects**. The assignment's `contentDigest` must equal that digest too.
3. **Verify the Project item** (GraphQL) — the item belongs to `projectNodeId`, its content is
   the expected Issue (node ID + number + repository), its status field node ID equals
   `expectedProjectStatusFieldNodeId`, and its current status is exactly `AwaitingValidation`.
4. **Coalesce every trusted assignment** — the named `eventCommentNodeId` must exist, be
   unedited, and carry the row's exact canonical event. Every other trusted, unedited comment
   claiming that deterministic `eventId` is coalesced too: identical duplicates are harmless,
   while any conflicting body or payload fails closed.
5. **Pin the profile** — `GET /repos/{owner}/{repo}/contents/.github/agents/{profile}.agent.md?ref={baseRef}`
   where `{profile}` comes from the assignment's `profileRef`; require the returned blob `sha`
   to equal the SHA the `profileRef` pins, and require `{profile}` to be in `allowedProfiles`.
6. **Reserve** — compute `executionId` from the run, then durably create-if-absent an
   `ExecutionRecord` keyed by `execution:{runId}` **before** any external write. An existing
   record must describe the same run/repository/subject/item; the reaction resumes from its
   state.
7. **Create or adopt one task** — reconcile first (adopt the single task whose prompt carries
   the `executionId`; ≥2 matches fail closed), otherwise `POST /agents/repos/{owner}/{repo}/tasks`.
8. **Post one `ExecutionStarted` comment** — adopting a prior write only when its canonical
   event JSON is byte-identical to the intended event, so an ambiguous write is never
   duplicated and a divergent comment claiming that event ID fails closed.

## Reservation, idempotency, and recovery

Every run is tracked by a durable `ExecutionRecord` keyed by `execution:{runId}`, written to
the state store **before** any GitHub write:

```text
Reserved -> TaskCreated -> Completed
     \-> Ambiguous (a write's outcome is unknown; needs reconciliation)
     \-> Failed    (a permanent create-task rejection; terminal)
```

- **Exactly one execution per run.** The `executionId` is a pure function of `runId`, so a
  duplicate delivery of the same row resolves to the same record and creates no second task or
  comment.
- **Duplicate delivery** of a `Completed` (or `Failed`) run is skipped entirely.
- **Crash / restart** between reservation and a confirmed write leaves a `Reserved` or
  `Ambiguous` record. On the next delivery that deterministic record is loaded before mutable
  issue, status, profile, or assignment checks. Recovery uses its pinned repository, subject,
  profile, model, ref, task, and comment intent, then
  [reconciles](#ambiguous-creation-and-reconciliation) against GitHub.
- Every record mutation is an exact-bytes compare-and-swap, so a stale in-memory copy can never
  clobber newer progress.
- The reaction's checkpoint (the query-outbox position) is advanced **only after** a run is
  fully processed. A transient failure returns an error that stops the reaction
  (`ReactionRecoveryPolicy::Strict`) so the batch replays from the outbox on restart, without
  ever recreating an already-confirmed task or comment.
- `strictRecovery` must be `true`.

## Model fallback

`requestedModel` is tried first. If — and only if — the response is HTTP `422` **and** the
response body clearly names an unsupported-model condition (see
`github::is_unsupported_model_error`), the reaction retries **exactly once** with
`fallbackModel` (if configured). The chosen model is persisted **before** each attempt. Any
other `422`, or a `422` with no usable fallback, is a permanent failure — never retried.

## The reporter prompt

The `prompt` sent to the Agent Task hands the issue-validator's scoped
`workgraph/report_completion` tool **exactly two** values: `subjectNumber` and `executionId`.
It instructs the agent to call that reporter exactly once with only those two arguments, and
states that every other correlation value (`runId`, `projectItemNodeId`, `subjectNodeId`,
`eventId`) is derived by the reporter from the trusted WorkGraph comments on the issue, never
from the prompt — so a tampered prompt cannot redirect or forge a completion event. Prompt text
is **never logged**.

## The `ExecutionStarted` comment

Immediately after a task is confirmed created, the reaction posts **one** GitHub issue comment
whose body is the shared `WorkGraphEvent/v1` comment format rendered by
`drasi_workgraph_common::comment::render_comment`:

```text
WorkGraphEvent/v1

<one summary line>

<one raw JSON WorkGraphEvent object>
```

The event is
`WorkGraphEvent::new(runId, projectItemNodeId, subjectNodeId, ExecutionStarted { executionId, taskId })`;
its `eventId` is derived deterministically, and its envelope carries only the canonical
`schemaVersion, eventId, eventType, runId, projectItemNodeId, subjectNodeId, payload` keys. This
is the **only** comment this reaction writes — there is no separate `workgraph.execution/v1`
comment shape, and this reaction never mutates the Project status (the router does that later).

## Failure-state observability

The `ExecutionStarted` GitHub comment is success-only. Failed or ambiguous reservations never
write an issue comment. Operational telemetry stays in logs and durable state.

After every durable `Failed` or `Ambiguous` execution-record write, the reaction emits one
single-line JSON log to the `workgraph.execution_state` log target. The body matches
`schema/workgraph-execution-state-v1.schema.json`:

```json
{
  "schema": "workgraph.execution-state/v1",
  "reactionId": "copilot-launcher",
  "executionId": "execution:validation:PVTI_example:sha256:<64-hex>",
  "runId": "validation:PVTI_example:sha256:<64-hex>",
  "status": "failed",
  "repository": "owner/repo",
  "issueNumber": 123,
  "errorPresent": true,
  "observedAt": "2026-08-13T19:00:00Z"
}
```

`status` is exactly `ambiguous` or `failed`. The envelope deliberately excludes `lastError`,
token values, prompts, and GitHub response bodies. Monitors should collect structured logs and
filter on log target `workgraph.execution_state` or JSON field
`schema=workgraph.execution-state/v1`. `ComponentStatus::Error` remains the coarse health
surface for transient/ambiguous pipeline stops, while the durable `ExecutionRecord` remains the
restart/idempotency source of truth.

## Ambiguous creation and reconciliation

If the create-task HTTP call fails at the **transport** level (timeout, connection reset — no
HTTP response received at all), the outcome is unknown: the task may or may not have been
created. The reaction marks the record `Ambiguous` and, on the next processing pass, calls
`GitHubClient::reconcile`, which lists recent tasks
(`GET /agents/repos/{owner}/{repo}/tasks`) and searches for **exactly one** whose prompt/body
contains the run's `executionId`:

- **Zero matches on a new, never-sent intent** → create the task once.
- **Zero matches after an ambiguous write** → repeat authoritative reads with backoff, then fail
  stopped while retaining the record; one zero-match list is never proof of absence and creation
  is never retried.
- **Exactly one match** → adopt it (record its task ID/URL) and proceed to the comment step.
- **More than one match** → fail closed and stop; it never guesses.

The `ExecutionStarted` comment step persists ambiguity before sending. A lost response therefore
enters read-only reconciliation: repeated listings may adopt the exact landed comment, but no
subsequent attempt can recreate it.

## Security

- `token` must be a **fine-grained personal access token** or a **GitHub App user-to-server
  token** scoped to Agent Tasks, issue read/comment, and Project read permissions on the
  repositories in `allowedRepositories`. Broad classic PATs are discouraged.
- The token is never logged, and never appears in any `Debug` output
  (`CopilotAgentTaskReactionConfig`, `GitHubConfig`, and `GitHubClient` all redact it as
  `[REDACTED]`). It **is** included in `Reaction::properties()` — a framework contract (config
  persistence must be lossless so the reaction restarts correctly), not a log/display surface.
- Prompt text is never logged, since it may contain sensitive repository or issue context.
- Set `expectedGithubUserId` to the immutable numeric GitHub user ID of the launcher token.
  Startup probes `GET /user` and fails closed if the token belongs to another identity. It must
  match `trustedExecutionAuthorDatabaseId`; configuration that disagrees is rejected, because a
  reaction that cannot recognise its own comments would post duplicates.
- **Trust is by numeric database ID + actor type, never login and never node ID.** Only
  `ResponsibilityAssigned` comments authored by `trustedAssignmentAuthorDatabaseId` +
  `trustedAssignmentAuthorType` and reported unedited by GitHub are read, and only an
  `ExecutionStarted` comment authored by `trustedExecutionAuthorDatabaseId` +
  `trustedExecutionAuthorType` whose canonical event JSON is byte-identical to the event this
  reaction intends to post can be adopted. See [Author trust](#author-trust).
- All input (`repository`, `requestedModel`, `fallbackModel`) is validated against explicit
  allowlists; empty allowlists allow nothing (fail-closed).

## Author trust

The authoritative GitHub Source projects four comment author fields, and this
reaction maps them as follows:

| Source field | Role |
|---|---|
| `authorDatabaseId` | compared against the configured `…AuthorDatabaseId` |
| `authorType` | compared against the configured `…AuthorType` |
| `authorId` (node ID) | **audit data only** — never configured, never compared |
| `authorLogin` | **display only** — never compared |

Two roles are configured separately, because the account that writes assignments
and the account this reaction posts as are usually different:

| Role | Configuration | Used for |
|---|---|---|
| assignment | `trustedAssignmentAuthorDatabaseId` + `trustedAssignmentAuthorType` | reading the `ResponsibilityAssigned` comment |
| execution | `trustedExecutionAuthorDatabaseId` + `trustedExecutionAuthorType` | adopting this reaction's own `ExecutionStarted` comment |

Both values of a role must match for a comment to be trusted in that role.

- A **login is display-only** and is never compared: logins can be renamed by
  their owner and the freed name reclaimed by someone else.
- The **node ID is not configured.** It is carried when the Source reports it so
  logs and errors can cite the exact account, and its absence never blocks trust.
- **No GitHub App attribution is involved.** The authoritative Source does not
  expose one for the comment and review nodes this workflow consumes, so
  requiring it would either fail closed on every real event or invite a
  non-authoritative substitute.
- **Known limitation:** every token that authenticates as one GitHub identity —
  a personal access token, a second PAT for the same account, or a GitHub App
  user-to-server token acting as that account — reports the *identical*
  `authorDatabaseId` and `authorType`. In this prototype such tokens are **not
  separately attributable**, and the reaction cannot distinguish a comment it
  wrote itself from one written by any other token holding the same identity. A
  trusted identity must therefore be a dedicated automation account whose
  credentials are not shared with humans or unrelated automation.

The contract, and the single mapping seams from authoritative GitHub REST author
metadata and from Source query rows onto it, live in
[`components/workgraph-common`](../../workgraph-common) (`trust` module).

## Testing

This is a **protocol-target** reaction (it calls the GitHub REST/GraphQL API directly, with no
target system to run in a container). Tests use `wiremock` to stand in for GitHub and an
in-memory durable `StateStoreProvider` to stand in for the persistent store. A restart is
modelled by pre-seeding the durable record and GitHub state into a fresh core, exactly as the
durability contract defines it.

```sh
make test              # unit tests
make integration-test  # end-to-end tests (wiremock + DrasiLib), ignored by default
make lint              # clippy -D warnings + fmt --check
make schema            # verify schema/workgraph-execution-state-v1.schema.json matches src/state.rs
make update-schema     # regenerate schema/workgraph-execution-state-v1.schema.json from src/state.rs
```

Integration coverage (`tests/integration_test.rs`) includes: the full happy path (exactly one
task and one canonical `ExecutionStarted` comment whose body is computed independently in the
test), the two-field reporter prompt, duplicate delivery (one task + one comment), a current
issue body whose digest no longer matches the row's `bodyDigest` (zero side effects), a row
naming a comment that no longer exists (zero side effects), rows whose Source
`authorDatabaseId`/`authorType` are not the trusted assignment identity — including this
reaction's own identity — while a perfectly good assignment sits on the issue (zero side
effects), an edited assignment reported either by the row's `isEdited` or only by GitHub (zero
side effects), a named comment whose content diverges from the row's event (zero side effects),
profile-blob SHA drift (zero side effects), a Project status other than `AwaitingValidation`
(zero side effects), exactly-once model fallback on an unsupported-model `422` and no fallback
on an unrelated `422`, ambiguous task creation (persists `Ambiguous`, posts no comment) with a
restart that adopts the single correlated task and posts one comment, a pre-existing
`ExecutionStarted` comment that claims this run's event ID with a different payload (never
adopted: zero task writes, zero comment writes, the run is not completed), and token redaction
in `Debug` output.

Row-level acceptance itself (author trust, `isEdited`, `bodyDigest`/run binding, event type,
subject/item binding, and legacy bodies) is unit-tested in `src/row.rs`, over the single shared
implementation in [`drasi-workgraph-common`](../../workgraph-common/src/row.rs).

## Dynamic plugin build

```sh
cargo build -p drasi-reaction-copilot-agent-task --release --features dynamic-plugin
```

The macOS cdylib is `libdrasi_reaction_copilot_agent_task.dylib`; Linux produces
`libdrasi_reaction_copilot_agent_task.so`.

## Known limitations

- **The GitHub "Agent Tasks" API is a preview/evolving surface.** This reaction implements the
  shape given in its requirements (`custom_agent`, `model`, `prompt`, `base_ref`,
  `create_pull_request` on `POST /agents/repos/{owner}/{repo}/tasks`) and makes documented,
  best-effort adaptations for the parts not pinned down publicly — see the module docs in
  `src/github.rs` for specifics (task-listing endpoint shape, "clearly unsupported model"
  detection heuristic, and the transport-error-only definition of "ambiguous").
- **Reconciliation's "exact match" is a substring search** for `executionId` in a task's
  prompt/body. If GitHub's real Agent Tasks API does not surface the prompt on the listing
  endpoint, this seam needs a metadata-based match instead — the ambiguous/no-match/adopt state
  machine in `src/state.rs` and `src/reaction.rs` is unaffected by that change.
- **Standalone publication still depends on TODO #574.** The workspace build and cdylib are
  ready, but publishing this reaction independently requires replacing workspace `drasi-lib`
  and `drasi-plugin-sdk` dependencies with versions compatible with the target dynamic loader.
