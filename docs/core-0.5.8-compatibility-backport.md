# Core 0.5.8 compatibility backport

This is the local-source delivery path approved in
[#933](https://github.com/drasi-project/drasi-core/issues/933) for the reviewed
[#810](https://github.com/drasi-project/drasi-core/pull/810) aggregate-identity
correction. It is **not a released fix**, a new aggregation algorithm, or a
reversion of current `main`. The implementation branch
`agentofreality-compatible-core-backport` targets only
`agentofreality/core-0.5.8-base`.

## Immutable origin and reviewed delta

The clean starting checkout and remote compatibility base were independently
verified at released commit `3d73500b1605428b441908d4eefa6375dde659b0`.
Annotated tag `drasi-core-v0.5.8` has tag object
`9b6a6c2a18f179745dd938774ad837b05adc15c4` and resolves to that same commit.
The official `drasi-core-0.5.8.crate` archive has SHA-256
`cc8377915e909144b748f0944d1f024d9ed7931c82b1609520b58f8447ac8813`;
its `.cargo_vcs_info.json` identifies the same commit and the `core` path.

Only the complete three-file patch from
`e759606fa065bee0ef9e60017e263f0f85dd0e44..540999d29375b509e88577eb4d24980618ef8ef8`
was applied to runtime code. The release's three starting files are byte-identical
to that patch's starting files, and the resulting three files are byte-identical
to the reviewed endpoint. No current-main commits, library implementation, SDK
code, or whole newer workspace was imported.

Reproduce the exact patch serialization with:

```sh
git -c core.abbrev=9 diff --no-ext-diff --no-color \
  e759606fa065bee0ef9e60017e263f0f85dd0e44..540999d29375b509e88577eb4d24980618ef8ef8 \
  -- core/src/query/continuous_query.rs \
     core/src/evaluation/parts/mod.rs \
     core/src/evaluation/variable_value/mod.rs
```

Its SHA-256 is
`200d4ad104dabac684297b83b35f31fa6f99baef17836ab3520957020f0a512d`
(9,568 bytes). Git object abbreviation width affects patch bytes: eight-character
abbreviations produce `c40df2ba0d34ce089c532fa600bb1009c0eaf25c165c766a2ffdddd1337d65a5`
without changing any hunk. The complete patch includes its existing white-box
collapsed-aggregation test.

| Runtime file | Released SHA-256 | Backported SHA-256 |
| --- | --- | --- |
| `core/src/query/continuous_query.rs` | `f34da3b3de346266faa4916c77b5c4cec2a014c1cd3674421212e92028901e08` | `03d59c1256e33982871b0fd0f81ba9b1574eadac3186dbc84a5b36cb2ae95911` |
| `core/src/evaluation/parts/mod.rs` | `c7bad9b228ab43306d7f973966f115be7de227c0085e47fd53bc8ea1cf6c1857` | `377f360a9dbf11a811e32d77d6e82880399c1169bf39399e9cdf8c8a8e42f507` |
| `core/src/evaluation/variable_value/mod.rs` | `ef82fb3cd0333578e7ef58da259243aabafc1da0d104de4ef06c4cc7436bfaae` | `1fc6bcbcf70841096e75f0972dce63a9de0352af6c914bfcdffedaaf1dc392e9` |

## Behavior and API boundary

An aggregate followed by an ordinary projection still identifies a group, not
the last contributing `MATCH` solution. In
[#680](https://github.com/drasi-project/drasi-core/issues/680), contributor-keyed
updates left historical aggregate rows in a materialized snapshot despite
correct live values.

The reviewed correction preserves grouping hashes through final projection:
`Adding`/`Updating` use `after_grouping_hash`, and `Removing` uses
`before_grouping_hash`. Without aggregation these hashes are initialized to the
solution signature, preserving ordinary row identity. The full correction also
compares grouped elements by reference, preserves initial/final default-state
flags when collapsing changes, and distinguishes adding, updating, removing and
no-op outcomes when contributors move or drain a group. Equal projected values
do not identify a row: independent groups can project identical values, including
when grouping columns are omitted from the final projection.

The released public checkpoint hook is unchanged:

```rust
F: FnOnce() -> Fut + Send,
Fut: Future<Output = Result<(), IndexError>> + Send,
```

The external
[`released_checkpoint_hook.rs`](../core/tests/released_checkpoint_hook.rs)
guard calls it with an owned `move || async move` closure, just as retained
registry `drasi-lib 0.8.9` requires. It also verifies the hook sees the index update
and stages its owned checkpoint once. This in-memory test is not a transactional
rollback or durable-recovery proof.

The result-taking hook from merged
[#830](https://github.com/drasi-project/drasi-core/pull/830) is not backported.
Public result types, SDK/FFI layout, manifests and package versions are unchanged.
Terminal aggregations still retain identity-valued empty rows under the released
semantics; this is not a fix for #384/#409.

## Release-compatible regression proof

The tests adapt #810's engine materializer and fixtures without using newer
library hydration, snapshot, SDK or checkpoint APIs. Every materialized row is
keyed only by the engine's emitted signature; the harness neither filters stale
values nor recomputes aggregates.

- [`aggregate_update_tests.rs`](../core/src/query/tests/aggregate_update_tests.rs):
  global bootstrap/current snapshots, 2000 -> 2050 -> 2150 live deltas, different
  contributors, deletion of the last contributor, reinsertion, reverse-order
  fresh contribution reconstruction, equal-valued independent groups, populated
  and empty destination migrations, whole-element updates, ordinary filtered rows,
  and the unchanged terminal-aggregation boundary.
- [`aggregate_snapshot_tests.rs`](../core/src/query/tests/aggregate_snapshot_tests.rs):
  the exact Trading portfolio-summary query, both synthetic joins
  (`OWNS_STOCK`, `HAS_PRICE`), both source IDs and string purchase-price conversion;
  plus #680's floor-comfort query with two floors and three rooms per floor.
  Expected summaries remain value 2000/cost 1800/count 2, then 2050 and 2150.
  Independent floor averages start at 46, move to 50/54, then converge at 50
  without losing either group.
- [`retained_multi_source_tests.rs`](../core/src/query/tests/retained_multi_source_tests.rs):
  optional matches, zero counts, filtered group migration and convergence of
  incrementally retained output with complete fresh reconstruction.

The extra Trading deletions remove contributing **positions**, not the live
Trading scenario's watchlist entry. Joined reconstruction follows the original
fixture's source order. An exploratory reversal of every joined bootstrap node
produced an empty result. An identical isolated diagnostic was then run on the
unchanged release and after applying only the reviewed patch (default solver):

| Bootstrap order | Released engine | Backported engine |
| --- | --- | --- |
| Original joined fixture order, final AAPL price 125 | Two contributor-keyed rows: 1250/800/count 1 and 2150/1800/count 2 | One group-keyed row: 2150/1800/count 2 |
| All joined nodes reversed | Empty result | Empty result |

Thus the observed missing joined results precede this patch and are distinct from
the corrected identity duplication. Arbitrary synthetic-join bootstrap ordering
is not a guarantee of this backport; no join/order fix is included. Reverse-order
reconstruction of independent contributions remains tested separately. The
actual Trading query and fixture's original source order are unchanged.

Before applying the runtime patch, both solvers ran the regressions on the
unchanged released files: **8 failed, 11 passed**, with the external hook guard
also passing. Failures included retaining both 1100/800/count 1 and
2000/1800/count 2 Trading summaries, changing floor identity when another room
contributed, and retaining a drained group. After the full patch, the same
assertions passed: **19 query tests plus the external hook guard**, for each
solver. The final test harness was also copied into an isolated exact-release
archive and reproduced the same eight failures, without rewriting branch history
or reversing runtime changes in the owning checkout.

```sh
cargo test --no-fail-fast -p drasi-core -- \
  query::tests:: zero_argument_checkpoint_hook_remains_source_compatible
cargo test --no-fail-fast -p drasi-core --features parallel_solver -- \
  query::tests:: zero_argument_checkpoint_hook_remains_source_compatible
cargo test --locked -p drasi-core -p drasi-query-ast -p drasi-query-cypher
cargo test --locked -p drasi-core --features parallel_solver
cargo clippy --locked -p drasi-core -p drasi-query-ast -p drasi-query-cypher \
  --all-targets --features drasi-core/parallel_solver -- -D warnings
cargo +nightly fmt --all -- --check
```

The selected-source gate passes 758 core unit tests, the external hook test,
24 Cypher parser tests and one core doctest (one existing doctest ignored).
The parallel core suite and strict all-target Clippy pass. Full-workspace nightly
formatting passes on both the backport and the exact release archive. Existing
group-switch and incident-alert expectations are adapted exactly as in #810 to
assert the corrected source/destination default flags; financial expectations
are not weakened.

The released repository does not track `Cargo.lock`. Local gates used the freshly
resolved, ignored lockfile with SHA-256
`b4cb4c0cc54ee019212dcaf75c9ac4e78fb07da99a230b708d7ff9119673b345`,
copied unchanged into the release controls. This is validation-only dependency
resolution, not the server's retained lockfile or an authorized dependency upgrade.

Broader legacy-workspace results and exact-head remote checks are recorded on
#933 and the backport PR, including any release-baseline failures. The six
existing test/lint/FFI/audit/deny/coverage workflows gain only the exact PR base
`agentofreality/core-0.5.8-base`. Other filters, coverage's main-only push trigger,
draft conditions, commands and gates remain unchanged.

## Local source closure and immutable consumption

The selected normal dependency closure is:

```text
drasi-core 0.5.8 (core/)
  drasi-query-ast 0.3.5 (query-ast/)
  drasi-query-cypher 0.3.6 (query-cypher/)
    drasi-query-ast 0.3.5 (the same query-ast/)
```

For the authorized sibling checkout/link, the server root uses all three
overrides together:

```toml
[patch.crates-io]
drasi-core = { path = "../drasi-core/core" }
drasi-query-ast = { path = "../drasi-core/query-ast" }
drasi-query-cypher = { path = "../drasi-core/query-cypher" }
```

The release workspace also lists `drasi-lib 0.9.0` and SDK/FFI packages `0.11.0`;
they are **not selected by those three overrides**. Do not replace the server's
registry library `0.8.9`, SDK/host SDK/FFI `0.10.0`, index/GQL pins or signed ABI
`0.11.0` plugins with workspace siblings. This is not a whole-workspace binary
compatibility claim.

Cargo path entries do not identify a Git revision. Use the exact pushed commit
reported in #933/the backport PR, verify a clean checkout at that SHA before
building, and pin the sibling CI checkout to the same full SHA. A floating branch
or the unchanged package version is insufficient provenance. Recheck the resolved
server graph and lockfile, ensuring one local identity for each selected crate.
Do not move either project's main checkout or overwrite an existing sibling path.

Server delivery and actual default-configuration validation belong to
[drasi-project/drasi-server#202](https://github.com/drasi-project/drasi-server/issues/202).
The owner of
[drasi-project/drasi-server#201](https://github.com/drasi-project/drasi-server/pull/201)
must run the unchanged real Trading REST/SSE/browser gate against that exact
repository-backed source graph. The earlier published-crate overlay pass does not
substitute for this validation. No React layer is declared unblocked here.

## Existing state and recovery limits

Correctly keyed future updates do not remove old contributor-keyed output.
Affected retained queries need explicit **complete reconstruction from
authoritative source data**, preferably in a fresh query/state namespace, with
reaction snapshots/cursors coordinated before switching consumers. Do not guess
old row identity from values, clear only output/outbox records, or assume that
stop/start or same-ID re-creation clears every durable store.

The newer library codec correction in
[#909](https://github.com/drasi-project/drasi-core/pull/909) is not consumed by
this backport. Already-malformed positional outbox records are not repaired.
Recovery must remain explicit and errors visible; no silent record deletion,
fallback guessing or data migration is included.

No main/base ref, existing #810/#909 history, server dependency or plugin is
modified by this branch. It performs no merge, auto-merge, release, publication
or real-data migration.
