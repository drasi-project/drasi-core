# Managed-query test controls

Enable `drasi-lib`'s **`test-support`** feature on a test dependency:

```toml
[dev-dependencies]
drasi-lib = { workspace = true, features = ["test-support"] }
```

`drasi_lib::test_support::QueryTestControl` supplies an independent physical
eligibility clock for an ordinary query added with `DrasiLibBuilder::with_query`.
Attach it before building:

```rust,ignore
let check = QueryTestControl::new(epoch_ms);
let deadline = QueryTestControl::new(epoch_ms);
let lib = DrasiLib::builder()
    .with_source(source)
    .with_query(check_query)
    .with_query(deadline_query)
    .with_query_test_control("check", check.clone())
    .with_query_test_control("deadline", deadline.clone())
    .build().await?;
```

After awaiting query readiness and establishing **query-applied input** ordering:

```rust,ignore
let before = check.advance_to(due - 1, timeout).await?;
let after = check.advance_to(due, timeout).await?;
assert!(after.output_sequence > before.output_sequence);
let stale = check.wake(timeout).await?;
assert_eq!(stale.output_sequence, after.output_sequence);
```

`new(u64)` initializes an epoch-millisecond clock; `now()` reads it.
`advance_to(u64, Duration)` advances monotonically and drains currently due work.
`wake(Duration)` drains at unchanged time, including an empty/not-yet-due queue.
Both return `drasi_lib::Result<DrainReport>`.

## What completion proves

The existing query-owned scheduler checks the real future queue, processes due
entries using the existing evaluator and transaction owner, and follows the
graph's existing output forwarding and delivery-confirmation hooks. No extra
runtime, worker, source event or synthetic result counter is introduced.

`DrainReport` contains `physical_time_ms`, committed logical `output_sequence`,
`output_generation`, `output_persistent` and actual `publication` mode. Work
produced/replayed by the drain has reached its immediate outgoing pipes.
This does **not** establish historical delivery, outlet observation, reaction
processing or external business completion. Keep those fences separate.
Source/WAL admission alone is not proof that query input has been processed.

Physical eligibility is independent of logical evaluation: a late wake at 9000
can process an entry due at 3000, but `datetime.realtime()` for that scheduled
evaluation still observes 3000. Stale entries use the evaluator's normal
validation and cannot consume a newer not-yet-due generation.

## Lifecycle and scope

Clones share clock and request serialization. A timeout/cancelled caller does
not roll back time or cancel graph-owned work; a later request cannot move the
clock while the earlier drain is blocked. Query stop preserves the production
graceful-drain contract and can time out on blocked output; it does not discard
that output or claim completion. Release deliberately blocked subscriber/storage
gates and await lifecycle cleanup, including shutdown. Controls do not introduce
force-cancellation or bypass cleanup. Processing failures are returned with their
causes, not reported as successful drains.

Unknown query IDs, duplicate attachments, sharing one control between queries,
out-of-range timestamps and backward advances are rejected. Before readiness
or after stop, requests fail rather than queueing against an inactive query.
Stop/restart preserves the control clock. Rebuilding the library requires a new
control. Controls do not change configuration hashes, query identities, output
generations, storage formats, transaction deduplication or recovery policy.

The feature supports ordinary builder-owned managed queries only. It does not
add a direct native component control API or a retained-history migration API.
With the feature disabled, no control state or branches are compiled. With it
enabled but no control attached, ordinary scheduling still uses the production
UTC clock and cadence; Tokio time pause alone does not change that clock.

Focused regressions: `cargo test -p drasi-lib --features test-support --test
query_test_control`; production-default regression: `cargo test -p drasi-lib
--no-default-features --test query_test_control`.
