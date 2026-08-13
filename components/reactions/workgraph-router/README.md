# WorkGraph Router Reaction

`drasi-reaction-workgraph-router` is a deterministic Phase 2 WorkGraph router reaction.

It consumes **ADDED** rows from the single query `route-awaiting-workgraph-items`, revalidates row provenance + allowlists, reserves routing by `(executionId, requiredEventType)` with durable atomic CAS fencing, applies rules v1, writes trusted JSON comments, creates next routing responsibility, updates Project status, and checkpoints only after durable side effects.

## Prerequisites

- A durable Drasi state store (`is_durable() == true`)
- A GitHub token environment variable (default: `GITHUB_TOKEN`)
- Query output contract matching `RoutingCandidate` (see [Input contract](#input-contract))

## Configuration

Top-level config (`camelCase`):

- `policyId`, `policyType`, `policyVersion`
- `allowedProjects`, `allowedRepos`, `allowedEventTypes`
- `allowedStatusTransitions` (`[{ from, to }]`)
- `allowedResponsibilityTypes`, `allowedActors`
- `trustedRoutingAuthors`, `trustedLauncherAuthors`, `trustedAgentAuthors`, `trustedRouterAuthors`
- `githubGraphqlUrl`, `githubRestUrl`, `githubTokenEnv`
- `projectStatusFieldName` (defaults to `Status`)
- `timeoutSecs`
- `reservationLeaseSecs` (defaults to `120`; must be `>= timeoutSecs`)
- `strictRecovery`

`policyType` currently supports only `rules_v1` (extensible contract already exists for future `linear` / `llm` modes).

## Input contract

Each ADDED row must deserialize into:

```rust
RoutingCandidate {
  execution_id, required_event_type,
  event_id, event_type, outcome,
  subject_repo, subject_issue_number,
  project_id, project_item_id, project_status,
  route_id, route_expected_event_id, route_expected_event_type,
  route_expected_subject_repo, route_expected_subject_issue_number,
  route_content_version, route_content_profile,
  responsibility_id, responsibility_type, responsibility_actor, submitter_actor,
  launcher_author, agent_author, router_author, routing_author, observed_authors,
  comment_id, comment_author, comment_body, comment_edited,
  comment_provenance_event_id, comment_provenance_event_type,
  content_version, content_profile
}
```

Non-ADDED diffs (`UPDATE`/`DELETE`) are ignored and never produce routing decisions.

## Rules v1 behavior

For `CompletedIssueValidation`:

- `outcome=passed`:
  - Project status: `AwaitingRouting -> AwaitingIssueRiskProfiling`
  - Next responsibility: `issue-risk-profiling`
- `outcome=failed`:
  - Project status: `AwaitingRouting -> NeedsMoreInformation`
  - Next responsibility: `issue-correction`
  - Owner: `submitterActor`
  - Marker request included

## Side effects

1. Trusted pure-JSON issue comment:
   - `type: "workgraph.routing-decision/v1"`
2. Trusted pure-JSON issue comment:
   - `type: "workgraph.routing-responsibility/v1"`
3. GraphQL `updateProjectV2ItemFieldValue` project status mutation

Reconciliation on retry requires trusted, unedited comments; forged public comments do not satisfy reconciliation.

## Durability and recovery

- Durable reservation key: `(executionId, requiredEventType)` only
- Reservation ownership transitions (create, renew, takeover, complete) use exact-byte state-store CAS with a monotonically increasing fencing epoch
- Reservations carry a bounded lease (`reservationLeaseSecs`) and can be reclaimed only after lease expiry (or persisted failed state) via CAS takeover that increments epoch
- Stored state includes reservation, decision, selected transition, side-effect progress, ambiguous/failed flags, and errors
- `is_durable() = true`
- `needs_snapshot_on_fresh_start() = false`
- `default_recovery_policy() = Strict`
- Retry/resume always re-checks GitHub issue/project state immediately before each side effect. Status mutation executes only from source status (`AwaitingRouting`); if destination is already observed, the router reconciles and marks completion without mutating.

## Running checks

```bash
make build
make test
make integration-test
make lint
```

## Integration tests (protocol-target)

Integration tests in `tests/integration_test.rs` use a mocked GitHub REST/GraphQL harness (`wiremock`) plus a durable test state store wrapper.

Covered scenarios:

- pass/fail routing
- duplicate race suppression
- concurrent replica fencing with shared durable state
- policy-version conflict suppression
- interrupted old-policy resume using persisted decision contract
- untrusted input rejection
- stale content rejection
- wrong status rejection
- retry preflight protection for closed issues and competing status changes
- lease expiry recovery and CAS-fenced ownership takeover
- GraphQL HTTP 200 + `errors` failure behavior
- partial side-effect recovery via reconciliation
- update/delete ignored
- secret redaction

Run:

```bash
cargo test -p drasi-reaction-workgraph-router --test integration_test -- --ignored --nocapture
```

## Troubleshooting

- `environment variable '...' is not set`:
  - ensure `githubTokenEnv` points to an exported token variable
- `selected transition ... is not allowlisted`:
  - update `allowedStatusTransitions` to include policy output
- `project item ... status is 'X' (expected 'AwaitingRouting' or '<destination>')`:
  - row is stale, changed by another workflow, or already routed
