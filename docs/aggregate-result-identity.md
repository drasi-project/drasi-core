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

## Dependency integration history (2026-09-18)

At `211d0f2a79aa2ad0f7cb841937f52013fe95ded6`, the upper #810 branch included a
normal merge of the existing
[#909](https://github.com/drasi-project/drasi-core/pull/909) at
`6455dd3e4e1f8b969b9957aa042c22061cdb8a0b`, preserving both PR histories. That lower
commit included then-current main `ead4279cc4ad847c875dc3ebebb1a54fe3b13de4` and
the named-field outbox writer correction for #908. Its staging, hydration,
atomicity, reader, and nine codec/reopen regression cases were inherited, not
copied or reimplemented in the upper layer.

Because #909 was fork-based, this was a **linked dependent-PR chain, not a native
GitHub stack**. #810 then targeted the upstream auxiliary branch
`agentofreality/core-909-base` at that exact lower head. The mirror is now retained
only as historical provenance; it is not advanced, deleted, or used as the base
for the post-merge reconciliation below. No native stack registration was made.

The prior [required CI failure](https://github.com/drasi-project/drasi-core/actions/runs/35390267902)
on the old #810 head was real: main's hydration exposed unreadable compact
`Update { grouping_keys: None, ... }` outbox records before either aggregate
snapshot assertion. An ordinary non-aggregate update also reproduced that
failure independently on unchanged main. It is distinct from #680's incorrect
group identity.

On that combined branch, the formerly failing two persistent aggregate tests
and the lower ordinary-update reopen test passed. Temporarily reversing only
the inherited named writer to `rmp_serde::to_vec` made all three fail at durable
outbox sequence 2; restoring the lower writer byte-for-byte made the identical
command pass:

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

That layer added six exact mirror-base workflow filters. Its successful
[mirror-base Rust run](https://github.com/drasi-project/drasi-core/actions/runs/35403891956)
is historical evidence, not proof for a later main-based integration.

## Reconciliation after #909 merged (2026-09-23)

#909 merged into `main` at **`b1f3b3c19d4f06baf7ede3a9d27accc215f2f009`**, with
final contributor head `f9d5143fd8e940edfac3b21e9a65cf76703dae8e`. The merged tree
matches that final contributor tree; the original `6455dd3` snapshot is not a
substitute for it.

#810 integrates that exact main by a normal merge, preserving its existing
history and aggregate corrections, and targets `main` directly. The incoming
named writer, reader, atomic staging/hydration, streaming bootstrap (#749),
mandatory source-event sequences (#858), and package-visibility checks (#863)
are retained. The only source-level reconciliation above main is in the
aggregate test helper: it passes its real sequence to `SourceEventWrapper::new`
and preserves the same source-position bytes. The lower reopen test uses its
final #909 sequence argument. No test, checkpoint behavior, or Strict recovery
assertion is dropped.

The six obsolete mirror-only CI filter additions are removed, leaving the
workflows byte-for-byte equal to this main, including the new mocked
`package-visibility.test.sh` gate and all existing main/push/security/coverage
conditions. No protection or skip condition is weakened.

Fresh local validation of the reconciled source passed:

| Gate | Result |
| --- | --- |
| Aggregate/codec selectors, default and parallel solvers | 18 passed each |
| Complete core/library suites, default and parallel solvers | 761 core unit, 947 library unit, 35 integration, and 98 doctests passed each; 56 existing ignored |
| `cargo test --locked --workspace --exclude drasi-host-sdk` | 4,828 passed, 262 existing ignored |
| `make test-host-sdk` with locally built test plugins | 50 passed, 6 existing ignored |
| `make clippy` and strict parallel-solver core/library Clippy | Passed |
| Parallel-solver shared in-memory scenarios | 74 passed |
| Stable/nightly formatting, typos, dependency-cycle and mocked visibility/retry gates | Passed |
| Existing main audit/deny policy | Passed; 18 allowed audit warnings, no new suppressions |

Builds used only this worktree's artifacts with two compiler jobs; full workspace
tests used two test threads. Existing package versions, sources, checksums, and
dependency edges were preserved except the two edges added by incoming main's
MSSQL/Oracle bootstrap manifests. No dependency refresh was performed. The
initial `--locked` rejection of those missing edges and its offline reconciliation
are recorded in the PR evidence.

Exact main-based remote CI head/base/tree and outcomes are recorded on #810
separately from these local results. Neither the old mirror's green CI nor the
server diagnostic below substitutes for fresh checks. This reconciliation does
not merge #810 or publish a release.

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

## Historical server diagnostic and source-consumption boundary

The combined branch inherits `drasi-core 0.5.9` and `drasi-lib 0.9.2` manifests
from main. The aggregate identity correction remains PR source, so a crate
version alone does not identify it. The following diagnostic records the
2026-09-18 server baseline, not the current development stack:
`drasi-server 0.2.1` at
`a2b648062a4c55e036d68b6f26bf73b4e773bcf1`, locked to `drasi-lib 0.8.9`,
`drasi-core 0.5.8`, and SDK crate `0.10.0`, with signed plugins using FFI ABI
`0.11.0`. This workspace's SDK crate is `0.11.2` and its independent
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
runtime. Statements that the default server foundation and React stack were
blocked belong to that historical baseline; this document does not determine
the current server stack's status.

Before the 2026-09-23 core reconciliation, the server owner/coordinator preserved
the selected **`211d0f2a79aa2ad0f7cb841937f52013fe95ded6`** source and tree
`a57386ecd0b200631059fcffd8a61bf036f4febb` in a separate persistent detached
checkout. The shared server source link continues to consume that exact snapshot,
not this moving PR branch. Its selected core/AST/Cypher development override,
registry library/SDK dependencies, server pins, manifests, lockfiles, binaries,
and running demo are not changed by this core task.

Updating #810 is **core PR validation, not server-adoption proof**. Consuming its
newer source requires a separate user decision and exact-build compatibility
validation. No server dependency/plugin/ABI, React layer, release, or publication
change is made here, and no claim of newly unblocking server work follows from
the core checks.
