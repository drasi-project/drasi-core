# drasi-computation-transformer-optimization

Native ComputationGraph transformer `drasi.standard/optimization`: a declarative
mixed-integer/linear model over the transformer's current input dataset. The model
is re-solved in the background whenever inputs change. Solution, status and
scenario rows are maintained incrementally on a volatile output stream.

The solver is [`good_lp`](https://crates.io/crates/good_lp) with the pure-Rust
`microlp` backend, so the plugin needs no native libraries.

## Behaviour

- **Inputs.** Every input change updates a retained dataset, keyed by element
  reference. No output is emitted from the data path.
- **Debounce.** After a change, a solve is launched once `debounceMs` has passed
  without further changes, and no later than `maxDelayMs` after the first pending
  change. Rapid changes are therefore coalesced.
- **Background solve.** Each solve runs on a background thread over an immutable
  snapshot of the dataset, so the data path is never blocked. The SDK wakeup source
  delivers completed results to `on_wakeup`, which diffs them against the published
  rows and emits deletes, inserts and replaces.
- **Changes during a solve.** If inputs change while a solve is running, its main
  result is still published. Any pending scenario solves are cancelled, and a new
  solve follows.
- **Skipped solves.** A model signature (64-bit FNV-1a over all set members) is
  computed first. If it is unchanged since the last completed solve, nothing is
  re-solved or re-emitted.
- **Deterministic output.** Sets are materialized in key order, and solver output
  is cleaned of numerical noise. Tie-breaking and output identities therefore do
  not depend on input order.
- **Upstream snapshots.** A continuous query keeps its bootstrap results to
  itself, so an optimization fed by queries would otherwise see only rows that
  change after it starts. When the host binds an `output_snapshots` dependency:
  - at the first start, every incoming edge on `in` is read from its producer's
    snapshot, and that dataset is solved and published before any live input;
  - live envelopes that a snapshot already covers (same producer epoch, sequence
    at or below the snapshot's) are skipped;
  - an upstream that restarts (a new producer epoch) is read again, and its
    elements that are gone are removed.

  This uses the SDK's `UpstreamSnapshots`. Without the dependency, behaviour is
  unchanged.
- **Lifecycle.** The output is volatile and owns no durable state.
  - `Pause` stops launching solves and stops publication; `Resume` catches up.
  - `Stop` abandons in-flight work; drain behaves like abort.
  - `Dispose` with erase clears everything.
- **Management.** Model updates are planned through `NativeManagement`.
  - Changing `stream` needs a `Replace` boundary.
  - Every other change is applied in place at a `Pause` boundary.
  - An applied update abandons the running solve and forces a fresh solve.

## Configuration

| Field | Default | Meaning |
|---|---|---|
| `stream` | required | Output stream id. |
| `namespace` | component id | Namespace of emitted elements. |
| `debounceMs` / `maxDelayMs` | 100 / 1000 | Quiet period, and the longest delay after the first pending change. |
| `timeLimitMs` | 2000 | Time limit for each main solve. |
| `scenarioTimeLimitMs` | `timeLimitMs` | Time limit for each scenario solve. |
| `maxRecords`, `maxVariables`, `maxConstraints` | 100000 each | Size limits. Exceeding `maxRecords` rejects the input batch; exceeding the variable or constraint limit gives status `limit`. |
| `maxScenarios` | 64 | Scenario values solved per batch. Values beyond this report status `limit`. |
| `tolerance` | 1e-6 | Relative tolerance used to hold earlier objectives at their optimum. |
| `statusLabel` / `scenarioLabel` | `OptimizationStatus` / `OptimizationScenario` | Labels of the status and scenario rows. |
| `sets`, `variables` | required | Index sets and variable families. |
| `constraints`, `objectives`, `scenarios` | `[]` | The rest of the model. |
| `provenance` | none | `{set, properties?}`. Copies the first member (in key order) of `set` onto the status and scenario rows as a `provenance` map: the listed properties, or all properties not starting with `$`. Null when the set is empty. |
| `preconditions` | `[]` | Input checks that must hold before the model is solved. See [Preconditions](#preconditions-unknown-rather-than-a-guess). |
| `summaries` | `[]` | Solution lists copied onto the status row. See [Summaries](#summaries-the-whole-solution-in-one-row). |

### Sets

Each set has a `name` and takes one of two forms:

- **Records form.** Members are input records with an optional `label` and/or
  `namespace`. They are keyed by the `key` properties, or by `[$namespace, $id]`
  when `key` is omitted.
- **Expansion form.** `from` names an earlier set, and `expand: {count, index}`
  creates `count` members per parent. Each expanded member copies the parent's
  properties and adds `index` = 0..count-1. Expanded members are keyed by the
  parent key plus the index.

Both forms accept a `where` list of conditions, and every condition must hold.
There are three kinds of condition:

- `{property, op, value | valueProperty}`, where `op` is one of `==`, `!=`, `<`,
  `<=`, `>`, `>=`, `in`, `notIn`, `exists` or `notExists`. `valueProperty`
  compares against another property of the same member.
- `{inSet, match: {memberProperty: otherSetProperty}}` holds when some member of
  the other set matches.
- `{notInSet, match}` holds when no member of the other set matches.

There is no join operator. Pair sets, such as eligible workload×GPU combinations,
are produced upstream by a continuous query or a policy plugin. They are then
filtered here.

### Variables

Each variable family is declared as:

`{name, type: binary|integer|continuous, over?, where?, lower?, upper?, output?}`

- `over` gives one variable per member of that set. Without `over`, the family is a
  single scalar variable.
- `lower` and `upper` are a number or `{property, scale?, default?}` read from the
  member. They are not allowed for binary variables. The default lower bound is 0.
- `output` has these fields:
  - `emit` (default true)
  - `label` (default: the variable name)
  - `properties` (default: all member properties except `$…` ones)
  - `includeZero` (default false)

### Terms, constraints and objectives

A term is `{variable, match?, where?, coefficient?}`. It selects the family's
variables that pass `where` and, inside a constraint group, whose `match`
properties equal the group's. The coefficient is either:

- a number (default 1), or
- `{property, scale?, default?, of?}`, read from the variable's member, or from the
  constraint group when `of: "group"`.

A missing property without a `default` is a model error.

A constraint is `{name, forEach? | groupBy?, where?, terms, op: <=|>=|==, rhs}`.
There is one row per filtered `forEach` member, or one row per distinct `groupBy`
tuple of the selected variables' member properties. With neither, there is a
single row. `rhs` is a number or a group property.

`objectives` are solved lexicographically. Each `{name, sense: minimize|maximize,
terms}` is optimized while every earlier objective is held at its optimum, within
`tolerance`. Without objectives, the model is solved for feasibility only.

### Scenarios (what-if)

A scenario is declared as:

`{name, set, property, removeFrom?, removeProperty?, optimize?}`

For each distinct value of `set.property`, the model is re-solved after removing
the members with `removeProperty == value` (default: `property`) from every set
in `removeFrom` (default: `[set]`).

- By default a scenario only checks feasibility.
- `optimize: true` also optimizes the objectives.
- Scenarios run after the main solve on the same snapshot.

To analyse an upstream-defined dataset instead, run a second instance fed by a
different query. This is also how capacity-only variants are run.

### Preconditions (unknown rather than a guess)

A precondition is declared as:

`{name, set, where?, min?, max?}`

It counts the members of `set` that pass `where`; at least one of `min` and `max`
is required. Preconditions are checked in order on the main dataset, after the
sets are materialized. When one does not hold:

- the model is not solved, and the status is `unknown`;
- `detail` names the precondition, the count and the bound, and for `max` the
  first five matching member keys, for example `precondition health-known does
  not hold: 1 member of bins matches, expected at most 0: [EU-B]`;
- the status row's `precondition` property holds its name (null otherwise);
- no solution or scenario rows are published, and `scenarios` is null.

Use them so that a solver never answers from incomplete or untrusted facts:
an input that has not loaded (`min: 1`), a member whose state is unknown
(`where` on that state, `max: 0`), or records that disagree with a context
record (`notInSet` the context, `max: 0`). Preconditions are not re-checked
inside scenarios.

### Summaries (the whole solution in one row)

A summary is declared as:

`{name, variable, where?, properties?}`

While the status is `optimal`, the status row's `summaries.<name>` is a list with
one entry per nonzero variable of the family (every variable with
`output.includeZero`) that passes `where`, in member key order. Each entry holds
the listed member properties (default: the variable's output properties) and
`value`. Otherwise `summaries` is null.

Solution rows arrive as separate changes, and disappear when the result is not
optimal. Consumers that act on a solution as a whole, such as approval
workflows or an idempotent apply, read the summary instead: it changes
atomically with the status. Keep summaries small; they are copied on every
status change.

## Output rows

| Row | Id | Properties |
|---|---|---|
| Solution | `<variable>/<key parts…>` (`<variable>` for scalars) | Selected member properties, `variable`, `value` (integer for binary/integer variables) |
| Status (`statusLabel`) | `status` | `status`, `detail`, `objective`, `objectives`, `signature`, `solutionSignature`, `variables`, `constraints`, `members`, `setSizes`; `precondition`, `summaries`, `scenarios` and `scenariosSignature` when configured |
| Scenario (`scenarioLabel`) | `scenario/<name>/<value>` | `scenario`, `property`, `value`, `status`, `feasible`, `detail`, `objectives`, `signature` |

In ids, `%` is escaped as `%25` and `/` as `%2F`.

- **Status.** One of `optimal`, `infeasible`, `unbounded`, `limit` (a time or size
  limit was reached), `error`, or `unknown` (a precondition does not hold).
- **`signature` and `solutionSignature`.** `signature` identifies the inputs
  (and configuration) that were solved. `solutionSignature` is a 64-bit FNV-1a
  over the published solution rows, null unless optimal. Together they let a
  consumer check that a solution it approved is still the published one, and
  was solved from the same facts.
- **`setSizes`.** Member count of each main set, by name; empty when the sets
  could not be materialized.
- **`scenarios` (status row).** Per scenario: `values` analysed, how many stayed
  `feasible`, and `failures`, a list of `{value, status, detail}` for every value
  that did not (infeasible or unsolved), in value order. The first failure is
  therefore the lowest-sorted loss that leaves no solution. `scenariosSignature`
  is the input signature the roll-up was computed for: like the scenario rows,
  the last completed analysis stays published until the next one replaces it.
  Null before the first analysis and while the status is `unknown`.
- **Infeasible `detail`.** When a constraint row has no eligible variables and
  cannot hold, `detail` names it and its group key, for example `constraint
  assign-once[support] has no eligible variables and cannot hold`.
- **Solution rows.** These exist only while the latest status is `optimal`. Every
  other status retracts them.
- **`provenance`.** Present only when configured. Consumers use it to check
  that a result was solved from the inputs they currently see, for example a
  single `Context` record carrying a version, because the inputs can arrive
  through several upstream queries at different times.
- **`feasible` on scenario rows.** `true` for optimal or unbounded, `false` for
  infeasible, and null otherwise.

## Example: GPU placement

The test `gpu_lab_*` in `src/component/tests.rs` reproduces the gpu-cluster-lab
placement and resilience solver purely through configuration. It uses:

- `Gpu`, `Workload`, `Pair` and `Assignment` input rows.
- Binary `assign` variables over `pairs` expanded by replicas.
- One-GPU-per-replica, memory, compute, peak-utilization and failure-domain
  spread constraints.
- A lexicographic objective: minimize moves from the previous assignment, then
  minimize the peak.
- Worker and region loss scenarios.

## Example: workload placement with approvals

The tests `placement_*` in `src/component/tests/placement.rs` place workloads on
deployments under a policy, health and capacity:

- Inputs from upstream queries: one `PlanContext` (run and revision),
  `PlanItem` (demand and current deployment), `PlanBin` (capacity and health),
  `PlanCandidate` (every workload and deployment pair, joined upstream with
  the demand, current deployment and capacity) and `PolicyResult` (one policy
  decision per pair).
- Binary `assign` over candidates that are allowed (`inSet` the allowed
  decisions), on a healthy deployment, with room.
- Exactly-one and capacity constraints; minimize moves, then peak utilization.
- A `loss` scenario per healthy deployment, read through the status row's
  `scenarios.loss.failures`.
- Preconditions that give `unknown` for unknown health, an undecided policy,
  a candidate without a decision, and inputs from another revision.
- `summaries` of all and of moved assignments, and `signature` /
  `solutionSignature` to detect stale approvals.

## Validation

```sh
cargo fmt -p drasi-computation-transformer-optimization -- --check
cargo clippy -p drasi-computation-transformer-optimization --all-targets -- -D warnings
cargo test -p drasi-computation-transformer-optimization
cargo build -p drasi-computation-transformer-optimization --features dynamic-plugin --lib
```
