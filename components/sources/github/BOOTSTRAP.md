# GitHub Source Bootstrap Provider

`drasi-source-github` includes an internal bootstrap provider (`GitHubBootstrapProvider`) used by default when a custom provider is not supplied in `GitHubSourceBuilder`.

## Behavior

On bootstrap:

1. Build the effective repository set from:
   - configured static `repositories`
   - repositories discovered from configured `projects` items
2. Fetch a full reconcile snapshot from GitHub GraphQL (repositories, issues, pull requests, projects, project items).
3. Map snapshot objects to normalized `SourceChange` events.
4. Atomically persist one `ReconcileState` record containing the new generation, index, and
   optional pending live delta (root snapshots remain separate for root-level diffing).
5. Dispatch the pending delta to ready live subscribers and atomically clear it only after a
   complete delivery; leave it durable when none are eligible.
6. Emit the new query's filtered full bootstrap, mark only that query live-ready, and retry any
   retained pending delta while still holding the source processing gate.

The source only supports channel dispatch mode. Each bootstrapping query is registered not-ready
at the dispatcher edge. Live events are dropped (never queued) for that query until its complete
snapshot is emitted; other ready queries continue receiving live deltas.

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
- State persistence failures prevent both live-delta and full-bootstrap publication.
- A pending delta contained in `ReconcileState` is replayed at least once after restart, then
  cleared. Index equality never infers pending ownership.
- Event channel closure fails bootstrap and leaves the query not-ready.
