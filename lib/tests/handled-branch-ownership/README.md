# Handled-branch ownership gate

This isolated, unpublished workspace implements the ownership proof in Phase 0
of the [Item 7 plan](../../../item7-implementation-plan.md). It is **not the
production latency feature** and does not change the telemetry catalogue,
runtime behavior, event encoding, or any production ABI.

Run from the repository root:

```sh
make test-handled-branch-ownership
cargo fmt --manifest-path lib/tests/handled-branch-ownership/Cargo.toml -p handled-branch-ownership -p handled-branch-ownership-fixture -- --check
cargo clippy --manifest-path lib/tests/handled-branch-ownership/Cargo.toml --workspace --all-targets --locked -- -D warnings
```

The optional release-mode `memory` example exercises both production default
boundaries and reads only its own process RSS:

```sh
cargo run --manifest-path lib/tests/handled-branch-ownership/Cargo.toml --example memory --release --locked
```

It verifies 16,384 records, the exact 8-MiB record-data limit, and complete
accounting cleanup. RSS includes allocator/map overhead and retained allocator
pages; measurements are machine-specific, not a universal process-memory limit.

The make target builds the fixture as a genuine independent cdylib before
loading it with `libloading`. The workspace is separate so this proof does not
rely on host/plugin feature unification or load the query runtime into its
plugin. It uses no tracing, OTel, or metrics exporter. Two additional native
tests exercise the actual shared `HandledBranchTracker`; their host-side timing
test uses Tokio's paused clock. The fixture itself still has no dependencies.

## What is proved

- One host-owned monotonic origin can feed separate output/sink branches.
  Completion is per branch, never per fan-out group.
- Per-envelope row bits make split processing idempotent. Completing one row
  repeatedly cannot substitute for a missing row. An invalid row set makes no
  partial update.
- Successful completion has a single linearization point. The receipt survives
  retries while any original lease is live; final release drops the origin,
  row state, and receipts without a TTL or permanent completed-history table.
- Independently built native code retains inputs after the creating call,
  clones them, transforms a value, and returns a producer-owned reply buffer.
  The reply retains timing until the host acquires it or discards the buffer.
- Only opaque owner/lease numbers cross the experimental FFI. Clocks, Rust
  ownership objects, and completion state stay in the host. Each side
  dereferences/releases only its own allocations.
- Callback contexts outlive graph retirement until native owners release them.
  Retirement prevents new timing work but does not prevent ownership cleanup.
- Both record count and accounted record-data bytes are admitted before
  allocation. Ownership transfers are charged, not an unbounded auxiliary map.
  Native timing saturation produces an untracked result, not a failed business
  transformation; disabled timing makes no registry calls.
- Tokens are owner-scoped and never reused. Foreign, expired, malformed, and
  retired timing requests have explicit outcomes.

Tests use a controllable host-only clock for exact durations, including a
24-hour live branch, and exercise concurrent host and cross-library callbacks.
No sleep duration is used as a correctness assertion.

## Ownership and saturation rules

An origin has one record, each independently acquired lease has one record, and
each output/sink branch has one record plus its row bitset. Local clones share
one owned lease. Each cross-library acquisition creates a distinct lease,
released by the acquiring owner. A native reply must retain its lease until
the host's acquisition attempt finishes, including failure.

Branch records stay with the origin until its last lease is released. If new
branch or transfer admission fails, seal further *new* branch admission for
that origin. Already admitted branches can still complete. This avoids an
unbounded negative-key cache and prevents a previously untracked retry from
later inventing a first-completion observation after capacity becomes available.

The byte counter charges concrete record data and row arrays, **not total
process RSS**. `BTreeMap` nodes, allocator bookkeeping, the registry itself, and
local wrapper allocations are additional; map growth is bounded by the record
cap. Production defaults and total retained-memory cost still require
qualification before this representation is adopted.

## Remaining production work

The protocol is experimental: it is not telemetry V3, an SDK capability, or a
new public runtime API. Production integration still needs:

- The real query-output identity rather than this proof's numeric output/sink
  keys, generation-fenced component authorization, and configured series.
- Default-off policy, transient context propagation, ingress and replay
  integration, actual graph/worker completion, and trace-independent coverage
  diagnostics on supported APIs.
- Negotiation through the existing native telemetry family, bounds on its
  actual frames, per-operation/poll/cancellation wiring, and compatibility with
  existing V1/V2/base plugins and an independently built old host.
- Full runtime/flavor tests, production budget selection, no-extra-work checks
  on disabled production paths, and overhead measurements.

The isolated tests are evidence for the ownership mechanism, not completion of
these integration/qualification gates.
