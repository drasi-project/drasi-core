# drasi-source-github

Authorized GitHub source plugin for Drasi.

## Minimal-v1 architecture

The source runs with two WAL partitions:

- `<source_id>::inbox` — admitted signed webhook deliveries (locator-only)
- `<source_id>` — normalized output `SourceChange` events

Flow:

1. Webhook ingress verifies HMAC + headers
2. Under one ingress mutex, checks retained inbox WAL for duplicate delivery GUID
3. Appends to inbox WAL before returning `2xx` (capacity/failure => `503`)
4. One strict FIFO worker hydrates authoritative state via GraphQL
5. Worker appends every normalized output change to output WAL **before** best-effort live dispatch
6. Worker prunes inbox entry after processing

## Replay and retention

- `supports_replay = true`
- Fresh subscribers replay retained output WAL from oldest sequence, then switch to live atomically.
- Resumed subscribers replay from `resume_from` using normal WAL replay.
- Output WAL pruning only advances from confirmed subscriber positions.
- With no subscribers (or no confirmed positions), retained output WAL is not pruned.

## Scope behavior

- Startup project discovery is **scope-only** (no startup graph emission).
- Project item hydration can grow repository scope.
- Scope does not shrink automatically (no reconcile loop/full inventory repair).

## Delivery semantics

- At-least-once/convergent delivery.
- Crash window between output append and inbox prune can replay a delivery; deterministic authoritative diffing converges state.
- Non-delete `node: null` hydrations are retried with bounded attempts, then treated as `gone-before-hydration` and FIFO advances.
- A literal `deleted` webhook action bypasses GraphQL and emits exactly one delete for the identified GitHub object. It does not infer cascade or relation deletes.

## Public config (v1)

Schema name: `source.github.GitHubSourceConfig`

The source properties are flat in the server source block:

```yaml
kind: github
autoStart: true
token:
  kind: Secret
  name: github-pat
repositories:
  - acme/widgets
projects:
  - owner: acme
    number: 3
webhook:
  host: 0.0.0.0
  port: 8080
  path: /webhook
  secret:
    kind: Secret
    name: github-webhook-secret
  bodyLimitBytes: 10485760
durability:
  enabled: true
  max_events: 10000
  capacity_policy: RejectIncoming
graphqlUrl: https://api.github.com/graphql
```

At least one repository or Project is required. `token` and `webhook.secret`
must be SecretReferences. Unknown fields are rejected.

## Contracts preserved

- kind: `github`
- plugin reference: `source/github`
- crate/name: `drasi-source-github`
- library basename: `libdrasi_source_github` on Linux/macOS (`drasi_source_github.dll` on Windows)
- descriptor secret enforcement for `token` and `webhook.secret`
- Graph schema: 8 nodes / 6 relations
- body/status/author mapping contracts (`bodyDigest`, `statusName`, author fields)
- no `performedViaGithubAppId` field

## Testing

Unit tests:

```bash
cargo test -p drasi-source-github
```

Integration tests (ignored by default):

```bash
cargo test -p drasi-source-github --test integration_test -- --ignored --nocapture
```

## Troubleshooting

- `401` webhook: signature/secret mismatch.
- `503` webhook: inbox WAL full/failing or source in fatal hydrator state.
- No updates: verify GraphQL authoritative fetch for locator node and `/health` status.
