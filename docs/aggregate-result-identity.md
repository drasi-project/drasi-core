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

## Original identity failure proof

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

In that original experiment, the three assignments were restored byte-for-byte
to the original #810 runtime
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

## Linked codec dependency and combined validation

The upper #810 branch now includes a normal merge of the existing
[#909](https://github.com/drasi-project/drasi-core/pull/909) at
`6455dd3e4e1f8b969b9957aa042c22061cdb8a0b`, preserving both PR histories. That lower
commit includes current main `ead4279cc4ad847c875dc3ebebb1a54fe3b13de4` and the
named-field outbox writer correction for #908. Its staging, hydration, atomicity,
reader, and nine codec/reopen regression cases are inherited, not copied or
reimplemented in the upper layer.

Because #909 is fork-based, this is a **linked dependent-PR chain, not a native
GitHub stack**. #810 targets the upstream auxiliary branch
`agentofreality/core-909-base`, which must equal the exact lower PR head above.
If #909 changes, its owner/coordinator must explicitly synchronize the mirror,
then reconcile and revalidate #810. Retarget #810 to `main` only after an eventual
user-authorized lower merge and a fresh ancestry/check preflight. Neither PR is
merged or published by this integration.

The prior [required CI failure](https://github.com/drasi-project/drasi-core/actions/runs/35390267902)
on the old #810 head was real: main's hydration exposed unreadable compact
`Update { grouping_keys: None, ... }` outbox records before either aggregate
snapshot assertion. An ordinary non-aggregate update also reproduced that
failure independently on unchanged main. It is distinct from #680's incorrect
group identity.

On the actual combined upper branch, the formerly failing two persistent
aggregate tests and the lower ordinary-update reopen test pass. Temporarily
reversing only the inherited named writer to `rmp_serde::to_vec` makes all three
fail at durable outbox sequence 2; restoring the lower writer byte-for-byte makes
the identical command pass:

```sh
cargo test -p drasi-lib --lib -- aggregate_snapshot_tests \
  test_e2e_outbox_persistent_reopen::case_2_named_update
```

The combined selectors exercise all seven aggregate regressions and all nine
lower codec cases (plus two existing persisted-state tests) under both solvers:

```sh
cargo test -p drasi-core -p drasi-lib -- \
  aggregate_snapshot persisted_ test_e2e_outbox_persistent_reopen
cargo test -p drasi-core -p drasi-lib --features drasi-core/parallel_solver -- \
  aggregate_snapshot persisted_ test_e2e_outbox_persistent_reopen
```

The lower cases cover the production writer/reader, absent/present/empty grouping
keys, other result variants, metadata/profiling, valid compact and new named
records together, fresh reopen, sequence continuation, and visible Strict failure
without deleting malformed legacy records. No assertions or recovery policies
are relaxed. Full local and exact-head remote results are recorded on #810.

The existing test, lint, FFI, audit, deny, and coverage workflows explicitly
include this one mirror base, preserving their other filters and gates. A
post-retarget push must run fresh checks; old `main`-base checks do not establish
the upper layer's CI. Coverage keeps its existing draft-PR skip condition.

## Persisted-state migration boundary

The identity correction changes **key meaning for projected aggregates**, not the
result-diff or index schema. Separately, the inherited #909 changes the encoding
of new persisted `QueryResult` records. Neither is an automatic migration of
retained state produced by an older engine.

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

The original #810 extension at `540999d2` used a persistent-snapshot fallback.
The combined branch instead inherits main's hydration, atomic output, reset,
and transactional trimming from #826/#830/#835/#927 through #909. These remain
separate implementations with their existing tests; the aggregate correction
does not replace their guarantees.

There are **two different legacy-state limits**. Old contributor-keyed rows need
the complete authoritative reconstruction described above. Malformed positional
outbox records with omitted fields remain unreadable even after switching future
writes to named fields: Strict recovery must report the error and preserve the
records. Valid historical compact records with intact positional fields and new
named records remain readable together. See
[Persisted Outbox Compatibility](../lib/README.md#persisted-outbox-compatibility)
for the inherited codec boundary. Do not silently discard records, downgrade
Strict, or clear only the outbox/output to manufacture a passing migration.

## Consumption and isolated server evidence

The combined branch inherits `drasi-core 0.5.9` and `drasi-lib 0.9.2` manifests
from main; the corrections are unreleased source, so a crate version alone does
not identify them. The recorded
server baseline is `drasi-server 0.2.1` at
`a2b648062a4c55e036d68b6f26bf73b4e773bcf1`, locked to `drasi-lib 0.8.9`,
`drasi-core 0.5.8`, and SDK crate `0.10.0`, with signed plugins using FFI ABI
`0.11.0`. This workspace's SDK crate is `0.11.1` and its independent
`FFI_SDK_VERSION` is `0.14.0`. A wholesale workspace/SDK upgrade is **not**
compatible with those existing plugin binaries.

The separately authorized P1 diagnostic used locked core `0.5.8` plus the full
three-file runtime diff **`e759606..540999d`**, not this newer combined workspace:
`core/src/query/continuous_query.rs`, `core/src/evaluation/parts/mod.rs`, and
`core/src/evaluation/variable_value/mod.rs`. Lib `0.8.9`, SDK/host SDK `0.10.0`,
all five signed ABI `0.11.0` plugins, and the exact non-core lock graph remained
unchanged. Those identity hunks do not change `ResultDiff` or an FFI layout.
Do not substitute the combined branch's full diff against the old base: it also
inherits newer main APIs, including the result-aware pre-commit hook.

The P1 owner/coordinator reported **exit 0** with the existing real Trading
harness, scenarios, and assertions unchanged: initial value 2000/cost 1800, live
2050, reload 2050, and offline/reconnect 2150, with exactly one current row in
every recorded summary REST response. Reported SHA-256 values:

- Candidate binary: `503d78df44764cf8f797a1d6a91ec21d1cdc5e3e49e585b44f2563df8cf176e1`.
- Patched core source tree: `b411434609a70a755099528c5d76526a0bc0c5bf0ca3cc934d39ed33d1636136`.

That pass verifies only the **fresh-state core-only overlay**, not latest-main
persistence, a released crate/plugin combination, or the default
[drasi-project/drasi-server#201](https://github.com/drasi-project/drasi-server/pull/201)
runtime. This upper integration changes no server dependency, plugin, query,
React consumer, release, or publication. The default server foundation and React
stack are not declared unblocked; any authorized default-runtime adoption still
needs its own exact-build SQL/CDC + REST/SSE/UI gate with singleton and
2000 -> 2050 -> 2150 assertions unchanged.
