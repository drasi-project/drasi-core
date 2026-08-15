# WorkGraph Admission Reaction

`drasi-reaction-workgraph-admission` is the entry point of the minimal WorkGraph
workflow. For one eligible Project Item + Issue pair it assigns the
`issue-validation` responsibility as a single `WorkGraphEvent/v1` comment and
then admits the item to `AwaitingValidation`.

```text
admission          -> ResponsibilityAssigned  -> status AwaitingValidation
copilot-agent-task -> ExecutionStarted
issue-validator    -> CompletedIssueValidation
workgraph-router   -> RoutingDecided          -> AwaitingIssueRiskProfiling
                                                 or NeedsMoreInformation
```

The event format, the deterministic `runId`/`eventId` algorithms, and the outer
comment grammar all live in [`components/workgraph-common`](../../workgraph-common).

## What it writes

Exactly one comment, in the exact shared grammar:

```text
WorkGraphEvent/v1

WorkGraph assigned issue validation for drasi-project/drasi-core#742

{"schemaVersion":"workgraph.event/v1","eventId":"event:…","eventType":"ResponsibilityAssigned","runId":"run:…","projectItemNodeId":"PVTI_…","subjectNodeId":"I_…","payload":{"responsibilityType":"issue-validation","profileRef":"issue-validator@<40-hex>","contentDigest":"sha256:<64-hex>"}}
```

…followed by exactly one `updateProjectV2ItemFieldValue` mutation setting the
Project status to `AwaitingValidation`.

## Row contract

Rows added to the single subscribed query must deserialize into
`AdmissionCandidate` (unknown fields are rejected):

| Field | Type | Notes |
|---|---|---|
| `repository` | string | `owner/repo`, must be allowlisted |
| `subjectNumber` | integer | issue number |
| `subjectNodeId` | string | `I_…`; re-verified against GitHub |
| `projectNodeId` | string | `PVT_…`, must be allowlisted |
| `projectItemNodeId` | string | `PVTI_…` |
| `projectStatus` | string | must equal `expectedSourceStatus` |

`Update` and `Delete` diffs are ignored — only rows newly added to the result set
can trigger admission.

A row only *nominates* an item. Everything trusted is re-read from GitHub before
any write: the authoritative issue body (which fixes the `runId`), the issue
state and node ID, the item↔issue↔project binding, the live status, and the agent
profile blob SHA.

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
| `expectedSourceStatus` | string | — | **required**, must differ from `AwaitingValidation` |
| `agentProfile` | string | `issue-validator` | pinned profile name |
| `profileBaseRef` | string | `main` | ref the profile blob is read from |
| `trustedAuthorDatabaseId` | u64 | — | **required, > 0**, numeric GitHub database ID of the identity this reaction posts as; see [Author trust](#author-trust) |
| `trustedAuthorType` | string | `Bot` | `User`, `Bot`, or `Organization` — the other half of the trust key |
| `timeoutSecs` | u64 | `30` | per-request timeout |
| `strictRecovery` | bool | `true` | must remain `true` |

Config schema name `reaction.workgraph_admission.WorkgraphAdmissionReactionConfig`,
config version `1.0.0`. Unknown fields are rejected.

### Author trust

Configured trust is exactly two values:

```yaml
trustedAuthorDatabaseId: 4021243
trustedAuthorType: Bot
```

The authoritative GitHub Source projects four comment author fields, and this
reaction maps them as follows:

| Source field | Role |
|---|---|
| `authorDatabaseId` | compared against `trustedAuthorDatabaseId` |
| `authorType` | compared against `trustedAuthorType` |
| `authorId` (node ID) | **audit data only** — never configured, never compared |
| `authorLogin` | **display only** — never compared |

Both trust values must match for a comment to be trusted.

- A **login is display-only**: logins can be renamed and the freed name
  reclaimed, so a login can never earn trust.
- The **node ID is not configured.** It is carried when the Source reports it so
  logs and errors can cite the exact account, and its absence never blocks
  trust.
- **No GitHub App attribution is involved.** The authoritative Source does not
  expose one for the comment and review nodes this workflow consumes, so
  requiring it would either fail closed on every real event or invite a
  non-authoritative substitute.
- **Known limitation:** every token authenticating as one GitHub identity (any
  PAT for that account, or a GitHub App user-to-server token acting as it)
  reports the *identical* `authorDatabaseId` and `authorType`, so in this
  prototype such tokens are **not separately attributable**. The trusted identity
  must be a dedicated automation account whose credentials are not shared.

### Narrowness

The reaction cannot be configured into a general-purpose GitHub client:

- both GraphQL documents are compile-time constants — there is no request
  template and no configurable mutation;
- endpoints must be `https` (or loopback) and may not embed credentials;
- the only write verbs are "create one issue comment" and "set one single-select
  status option"; and
- empty allowlists allow nothing.

## Durability and recovery

- `is_durable() = true`, `needs_snapshot_on_fresh_start() = false`,
  `default_recovery_policy() = Strict`, `checkpoint_ownership() = Reaction`.
- The durable record is keyed by `runId` and written **before** the first GitHub
  write, so a record's existence means "an external effect may already have
  happened" and recovery reconciles rather than retrying blindly.
- The record is consulted **before** the mutable profile path is resolved. A
  completed run is a no-op decided from the record alone — no profile read, no
  Project read, no writes — and an in-flight run is rebuilt from the immutable
  `profileRef` its own record captured. `profileBaseRef` is a mutable ref, so
  the blob it resolves to drifts with ordinary commits; that drift must never
  re-open a finished run or block one whose assignment is already public. The
  live profile is resolved only for a `runId` that has no record yet, and if a
  concurrent creator wins the compare-and-swap, the winner's recorded pin is
  what gets resumed.
- A record that disagrees with the run it is stored under (`runId`,
  `contentDigest`, row binding) or whose `profileRef`/`eventId` cannot rebuild
  the run's assignment is corrupt, and fails closed instead of publishing
  something the intent never promised.
- Each side effect is persisted with an exact-bytes compare-and-swap before the
  next one starts, so a stale writer can never clobber newer progress.
- A failed or unconfirmed write marks the record ambiguous and stops the
  reaction without advancing the checkpoint.
- On the next attempt the reaction lists issue comments and **adopts** an
  existing assignment only when its author is the trusted author (numeric
  database ID + actor type), GitHub reports it as unedited, and its canonical
  event JSON — envelope *and* payload — is byte-identical to the assignment this
  reaction intends to publish. `eventId` covers the run and event type only, so
  a *single* pre-existing comment claiming that `eventId` with different content
  fails closed rather than being adopted, as do two comments that disagree.
- The status mutation tolerates "already at `AwaitingValidation`", which is what
  makes a retry after an ambiguous mutation safe.
- Permanent rejections (stale status, closed issue, mis-bound subject, row
  outside the allowlists) are logged and skipped. They have no external effect,
  so they never wedge the reaction.

### Idempotency

`runId` is `sha256` over the Project Item node ID, the subject node ID, and the
digest of the authoritative issue body. Editing the issue body therefore starts a
*new* run with a *new* assignment rather than mutating the old one, and replaying
the same row can never produce a second comment. Editing the *profile* does not
touch existing runs at all: they complete at the blob they were pinned to, and
only the next new run picks the new blob up.

## Testing

Protocol-target reaction: a stateful `wiremock` server stands in for GitHub, and
a durable in-memory state store stands in for the persistent store.

```sh
make test              # unit tests
make integration-test  # end-to-end tests (ignored by default)
make lint              # clippy -D warnings + fmt --check
```

Integration coverage (`tests/integration_test.rs`):

- happy path: one canonical comment, then the status mutation
- duplicate delivery admits exactly once
- an ambiguous comment write is persisted and never proceeds to the status write
- restart after that ambiguity adopts the existing comment instead of re-posting
- forged (untrusted author, wrong actor type) and edited comments are never
  adopted
- two trusted comments claiming one event ID fail closed with zero writes
- one trusted, unedited comment claiming this `eventId` with a different payload
  is never adopted: zero comments, zero status mutations, no status drift
- an author GitHub reports without a node ID is still adopted (the node ID is
  audit data)
- stale status, mis-bound subject, and closed issues produce zero side effects
- editing the issue body produces a distinct run
- a completed run replayed after the profile blob moves is a no-op: no comment,
  no status mutation, no profile read, and the reaction keeps running
- a status write that fails after publication is finished from the recorded pin
  on restart, even though the profile moved in between: no second comment and
  exactly one status mutation

## Dynamic plugin build

```sh
cargo build -p drasi-reaction-workgraph-admission --release --features dynamic-plugin
```

The plugin ID is `workgraph-admission-reaction`.
