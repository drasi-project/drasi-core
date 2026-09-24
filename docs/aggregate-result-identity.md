# Aggregate result identity

A result row represents either a `MATCH` solution or an aggregate group.
Aggregation replaces the match identity with the grouping-key identity, even
when subsequent projections omit the keys:

```cypher
MATCH (p:Position)
WITH p.account AS account, sum(p.value) AS totalValue
RETURN totalValue
```

Changing one contribution updates its group in place; migrating between groups
updates the source and destination independently. Two groups can have identical
projected values without being the same row. Each successive aggregating `WITH`
establishes its own grouping identity. Removes use the before identity;
additions and updates use the after identity.

## Grouping equality and hashing

The aggregate index, lazy extrema sets, and output signatures use the same
grouping-value rules:

- Elements compare by stable `ElementReference`, not mutable properties.
- Integers and integral floats share a group only when their values match
  exactly. `1` and `1.0`, including integer/float zero and negative zero, are the
  same group. Large integers are never rounded through `f64`: integer
  `9007199254740993` is distinct from float `9007199254740992.0`.
- Lists and objects apply these rules recursively. Order matters in lists;
  object keys matter. Strings, booleans, and numbers remain distinct types.

Grouping-equal values must hash equally. These rules are local to grouping and
internal change detection; they do not redefine general query comparisons or
arithmetic. Snapshot/default reconciliation must use the same group identity.

The existing 64-bit, non-cryptographic hash is not a uniqueness proof or a
tenant-isolation guarantee. Collision disambiguation requires a coordinated
index/output/persistence design; changing only final signatures, or using a
random key on every restart, would not provide that guarantee.

## Default transitions and notifications

Internal aggregation contexts retain snapshot/default markers so later query
parts can reconcile their already-applied contributions. These markers are not
proof that a group is empty: a migration destination may already be populated.
Collapsing changes keeps the earliest before side and the latest after side,
including each side's identity and default marker.

At the final output boundary, an existing terminal aggregate with unchanged
values emits no notification. First creation of a zero-valued aggregate and real
nonzero-to-zero changes still emit results. Terminal and some chained aggregates
retain identity-valued rows under existing empty-group semantics; this is not an
empty-group-removal policy change. Future reprocessing must still evaluate
unchanged inputs against the later clock before applying this notification policy.

## Persisted-state upgrades

Correcting identity does **not** automatically migrate previously persisted
state. **Rebuild all affected numeric grouping state, including groups keyed by
ordinary integers.** Integers and equivalent integral floats now use explicit
sign-and-magnitude encoding. This also affects numbers nested in lists or objects,
aggregate-index keys, default/current fingerprints, output identities, and lazy
`min`/`max` set keys. The typed lazy-set encoding also affects nonnumeric grouping
keys. There is no compatibility fallback. Old contributor-keyed output rows can
also remain alongside corrected group-keyed rows.

Reconstruct affected queries completely from authoritative source data,
preferably into a new query/state namespace. Verify the new snapshot before
switching consumers and coordinate downstream snapshots/checkpoints. Do not
guess which old row to remove by matching its value, discard only output rows
while retaining incompatible indexes/checkpoints, or silently reset stored data.

Malformed legacy positional outbox records are a separate concern. The named
writer does not repair them; Strict recovery must keep failing visibly while
preserving those records. See
[persisted outbox compatibility](../lib/README.md#persisted-outbox-compatibility).

## Regression navigation

- [Grouping equality/hash contracts](../core/src/evaluation/variable_value/tests/grouping_test.rs):
  exact numeric boundaries, compound values, and element references.
- [Engine identity and emission cases](../core/src/query/tests/aggregate_update_tests.rs):
  materialized snapshots, key migration, chained aggregates, lazy extrema,
  terminal no-ops, and future reprocessing.
- [Retained multi-source transitions](../core/src/query/tests/retained_multi_source_tests.rs)
  and [part reconciliation](../core/src/evaluation/parts/tests/multi_part.rs).
- [Public snapshots/notifications](../lib/tests/aggregate_snapshot_e2e.rs) and
  [native persistent reopen cases](../lib/src/queries/e2e_checkpoint_tests.rs).
