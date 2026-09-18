# Aggregate result identity and retained snapshots

An aggregate followed by an ordinary projection still identifies a **group**, not
the individual `MATCH` solution that most recently contributed to it. This
includes a global aggregate with no grouping columns:

```cypher
MATCH (p:Position)
WITH sum(p.value) AS totalValue, sum(p.cost) AS totalCost,
     count(p) AS positionCount
RETURN totalValue, totalCost, positionCount
```

In [#680](https://github.com/drasi-project/drasi-core/issues/680), projected
aggregate updates had contributor signatures. `QueryOutputState` and persistent
live-result writers correctly upserted by the supplied signature, but different
contributors therefore left multiple historical rows for one aggregate.

[#810](https://github.com/drasi-project/drasi-core/pull/810) preserves the grouping
hash through the final projection: `Adding` and `Updating` use
`after_grouping_hash`; `Removing` uses `before_grouping_hash`. For queries without
aggregation, these hashes start as the solution signature, preserving ordinary
row identity. No value-based row reconciliation is needed or safe: independent
groups may project identical values and may omit their grouping columns entirely.

## Regression coverage

The engine tests in
[`aggregate_update_tests.rs`](../core/src/query/tests/aggregate_update_tests.rs)
reuse the existing materialized-query helper and minimize joined portfolio rows
to their value/cost contributions. They assert the complete keyed result set,
not merely the numerical value of an emitted delta.

The public API tests in
[`aggregate_snapshot_e2e.rs`](../lib/tests/aggregate_snapshot_e2e.rs) use the
production Trading query, both synthetic joins (`OWNS_STOCK`, `HAS_PRICE`), two
source IDs, quantities, and purchase-price conversion from
[the recorded server fixture](https://github.com/drasi-project/drasi-server/tree/428ea4bb2121b565af39e2ae25bf010a65d344f8/examples/trading/app/test/fixtures/recorded/a2b6480-core-0.5.8).
Only the transports are replaced by the existing mock-source helper and a
deterministic bootstrap provider. They check `DrasiLib::get_query_results`, keyed
snapshots, and subscribed live diffs after bootstrap/processing acknowledgments,
without settling sleeps.

| Stage | Authoritative Trading summary | Retained rows |
| --- | --- | --- |
| Bootstrap AAPL 10 at 110, MSFT 5 at 180 | Value 2000, cost 1800, count 2 | Exactly 1 |
| AAPL price 115 | Value 2050, cost 1800, count 2 | Exactly 1, same signature |
| AAPL price 125 | Value 2150, cost 1800, count 2 | Exactly 1, same signature |
| Additional deletion of the MSFT **position** | Value 1250, cost 800, count 1 | Exactly 1, same signature |
| Delete the remaining position | Empty result | 0 |
| Recreate query with empty bootstrap | Empty result | 0 |
| Recreate query with the original complete bootstrap | Value 2000, cost 1800, count 2 | Exactly 1, original signature |

The additional position deletion is deliberately **not** the live-server
scenario's MSFT watchlist deletion: removing a watchlist entry does not remove its
portfolio contribution.

Additional coverage includes:

- The query from #680, with two floors and three rooms per floor. Initially both
  averages are 46; different contributors update one floor to 50 then 54, while
  the other remains independent. Both floors subsequently have the same average
  of 50 without losing or duplicating either group.
- Equal-valued groups whose keys are projected away, updates to just one group,
  and migration into a populated destination with removal of the drained source.
- Alternate contributors, deletion, reinsertion, and fresh engine reconstruction
  in reverse insertion order.
- Native RocksDB reopen, continuing updates from another contributor, deletion,
  and an empty persisted snapshot, using the existing checkpoint-test fixtures in
  [`e2e_checkpoint_tests.rs`](../lib/src/queries/e2e_checkpoint_tests.rs).

The existing #792 and #897 tests and runtime changes remain intact. Terminal
aggregations (an aggregating `RETURN` without the ordinary projection) retain
identity-valued empty rows under existing semantics. A separate test explicitly
distinguishes this from the projected aggregate's removal; this work does not
claim to resolve #384/#409 or rebootstrap double-counting issues.

## Controlled failure proof

The control used #810 at `76dc78055887eaceb9fc84e94578d73ed5f1c298` plus the new
tests. Only the three final identity assignments in `project_solution` were
temporarily changed back to `cc.solution_signature`; all other #810 corrections
were left in place. No branch/history rewrite or alternative reconciliation
prototype was used.

```sh
RUST_LOG=warn cargo test -p drasi-core \
  aggregate_snapshot_global_sum_tracks_bootstrap_updates_and_deletes
RUST_LOG=warn cargo test -p drasi-lib --test aggregate_snapshot_e2e
```

With those original faulty stamps, both commands exit 101:

- The engine retains the intermediate `{totalValue:1100,totalCost:800,positionCount:1}`
  row alongside `{totalValue:2000,totalCost:1800,positionCount:2}`.
- The Trading public snapshot contains those same two rows instead of one.
- The floor-comfort live update changes its signature when a different room
  contributes to the unchanged floor group.

The three assignments were restored byte-for-byte to the original #810 runtime
file (SHA-256
`03d59c1256e33982871b0fd0f81ba9b1574eadac3186dbc84a5b36cb2ae95911`).
`git diff --exit-code HEAD -- core/src/query/continuous_query.rs` confirmed that
the experiment left no runtime modification.

```sh
RUST_LOG=warn cargo test -p drasi-core -p drasi-lib aggregate_snapshot
RUST_LOG=warn cargo test -p drasi-core -p drasi-lib \
  --features drasi-core/parallel_solver aggregate_snapshot
```

Each restored run passes all seven new regressions: three engine tests, two
public API tests, and two persistent-output tests. Broader suite and CI results
are recorded on #810 separately from this focused failure proof.

## Persisted-state migration boundary

This changes **key meaning for projected aggregates**, not the result-diff,
serialization, or index schema. It is not an automatic migration of retained
output produced by a faulty engine.

The legacy-key regression constructs actual `MATCH` contributor signatures,
seeds intermediate/current rows under those keys in RocksDB, reopens through
DrasiLib, applies a corrected aggregate update, and reopens again. The two
legacy rows remain alongside the new group-keyed row. Reopening or applying a
new correctly keyed upsert cannot infer which old keys should be removed.
Guessing from `before` values would also risk removing legitimate equal-valued
groups.

For affected persisted queries, arrange an **explicit complete reconstruction
from authoritative source data**. The tested safe isolation is a new query/state
namespace: reconstruct all contributions there and verify its snapshot before
switching consumers. The regression also verifies the old namespace is left
intact. Coordinate reaction snapshots/cursors with that switch. Do not discard
only the output rows while retaining incompatible checkpoints/accumulators, and
do not assume stop/start or removing/re-adding the same ID clears every durable
store on an older host.

This branch's `fetch_snapshot` can fall back to persistent live results when its
in-memory output is empty. The focused reopen tests do not establish complete
multi-group hydration after the first resumed event, result-sequence/outbox
continuity, or same-ID reset completeness. Those are separate recovery concerns,
including the work tracked in #826/#830/#835; no such implementation is imported
or changed here.

## Consumption and remaining server validation

This branch has `drasi-core 0.5.8` and `drasi-lib 0.9.1` manifests; the correction
is unreleased source, so a crate version alone does not identify it. The recorded
server baseline is `drasi-server 0.2.1` at
`a2b648062a4c55e036d68b6f26bf73b4e773bcf1`, locked to `drasi-lib 0.8.9`,
`drasi-core 0.5.8`, and SDK crate `0.10.0`, with signed plugins using FFI ABI
`0.11.0`. This workspace's SDK crate is `0.11.1` and its independent
`FFI_SDK_VERSION` is `0.14.0`. A wholesale workspace/SDK upgrade is **not**
compatible with those existing plugin binaries.

For a separately authorized isolated server trial, the ablation identifies the
three final identity assignments in `core/src/query/continuous_query.rs` as the
#680 identity correction. Apply/pin a core-only source override based on the
locked `0.5.8` source while
keeping the server, `drasi-lib 0.8.9`, SDK/host SDK, dependency lock, and signed ABI
`0.11.0` plugins otherwise unchanged. To retain this PR's complete deletion,
migration, and whole-element grouping behavior (#792/#897), carry all its core
runtime hunks, including `core/src/evaluation/parts/mod.rs` and
`core/src/evaluation/variable_value/mod.rs`, not only those three assignments.
None of these identity changes require a `ResultDiff`, FFI layout, or consumer API
change. Verify the resolved dependency graph and the exact built revision in that
trial; neither the reduced-hunk variant on the locked server nor an arbitrary
workspace upgrade is established by the Rust tests here.

No server dependency, plugin, query, React consumer, release, or publication is
changed by this validation. The owner of
[drasi-project/drasi-server#201](https://github.com/drasi-project/drasi-server/pull/201)
must rerun its actual live-server gate with fresh isolated state and preserve the
singleton aggregate, 2000 -> 2050 -> 2150 totals, SQL/CDC, reload, and offline-update
reconnect assertions. REST snapshots, real SSE, and the UI must agree without
filtering old rows, selecting a convenient row, recomputing totals, or forcing a
refresh. The server foundation and React stack remain blocked until that gate
passes.
