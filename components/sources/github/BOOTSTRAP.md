# GitHub Source Bootstrap Provider

`drasi-source-github` includes an internal bootstrap provider (`GitHubBootstrapProvider`) used by default when a custom provider is not supplied in `GitHubSourceBuilder`.

## Behavior

On bootstrap:

1. Build the effective repository set from:
   - configured static `repositories`
   - repositories discovered from configured `projects` items
2. Fetch a full reconcile snapshot from GitHub GraphQL (repositories, issues, pull requests, projects, project items).
3. Map snapshot objects to normalized `SourceChange` events.
4. Emit filtered bootstrap events according to requested node/relation labels.

`BootstrapResult.source_position` is `None` (runtime durability/recovery is handled by webhook admission + source WAL + authoritative hydrator snapshots).

## Bootstrap → Runtime Handover Semantics

- Bootstrap snapshot emission plus runtime webhook hydration is **convergent/at-least-once**, not exactly-once.
- If a crash happens after a runtime change is dispatched but before the committed-marker snapshot write, recovery can replay duplicate runtime `SourceChange` events.
- Query/reaction consumers must be idempotent with duplicate events and rely on convergence.
- Runtime receive order follows admitted WAL sequence, not GitHub’s global causal/event ordering.

## Configuration

Bootstrap uses the source config:

- `token`
- `repositories`
- `projects`
- `graphql_url`

and respects query bootstrap label filters from `BootstrapRequest`.

## Failure Modes

- Invalid/missing token or GraphQL request errors fail bootstrap.
- Project snapshot fetch failures fail bootstrap.
- Event channel closure stops emission early and returns emitted count so far.
