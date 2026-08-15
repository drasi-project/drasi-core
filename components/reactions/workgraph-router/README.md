# WorkGraph Router Reaction

`drasi-reaction-workgraph-router` is the last step of the minimal WorkGraph
workflow. It routes **directly** from a trusted `CompletedIssueValidation`
comment while the Project Item is still at `AwaitingValidation`.

```text
admission          -> ResponsibilityAssigned  -> status AwaitingValidation
copilot-agent-task -> ExecutionStarted
issue-validator    -> CompletedIssueValidation
workgraph-router   -> RoutingDecided          -> AwaitingIssueRiskProfiling
                                                 or NeedsMoreInformation
```

The event format, the deterministic `runId`/`eventId` algorithms, the outer
comment grammar, and the author-trust contract all live in
[`components/workgraph-common`](../../workgraph-common).

## What it writes

Exactly one comment, in the exact shared grammar:

```text
WorkGraphEvent/v1

WorkGraph routed drasi-project/drasi-core#742 to AwaitingIssueRiskProfiling

{"schemaVersion":"workgraph.event/v1","eventId":"event:…","eventType":"RoutingDecided","runId":"run:…","projectItemNodeId":"PVTI_…","subjectNodeId":"I_…","payload":{"fromStatus":"AwaitingValidation","toStatus":"AwaitingIssueRiskProfiling","nextResponsibilityType":"issue-risk-profiling"}}
```

…followed by exactly one `updateProjectV2ItemFieldValue` mutation setting the
Project status to the **final** destination.

There is no intermediate `AwaitingRouting` status and no fifth
"next responsibility assigned" event: the next responsibility travels inside the
`RoutingDecided` payload.

## Decision table

The routing table is fixed by the event contract, not by configuration:

| Completion outcome | `toStatus` | `nextResponsibilityType` |
|---|---|---|
| `passed` | `AwaitingIssueRiskProfiling` | `issue-risk-profiling` |
| `failed` | `NeedsMoreInformation` | `issue-correction` |

## Row contract

Rows added to the single subscribed query must deserialize into
`RoutingCandidate` (unknown fields are rejected):

| Field | Type | Notes |
|---|---|---|
| `repository` | string | `owner/repo`, must be allowlisted |
| `subjectNumber` | integer | issue number |
| `subjectNodeId` | string | `I_…`; re-verified against GitHub |
| `projectNodeId` | string | `PVT_…`, must be allowlisted |
| `projectItemNodeId` | string | `PVTI_…` |
| `projectStatus` | string | must equal `AwaitingValidation` |

`Update` and `Delete` diffs are ignored — only rows newly added to the result set
can trigger routing.

A row only *nominates* an item. It carries **no** outcome, event ID, or author:
the decision comes solely from the trusted comments on the issue. Everything is
re-read from GitHub before any write.

## What must be true before the router writes anything

1. **Current body.** The authoritative issue body is re-read and digested; the
   `runId` derives from (Project Item, subject, body digest). An issue edited
   since validation therefore has a different `runId`, so its chain does not
   exist and the run produces zero side effects.
2. **Binding.** The Project item must belong to the allowlisted project, its
   content must be the expected issue (node ID, number, repository), and the
   status field must be the pinned `PVTSSF_…` node.
3. **Status.** The item must still be at `AwaitingValidation`. An item already
   at a decided status is finished *only* by the durable post-publication resume
   below, never by re-deriving a decision.
4. **Trusted authorship.** Only comments authored by the configured trusted
   author — numeric database ID **and** actor type — and that GitHub reports as
   never edited are considered at all.
5. **A complete chain for that exact run:** `ResponsibilityAssigned` (naming
   `expectedProfile`, carrying the current body digest) → `ExecutionStarted` →
   `CompletedIssueValidation` carrying the *same* `executionId`.
6. **Exactly one accepted completion.** Byte-identical duplicates coalesce to the
   earliest physical comment; two comments claiming the same `eventId` with
   different content fail closed with zero writes.

## Configuration

| Field | Type | Default | Notes |
|---|---|---|---|
| `githubRestUrl` | string | `https://api.github.com` | https only (http allowed for loopback test endpoints) |
| `githubGraphqlUrl` | string | `https://api.github.com/graphql` | same restriction |
| `githubTokenEnv` | string | `GITHUB_TOKEN` | env var holding the token |
| `allowedRepositories` | string[] | — | **required, non-empty** (fail-closed) |
| `allowedProjects` | string[] | — | **required, non-empty**, each `PVT_…` |
| `projectStatusFieldName` | string | `Status` | single-select field name |
| `expectedProjectStatusFieldNodeId` | string | — | **required**, `PVTSSF_…` |
| `expectedProfile` | string | `issue-validator` | the profile the assignment must name |
| `trustedAuthorDatabaseId` | u64 | — | **required, > 0**, numeric GitHub database ID whose comments are trusted (and which this reaction posts as); see below |
| `trustedAuthorType` | string | `Bot` | `User`, `Bot`, or `Organization` — the other half of the trust key |
| `timeoutSecs` | u64 | `30` | per-request timeout |
| `strictRecovery` | bool | `true` | must remain `true` |

Configured trust is exactly two values:

```yaml
trustedAuthorDatabaseId: 4021243
trustedAuthorType: Bot
```

`trustedAuthorType` is one of `User`, `Bot`, `Organization`. The identity this
reaction posts as must be this identity, so it can adopt its own decision
comment after an ambiguous write.

Config schema name `reaction.workgraph_router.WorkgraphRouterReactionConfig`,
config version `1.0.0`. Unknown fields are rejected.

### Author trust

The authoritative GitHub Source projects four comment author fields, mapped as
follows:

| Source field | Role |
|---|---|
| `authorDatabaseId` | compared against `trustedAuthorDatabaseId` |
| `authorType` | compared against `trustedAuthorType` |
| `authorId` (node ID) | **audit data only** — never configured, never compared |
| `authorLogin` | **display only** — never compared |

- Both trust values must match. A login is **display-only** and is never
  compared: logins can be renamed and the freed name reclaimed.
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
  separately attributable**, so a trusted identity must be a dedicated automation
  account whose credentials are not shared.

### Narrowness

The reaction cannot be configured into a general-purpose GitHub client:

- both GraphQL documents are compile-time constants — there is no request
  template and no configurable mutation;
- the routing table is fixed by the event contract, so configuration cannot
  introduce a new destination status or responsibility;
- endpoints must be `https` (or loopback) and may not embed credentials;
- the only write verbs are "create one issue comment" and "set one single-select
  status option"; and
- empty allowlists allow nothing.

The router never reads repository contents: the agent profile blob was already
pinned and verified at assignment and at launch, so re-pinning it here would only
let an unrelated later edit to the profile file wedge a completed validation.

## Durability and recovery

- `is_durable() = true`, `needs_snapshot_on_fresh_start() = false`,
  `default_recovery_policy() = Strict`, `checkpoint_ownership() = Reaction`.
- The durable record is keyed by `runId` and written **before** the first GitHub
  write, so a record's existence means "an external effect may already have
  happened" and recovery reconciles rather than retrying blindly.
- **Before publication**, the record pins the physical completion comment it
  decided from **and the SHA-256 of that exact accepted body**. A resumed run
  re-derives both; if the completion has been edited, or a different comment now
  claims the completion, or the derived decision differs, the run halts instead
  of routing.
- **After publication**, the remaining status move is finished from the durable
  record alone. The record also pins the canonical JSON of the decision it
  published and an *open-run pointer* from the Project item to that run, so a
  replay finds the decision even when the issue body (and therefore a freshly
  derived `runId`) changed afterwards. A decision that is already visible in the
  issue thread is never stranded by a later edit of its inputs.
  The published decision comment itself is still reconciled: it must exist, be
  authored by the trusted author, be unedited, still name the recorded
  run/item/subject/event ID, and carry exactly the pinned canonical event JSON.
  If it was edited, deleted, or replaced, the run **halts as a hard error** with
  zero side effects — it is never skipped as a permanent rejection.
- Each side effect is persisted with an exact-bytes compare-and-swap before the
  next one starts, so a stale writer can never clobber newer progress.
- A failed or unconfirmed write marks the record ambiguous and stops the
  reaction without advancing the checkpoint.
- On the next attempt the reaction adopts an existing `RoutingDecided` comment
  only when it is authored by the trusted author, is unedited, and its canonical
  event JSON — envelope *and* payload — is byte-identical to the decision this
  run intends to publish. `eventId` covers the run and the event type only, so a
  *single* pre-existing comment claiming that `eventId` with different content
  fails closed rather than being adopted.
- The status mutation tolerates "already at the destination", which is what makes
  a retry after an ambiguous mutation safe.
- Every status move — first attempt or replay — goes through the same verified
  finish: the recorded decision comment is checked, the destination must be one
  of the two statuses the event contract allows, and the destination comes from
  the durable record rather than a freshly derived decision.
- Permanent rejections (wrong status, mis-bound subject, missing or incomplete
  chain, row outside the allowlists) are logged and skipped. They have no
  external effect, so they never wedge the reaction. A run whose validation has
  simply not reported yet falls in this category and is re-nominated by the next
  result diff.

### Idempotency

`runId` is `sha256` over the Project Item node ID, the subject node ID, and the
digest of the authoritative issue body; `eventId` is `sha256` over the `runId`
and the event type. The `RoutingDecided` `eventId` is therefore the reservation:
replaying the same row can only ever resolve to the same decision comment.

## Testing

Protocol-target reaction: a stateful `wiremock` server stands in for GitHub, and
a durable in-memory state store stands in for the persistent store.

```sh
make test              # unit tests
make integration-test  # end-to-end tests (ignored by default)
make lint              # clippy -D warnings + fmt --check
```

Integration coverage (`tests/integration_test.rs`):

- a passing validation routes directly to `AwaitingIssueRiskProfiling`
- a failing validation routes directly to `NeedsMoreInformation`
- the status is sampled continuously and never visits `AwaitingRouting`; exactly
  one `RoutingDecided` comment and one status mutation are produced
- duplicate delivery routes exactly once
- a body edited since validation produces zero side effects
- untrusted authors (a different account, and the trusted database ID under the
  wrong actor type) and edited completions are never routed
- a renamed login still routes — trust is keyed on the numeric database ID and
  the actor type
- an incomplete chain, and a completion from another execution, produce zero
  side effects
- two trusted completions claiming one event ID fail closed with zero writes
- byte-identical duplicate completions coalesce to the earliest comment
- an ambiguous comment write is persisted and never proceeds to the status write
- restart after that ambiguity adopts the existing comment instead of re-posting
- a completion edited after acceptance halts the resumed run
- stale and mis-bound rows produce zero side effects
- a silently rewritten completion body is not routed
- a completion re-summarised without an edit flag (byte-identical event JSON, so
  neither the edit check nor duplicate coalescing would notice) still halts a
  resumed run, because the persisted hash covers the exact accepted body
- one trusted, unedited comment claiming this run's `RoutingDecided` `eventId`
  with a different decision is never adopted: zero comments, zero status
  mutations, no status drift
- a status update that fails after the decision is published is finished from
  durable state on replay — even after the issue body changes — and applies the
  persisted status exactly once
- a published decision comment that is edited, deleted, or replaced by a
  different event halts the resumed run with zero side effects

## Dynamic plugin build

```sh
cargo build -p drasi-reaction-workgraph-router --release --features dynamic-plugin
```

The plugin ID is `workgraph-router-reaction`.
