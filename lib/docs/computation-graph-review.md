# ComputationGraph branch review

**Branch:** `agentofreality-parallel-computation-graph` (drasi-core), compared with `main`
**Scope:** design and implementation of the ComputationGraph runtime in `lib/` (`lib/src/computation`, `lib/src/management` and the rewired drasi-lib APIs), the computation contracts and plugin crates it depends on, the documentation set, the test suite, and `computation-graph-remaining-work.md`.
**Date:** 28 October 2026

## Summary

The branch is large: 116 commits, 1,418 files, +495k/−35k lines. About 75k lines are new non-test source in `lib/src` and about 80k lines are new tests.

Much of the core is carefully designed:
- **Ownership:** the runtime owns its tasks and keeps their join handles.
- **Fencing:** generations and epochs are checked consistently, so an old handle cannot act on a replacement.
- **Atomic query commits:** query output, input progress and delivery sequence are committed together.
- **Explicit acknowledgement and retention:** a disconnected subscriber still holds its history.
- **Defensive decoding:** MessagePack decoding is pre-scanned and bounded.
- **Locks:** no `std` lock is held across `.await` in any reviewed file.
- **Documentation accuracy:** every API, default and configuration key spot-checked in the docs matched the code.

The problems fall into four groups:

1. **Correctness at failure boundaries.** Per-component conditions propagate with `?` until they stop the whole graph or drop other callers' requests. Reconciliation reports "not committed" as `Ok`. `DrasiLib::start()` succeeds while queries fail. Quiesced components show as Running. A requested re-snapshot changes results outside the sequenced output history.
2. **Structure.** Very large modules and functions, validation logic duplicated across entry points (the copies have already drifted), event loops and pipe receivers copied and diverging, and state spread over parallel sets and boolean flags.
3. **Public API.** It is not yet suitable for an exemplary, semver-stable crate:
   - 44 glob re-exports;
   - almost no `#[non_exhaustive]`;
   - `anyhow`/`String` errors and panicking methods on public facades;
   - `Legacy*` types in the new `v1` namespace;
   - breaking changes with no deprecation path, version bump, CHANGELOG or migration guide.
4. **Process and documentation.** The branch is too large to review as one PR. CI was switched to serial testing. Generated artifacts are committed stale. The docs sprawl, and several read as internal qualification logs pinned to the feature branch. The remaining-work list does not say which items block a merge.

### Top priorities

| # | Issue | Severity |
|---|---|---|
| H1 | Per-component errors stop the whole graph or silently drop queued start/stop requests | High |
| H2 | Reconciliation executes a recomputed plan without comparing it with the approved preview | High |
| H3 | A reconciliation that did not commit returns `Ok` with a "Completed" summary | High |
| H4 | `DrasiLib::start()` returns `Ok` when queries fail, and leaves `is_running()` true after errors | High |
| H5 | Quiescing/Quiesced components are reported as `Running` | High |
| H6 | A requested re-snapshot changes result rows without sequenced output | High |
| H7 | Unbounded lineage depth: recursive drop can overflow the stack; encode cost grows quadratically | High |
| H8 | Public manager and graph methods panic (`expect`) on reachable runtime errors | High |
| H9 | Public API surface is uncontrolled (globs, no `#[non_exhaustive]`, untyped errors) | High |
| H10 | Breaking changes without deprecation, version bump, CHANGELOG or migration guide; README claims removed APIs still exist | High |

## How to read this document

**Severity**
- **High:** a correctness, data-integrity or availability defect, or an issue that should block a stable release.
- **Medium:** a real defect with limited impact, a significant design or performance problem, or a maintainability risk likely to cause defects.
- **Low:** a localized defect or design improvement.
- **Nit:** style and idiom.

**Confidence**
- **Confirmed:** traced in the code.
- **Plausible:** the mechanism is in the code, but the impact needs a test to demonstrate.
- **Verified:** the item was re-checked directly in the code while writing this document.

**Method.** This is a static review: Rust is not installed on the review machine, so nothing was compiled, linted or tested. The code was read by subsystem (graph controller, graph model and reconciliation, query path, pipes and delivery, component contracts and plugins, runtime and public API, docs and tests). Findings were then merged and de-duplicated. Line numbers refer to branch HEAD and may drift. A `cargo clippy --all-targets` and test run would complement this review.

---

## High severity

### H1. Per-component errors stop the whole graph or drop other requests
**Confirmed; verified.** Location: `lib/src/computation/v1/graph/controller.rs`.

- **The main loop propagates every error.** It runs `advance_additions(graph)?`, `bind_ready_dependencies(...).await?`, then `advance_drains(..)?`, `advance_start(graph)?`, `advance_stop(graph)?` and `complete(..)?` (around 5378–5397 and 5455–5466).
  - Inside those, per-component conditions return `Err`: a busy instance lease (`take()?` returns `OperationInProgress`), a stale completion (`Err(GraphError::StaleGeneration)`), or a peer attach failure.
  - `run()` returning `Err` sends `ComputationGraph::execute` into `cleanup()`, which stops every component.
- **Group loops drop work.** Start, drain and stop groups are drained with `for group in std::mem::take(&mut self.starting) { self.advance_start_group(graph, group)?; }` (around 3972).
  - An error on one group drops every later group. Their reply senders are dropped (callers see `ControllerClosed`), and their pending and `suspended_starts` bookkeeping is lost.
  - Inside the nested reconcile loops the controller keeps running without those groups.
  - `begin_stop_reply` can also leave earlier indices half-mutated.

**Fix**
- Keep `?` only for genuine controller invariants.
- Drop stale completions with a debug log.
- Record lease, peer and per-component errors through `fail_operation` on that component.
- Process each group in isolation: reject that group's reply with its error and re-queue the unprocessed groups.
- Add a test where a reconciliation holds a lease while a start group advances.

### H2. Reconciliation executes a recomputed plan without comparing it with the approved one
**Confirmed structurally; impact plausible.** Location: `graph/reconcile.rs:2356–2436`.

`execute` builds the reservation, conflict check and epoch check from the caller's `supplied` preview. It then runs `let mut plan = preview(graph, supplied.changes)?;` and executes that plan without comparing the two.

`preview()` reads observed state that does not bump the revision:
- resource realization and failure;
- component failures (`Retry`);
- `exhausted`;
- relationship binding.

If any of these changed between preview and reconcile, the executed plan can replace, pause or restart components the caller never approved. Those components are not in the reservation, so concurrent `Start`/`Stop` commands are let through for them. The report does not return the executed sets, so the caller cannot tell.

**Fix**
- Reject with `StaleRevision` unless the recomputed impact sets equal `supplied`, or rebuild the reservation and epochs from the executed plan.
- Return the executed impact sets in `ReconciliationReport`.

### H3. A reconciliation that did not commit returns `Ok`, and cleanup errors hide the original failure
**Confirmed.** Location: `graph/reconcile.rs:2567–2598`, `2700–2730`.

**Not committed reported as `Ok`.** These pre-commit failures all `return Ok(report)` with `committed: false` and `summary: CompletedWithFailures`:
- boundary drain;
- `pause_control`;
- stop failures;
- resource cleanup;
- a `commit_desired` error.

Components that stopped successfully stay stopped. A caller using `GraphControl::connect(...).await?` sees success while the topology is unchanged.

**Original failure lost.** On the abort path, `resume(graph, operations, &plan.pause)?` replaces the report, which holds the real cause, with `GraphError::Cleanup { primary: None, .. }`.

**Fix**
- Return `ReconcileOutcome::{Committed(Report), NotCommitted { stopped, failures }}`, or an `Err` before commit.
- Add `#[must_use]` to the report types.
- Push resume errors into `report.failures`, or set `Cleanup { primary: Some(original) }`.

### H4. `DrasiLib::start()` returns `Ok` when queries fail, and leaves `is_running()` true after an error
**Confirmed; verified.** Location: `lib/src/computation/runtime/mod.rs:1666`, `:1704`; `lib/src/computation/instance_ops.rs:305–307`; `lib/src/lib_core.rs:370`, `:414–433`.

**Query failures are only logged.**
- `start_kind("query")` logs and continues: `if let Err(error) = self.start_component(id.as_str(), "query").await { log::warn!("Query startup failed: {error:#}"); }`.
- The record sweep does the same: `Err(error) if kind == "query" => log::warn!(...)`. Sources and reactions push into `failures` instead.
- The rustdoc for `start()` promises an error if "Any component fails to start".
- On `main`, `QueryManager::start_all` returned `Err`.

**The running flag survives errors.**
- `*self.running.write().await = true` is set right after sources start, even if they failed.
- In `start()`, the flag is set before `management.reconcile()`. A not-ready result returns `Err` with the flag still `true`.
- After a failed `start()`, `is_running()` is `true`, a retry fails with "already running", and partly started sources keep running.

**Fix**
- Push query failures into `failures`.
- Choose one contract and document it: either roll back on error (stop what started, clear `running`), or report a typed partial start (`PartiallyStarted` / `StartReport`).
- Test a failing query, a failing source, and management not becoming ready.

### H5. Quiescing and Quiesced components are reported as `Running`
**Confirmed; verified.** Location: `lib/src/computation/runtime/events.rs:86–107`.

`observed_status` handles Failed, Starting, Stopping and Stopped. Every other lifecycle, including `Quiescing` and `Quiesced`, falls through to `ComponentStatus::Running`.

These states are used during updates, `pause_consumers` and recovery freezes. During them, public status, the event stream and `wait_for_status` all report Running while no work is processed.

**Fix**
- Use an exhaustive `match` on `ComponentLifecycle` with no fall-through arm.
- Add a `Paused`/`Reconfiguring` status. `ComponentStatus` needs `#[non_exhaustive]` first (see H9).

### H6. A requested re-snapshot changes result rows outside the sequenced output history
**Confirmed mechanism; frequency depends on the provider.** Location: `lib/src/computation/v1/query_recovery.rs:353–470` (`run_bootstrap`).

**What happens**
- When `provider.has_pending_snapshot()` is true on an already bootstrapped query, `run_bootstrap` runs again "without resetting previously committed output or its generation".
- Snapshot rows are written to the live-results store (`stage_projection`) and to the in-memory rows, but never to the outbox.
- The result sequence is re-staged unchanged.

**Consequences**
- Sequence-based readers (`replay_after`, `QueryResultReader`, `fetch_outbox`, downstream graph consumers) never see these changes.
- Deletes that happened while the query was down cannot appear, because a snapshot only inserts.
- If a re-snapshotted row is also in the retained outbox, the next restart fails `recover()` with `Inconsistent("snapshot contradicts retained history")`. A Strict query then stays down. An AutoReset query wipes its state and rebuilds.

**Fix**
- Publish re-snapshot differences, including deletes, as ordinary sequenced outputs; or treat a requested snapshot on a completed bootstrap as a reset with a new generation.
- Add a test: bootstrap, commit an output, restart with a pending snapshot that changes that row, then restart again.

### H7. Unbounded lineage depth: recursive drop can abort the process, and encode cost grows quadratically
**Confirmed that depth is unbounded and the drop is recursive (verified); stack overflow at configured limits is plausible.** Location: `components/computation-contracts/src/envelope.rs:218–222`, `:385–395`; `binary_codec.rs:106–143`; `codec.rs:174–181`.

`Lineage { parent: Option<Arc<Lineage>> }` has no depth cap and no custom `Drop`. Every `derive()` adds a level, and both decoders rebuild arbitrarily long chains.

**Stack overflow on drop**
- Decoding is iterative, but the derived drop recurses once per level.
- Frames are allowed to be 64 MiB (plugin ABI) or 32 MiB (network config). At roughly 85 bytes per entry, one frame can hold hundreds of thousands of levels, enough to overflow a 2 MiB tokio worker stack and abort the process.

**Quadratic encode cost.** Without any attacker, every hop in a long pipeline serializes and size-measures the whole chain.

**Fix**
- Enforce a maximum depth in `derive()` and in both decoders.
- Implement an iterative `Drop` that unlinks parents with `Arc::try_unwrap`.
- Consider putting only the immediate parent on the wire.
- Add a test that decodes and drops a maximum-size lineage frame on a 2 MiB stack.
- This belongs under CW-19.

### H8. Public methods panic on reachable runtime errors and swallow others
**Confirmed panics (verified); reachability plausible.**

Locations:
- `lib/src/queries/manager.rs:84–110`, `140–148`
- `lib/src/reactions/manager.rs:84`
- `lib/src/sources/manager.rs:105`
- `lib/src/lib_core_ops/graph_ops.rs:49–52`

**Panicking methods**
- `list_queries`, `get_all_query_labels`, the reaction and source `list_*` methods, and `get_graph` all call `.expect("initialized computation registry")`.
- The underlying calls can fail with:
  - "native component … has no unique instance binding";
  - "component instance has no graph record";
  - "native runtime is not initialized";
  - an error when the parent has been dropped.
- On `main`, `get_graph` could not fail.

**Swallowed errors**
- `get_query_config` uses `.ok()`, and `subscribe_logs` discards errors.
- `get_all_query_labels` drops label-extraction failures that `main` logged.
- `get_query_instance` returns `Result<_, String>`.

**Wider pattern**
- There are 257 `.expect(` calls in non-test computation and management code.
- The workspace lints `unwrap_used` but not `expect_used`, so `expect` is the unchecked loophole.

**Fix**
- Return `Result` from these facades; this is already a breaking release.
- Restore the warning logs.
- Enable `clippy::expect_used = "warn"` for the library, and triage the remaining `expect` calls down to documented invariants.

### H9. The public API surface is uncontrolled
**Confirmed.**

**What is wrong**
- **Glob re-exports.** `lib/src/computation/v1/mod.rs:188–309` has 44 `pub use module::*` lines. Every new `pub` item in about 40 modules automatically becomes public API, including `legacy_reaction::*`, `legacy_source::*` and `legacy_index::*`.
- **No `#[non_exhaustive]`.** Across `lib/src/computation` and `computation-contracts` there are about 106 public enums and 288 public structs, and only a handful of `#[non_exhaustive]` attributes. Affected types include:
  - `GraphError`, `DesiredMutation`, `ControlError`, `ComponentLifecycle`, `ComponentHealth`, `PipeError`, `DrasiError`, `ManagementError`, `ComponentStatus`;
  - option and config structs with public fields: `BoundedPipeConfig`, `SourceTimeMergeDefinition`, `AdmissionOptions`, `ManagementStatus`, `ManagementOptions`, `ReactionPluginOptions`.

  Adding any variant or field is therefore a semver break.
- **Leaked third-party types.** `QueryResultSnapshot.rows` is an `im::HashMap`, so the `im` crate is part of the public API.
- **Forgeable fencing tokens.** `GraphRevision(pub u64)`, `ComponentGeneration(pub u64)` and `OperationEpoch(pub u64)` are host-minted fencing tokens with public constructors.
- **Untyped and inconsistent errors.**
  - `GraphError::Topology { reason: String }` is used at about 300 call sites, including authorization failures.
  - `PipeError::Backend(#[from] anyhow::Error)` carries protocol violations, misconfiguration and storage failures alike.
  - `DrasiError::with_cause` turns every structured variant into `Internal` (18 uses). That breaks the match pattern the crate's own error docs recommend, and turns `RevisionConflict` into `Internal`.
  - `anyhow::Result` and `Result<_, String>` appear on public facades, contradicting `error.rs:46` ("**Never** use `anyhow!()` in public API methods").
  - Adapters convert typed errors to strings (`SubscriptionPhase::Failed(Arc<str>)`, `SourceProgressSnapshot.failure`). Retry classification relies on downcasting, so a retryable `io::Error` becomes Terminal (see M17).
- **Misleading names.** `Legacy*` adapters sit in the new `v1` namespace, and `DrasiLib::computation_plugin_services()` returns `LegacyPluginServices`.
- **Clashing alias.** `v1` re-exports a `Result` alias that clashes with `std::result::Result`.
- **Large, undocumented facade.** `DrasiLib` grew from 63 to 102 inherent methods, many without rustdoc.

**Fix**
- Replace the globs with explicit export lists.
- Add `#[non_exhaustive]` to public enums and option/status structs, with constructors or builders.
- Make newtype fields private.
- Wrap `im` types behind accessors.
- Add structured error variants (duplicate component, cycle, unknown endpoint, permission denied, conflict, protocol violation), with a `kind()` that survives `with_cause`.
- Move the adapters to a `compat`/`#[doc(hidden)]` module, or name them by role.
- Group the `DrasiLib` methods into sub-handles (`computation()`, `management()`).
- Add `cargo-semver-checks` or `cargo public-api` to CI.
- Document what the `v1` namespace promises.

### H10. Breaking changes ship without a deprecation path, and the docs describe APIs that were removed
**Confirmed.**

**False claims in the docs.** `lib/README.md:55–58` says `QueryBase` and `SequenceDedup` "now live under `compatibility`; their original imports remain available". Neither type nor any `compatibility` module exists. `lib/AGENTS.md` and `lib.rs` both say they were removed.

**Breaking changes**
- `QueryBuilder::from_source` and `from_source_with_pipeline` were removed with no `#[deprecated]` shim. The README sample was silently changed to `from_input`.
- `QueryConfig.sources` was renamed to `inputs` and `bootstrap_buffer_size` removed, with `#[serde(deny_unknown_fields)]` and no alias. Existing YAML and JSON configurations now fail to load.
- The root `ComponentGraph` was replaced, the `managers` module removed, and `QueueOrder` made crate-private.
- The `QueryManager::add_query` semantics changed: it now activates immediately, where `main` only provisioned the query.
- `add_query_without_save` became an alias of `add_query`.

**Missing release work**
- `lib/Cargo.toml` is still `0.9.3` and `lib/CHANGELOG.md` is unchanged.
- There is no migration guide. Old runtime state is not converted, according to `computation-graph-backlog.md` and `computation-graph-configuration.md`, so upgraders face a data-reset risk that is not documented for them.

**Fix**
- Bump the version to `0.10.0`.
- Write a curated CHANGELOG section and a `migrating-to-computation-graph.md` covering API renames and removals, configuration keys, persisted-state compatibility and the reset procedure, and Server/plugin version pairing.
- Add `#[deprecated]` wrappers for `from_source*` and `#[serde(alias = "sources")]` for one release.
- Correct the README.

---

## Medium severity

### Graph controller and reconciliation

**M1. The controller event loop is duplicated, and the copies already differ.** *Confirmed.*

Locations: `controller.rs:5433–5468` against `reconcile.rs:1946–2005` and the further `operations.futures.next()` arms at about 2199, 2338, 2482, 2948 and 3689.

Differences between the copies:
- The main loop ignores a retirement failure (`let _ = operations.complete_retirement(..)`) and clears `retiring`. `drive_reconciliation` fails the reconciliation instead and never clears `retiring`.
- `drive_with_deadline` has no arms for resource jobs, peer changes or retirements, so readiness wakeups are lost while a reconciliation waits.

Fix: one `pump_once` step function used by every loop.

**M2. Structural validation is duplicated across entry points, and the copies have drifted.** *Confirmed.*

Rules are validated separately in `build`, `describe`, `validate_definition`, `BindStream`, `addition::insert` and `ControlConnections`. They disagree:
- **`add_component` skips the duplicate output-stream check.** `addition.rs:790–1060` copies `definition.streams` without checking that each stream identity is unique and bound to an output port; `BindStream` and `describe()` reject both. Every later `preview()` then fails with "duplicate output stream", so the graph can no longer be reconciled. Role validation does happen later, at creation (`controller.rs:3507`).
- **`SetTopology` previews never call `validate_definition`.** `commit_desired` silently filters out references to unknown components, and self-loop control connections are accepted (`reconcile.rs:440–476`, `3346–3384`).
- **`DesiredTopology::build` silently drops supplied bindings it doesn't use** (`topology.rs:462–551`), while `prepare` rejects them. A dropped resource handle's async cleanup never runs.
- **Cycle detection ignores subscriptions** (`graph.rs:259`), although the docs say layouts are acyclic. An A↔B subscription, or edge A→B plus subscription B→A, is accepted, and the drain/stop order ignores it.

Fix: one validator over the complete desired model used by every path. Add a property test that anything accepted on one path is also accepted by `describe`.

**M3. `ControlConnections` mutates the live snapshot before a step that can fail.** *Confirmed.*

`controller.rs:5838–5852` assigns `graph.snapshot.subscriptions` (or `control_connections`) and then calls `refresh_connections`. If that fails, the revision is not bumped and nothing is republished, but dependency and readiness logic already acts on the rejected configuration.

The "bump the revision, mutate, publish" sequence is copied four times (5560, 5612, 5838, 6006). Fix: `commit_revision(graph, mutate)`, which validates first and then publishes once.

**M4. Activation has no deadline.** *Confirmed.*

`controller.rs:3712–3730` (`bind_inputs`, `component.start()`, `bind_output_destinations`) and `ConstructionContext::resolve` (3559) are unbounded, while create, reconfigure and stop all use `timeout`.

Other timeout problems:
- A factory-create timeout is reported as `ReconciliationTimeout`.
- Resource construction uses a hard-coded `Duration::from_secs(30)` (`resource_tasks.rs:388`).

Fix: add these to CW-06 with typed timeout errors.

**M5. A stale resource job can make the controller spin.** *Plausible.*

On a generation mismatch, `resource_tasks.rs:523–535` resets `applied = false` and returns `StaleGeneration`, but leaves the job in `jobs`. `next()` returns the same ready job on every loop iteration, so the controller spins at full CPU, and `contains(id)` blocks reconstruction.

Fix: retire stale jobs. Add a regression test with a timed-out construction followed by resource removal.

**M6. Every observation update deep-clones the graph state, which makes deploy and stop-all O(N²).** *Confirmed.*

Locations: `controller.rs:1221–1227` (`update`), `registry.rs:61–70`, `inspection.rs:119–131`.

What gets cloned:
- The inspector keeps up to 256 `Arc<ObservedGraph>`, so every `Arc::make_mut` deep-clones.
- Each publish clones `GraphSnapshot`.
- The registry rebuild locks every slot.

`update` is called inside per-component and per-edge loops, which is where the quadratic cost comes from.

Fix: batch publications per controller turn, store `Arc<GraphSnapshot>` and share it, and name these call sites in CW-31.

**M7. Removed components' slots are never reclaimed, and some code still iterates them.** *Confirmed.*

`graph.components` and `graph.nodes` never shrink; removal only does `graph.ids.remove(id)`. `addition.rs:904` and `controller.rs:5789` iterate every slot, so a removed consumer-progress producer can still block new subscriptions. Indices into these vectors are bare `usize` for both components and edges.

Fix:
- Iterate only through `graph.ids`.
- Add `ComponentIndex`/`EdgeIndex` newtypes.
- Use a generational arena such as `slotmap`.
- Covered partly by CW-33.

**M8. A transient control-handler error blocks the component until it restarts.** *Plausible.*

`controller.rs:3072–3086` sets `node.failure` for non-damaging errors as well as damaging ones. `failure.is_some()` gates deferred binding, addition eligibility and auto-start.

Fix: separate the "last diagnostic" from a blocking failure.

**M9. The controller and reconciler are oversized and state is spread across parallel sets.** *Confirmed.*

Sizes:
- `controller.rs` is 6,622 lines.
- `Operations` has about 40 fields of parallel sets: `quiescing`, `paused`, `resume_after_quiesce`, `suspended_starts`, `stop_after_abort`, `propagated_stop`, `exhausted`, and others.
- `handle_command` is a roughly 600-line match.
- Other long functions: `preview` (about 720 lines), `commit_desired` (about 565), `execute_reserved` (about 470), `ComputationGraphBuilder::build` (about 490).

M1–M3 come directly from this. Fix:
- Split into a lifecycle scheduler, control drivers, a creation queue and one handler per command.
- Use one run-state enum per component.
- Separate pure stages: mutate → validate → impact → execute phases.

### Query, time merge and transactions

**M10. Durable time merge rewrites its whole buffer on every input, emit and confirmation.** *Confirmed.*

`time_merge/recovery.rs:47–100` re-encodes every buffered envelope into one checkpoint, about three times per event. Total work grows quadratically with buffer occupancy.

Fix: one key per buffered entry, written once and deleted on confirmation; or at least keep the already-encoded bytes. Related: `transform` makes about five codec passes plus a SHA-256 per event (`time_merge.rs:780–787`, `913–918`).

**M11. Time merge reorders events within one producer.** *Confirmed behaviour; whether it is acceptable is a design decision.*

The buffer key is `(time, rank, seq)`. A source may go backwards by up to `reorder_window_ms` without being treated as late, so an update followed by a delete from one source can be emitted delete-first. Downstream cannot detect it, because the output gets a fresh logical sequence.

Fix: merge per-source FIFO queues by time (k-way merge), or document this behaviour and make it an option with tests.

**M12. Time merge restore resets deadlines.** *Confirmed.*

`recovery.rs:247,276` sets every restored deadline to `now + max_wait_ms` and resets idleness. Each restart therefore extends worst-case latency.

Fix: persist the admission time.

**M13. Per-input hot-path costs.** *Confirmed.*

- `producer_progress.rs:352–444` reads two checkpoints, JSON-decodes the owner identity and re-stages it for every input, although the query already caches watermarks.
- `progress_key` allocates one `String` per byte.
- Delivery confirmation recomputes the configuration hash (JSON-serializing the query text and settings) per output and commits one transaction per output (`query_delivery.rs:333–372`).
- `transaction_transformer.rs:487,525` validates by a full encode/decode round-trip after every step.
- Every outbox fetch clones the whole retained outbox under the read lock (`query_output.rs:90–107`).

Fix: cache committed progress and identity in memory, write the owner key only when it changes, cache the configuration hash, add a validator walk instead of codec round-trips, and add a `history_after(after)` view.

**M14. Polling instead of notification in several idle paths.** *Confirmed.*

| Location | Poll interval | What it does |
|---|---|---|
| `query.rs:191–209` | 25 ms | Calls `peek_due_time()` on the persistent future queue for every idle query |
| `query_scheduling.rs:146–152` | 100 ms | Scheduled source |
| `wal_source.rs:239–242` | 25 ms | WAL replay source; also reads the whole retained tail |
| `plugin_source.rs:322–331` | 50 ms | Borrowed plugin sources |

Fix: use `Notify`/`watch` on enqueue, sleep until the earliest due time, and keep only a long safety re-check.

**M15. `ContinuousQueryTransformer` is a god object, and its commit logic exists three times.** *Confirmed.*

`query.rs:334–378` has about 45 fields mixing configuration, durable-state caches, delivery state, lifecycle flags and API views. It uses `Mutex`es and atomics even though `transform` takes `&mut self`, only because the commit hook closures borrow `&self`.

The output preparation and atomic/non-atomic staging logic is copied in `process`, `on_wakeup` and bootstrap. Each copy ends in a hand-built table closed by `Ok(_) => unreachable!()`.

The transaction transformer also duplicates the middleware transformer's continuation and guard machinery.

Fix:
- Split into `QueryCore`, `InputLedger`, `OutputLedger` and `QueryPublication`.
- Use one `prepare_output` and one `commit_staged` path.
- Use an exhaustive `enum Commit { Atomic, NonAtomic }`.
- Share a `DurableContinuation` helper between the two transformers.

**M16. AutoReset wipes durable state for ownership mismatches, not just staleness.** *Plausible.*

`query_recovery.rs:171–185` resets on any `Inconsistent` error, including "retained output belongs to another producer identity", which can mean two queries are sharing a storage scope.

Fix: restrict AutoReset to staleness errors. Make identity and ownership mismatches always fatal.

### Component contracts, plugins and middleware

**M17. Adapters erase typed errors, so retry classification goes wrong.** *Confirmed erasure; specific misclassifications plausible.*

`plugin_source.rs:701,810` stores `format!("{error:#}")`, and progress failures are `Arc<str>`; both are re-raised with `bail!("{error}")`. `operation_failure_disposition` (`lifecycle.rs:191–255`) classifies errors only by downcasting, so a retryable `io::Error` or `SourceResetRequired` becomes Terminal, and panic quarantine markers are lost.

Related inconsistencies:
- `LegacySourceFactory::create` marks a permanent "already initialized" bind failure as retryable (`legacy_source.rs:230,447`). The flag is set before `initialize()` runs and is never reset.
- `MiddlewareTransformer::start` poisons the instance permanently on any `recover()` error, while that error may still be classified retryable (`middleware.rs:544–565`).

**M18. Plugin hosts hold async mutexes across long external awaits.** *Confirmed lock; impact plausible.*

- `ReactionPluginHost::start_with_heads` (`plugin_reaction.rs:685`) holds `life` across catalogue readiness waits (300 s by default, twice per query), `reaction.start()`, bootstrap and outbox replay.
- `SourcePluginHost::acquire` (`plugin_source.rs:220`) holds it across create, initialize and start.
- Stop, shutdown, accept and gap recovery all wait on the same lock.
- `PluginObservations::read` (`plugin_services.rs:82–95`) holds `updates` across an unbounded `recv()`. That avoids a deadlock only because callers drop the future first, and a comment in `runtime/reaction.rs:226` records that as an unwritten rule.

Fix: keep short critical sections over a state machine, use a cancellation token so stop can pre-empt startup, and give the observation receiver a single owner.

**M19. Wrapper components silently drop capabilities.** *Confirmed.*

`ComputationComponent` has about 14 defaulted methods. `CheckpointedSink` (`consumer_recovery.rs:895–906`) forwards only `descriptor`, `start` and `stop`. As a result, a wrapped sink:
- loses input binding;
- loses readiness confirmation;
- loses its recovery contract;
- loses batching;
- loses deprovisioning;
- loses configuration and output readers.

None of this causes a compile error.

Fix: put the static capabilities in one `fn capabilities()` struct, or add a delegation macro.

**M20. Component traits mix alternative protocols, so their contracts are easy to break.** *Confirmed.*

`component.rs` has several method pairs that must be implemented consistently, with mismatches surfacing only as runtime `bail!`:
- `EnvelopeSource::admission` versus the required `next`;
- `EnvelopeSink::batching` plus `handle_batch` (defaulting to `bail!`) versus `handle`;
- `binds_inputs` / `bind_inputs`;
- `has_pending_emissions` / `continue_transform`;
- `wakeup_source` / `on_wakeup`.

Fix: return the alternatives as sub-objects or enums (`ingress() -> Pull | Admission`, `input_binder() -> Option<&mut dyn InputBinder>`), or split them into separate traits.

**M21. Duplicate legacy adapters have already diverged from the plugin adapters.** *Confirmed duplication; specific failure plausible.*

`legacy_source.rs` and `legacy_reaction.rs` duplicate `plugin_source.rs` and `plugin_reaction.rs`, and only two tests use them. They behave differently:
- They don't filter `SourceControl::Subscription`.
- They restart at `sequence: 0`.
- They hard-code a channel of 64.

Fix: remove them under CW-63, or reduce them to constructors over the plugin hosts.

### Pipes and delivery

**M22. Every QoS publish and acknowledgement rewrites all channel metadata as JSON (up to 4 MiB).** *Confirmed.*

Locations: `qos_pipe.rs:1185–1218`, `1344`, `1540–1554`; `qos_pipe/shared.rs:247`, `350`; `admission.rs:497`, `514`.

What gets rewritten:
- `Metadata` holds the definition, every subscriber cursor, up to 16,384 admission receipts and 1,024 replay receipts.
- 32-byte digests are written as JSON arrays of numbers.
- With admission enabled, one acknowledgement rewrites megabytes.
- The delivery runner does the same per operation for its ledger (`delivery.rs:593–632`).

Fix:
- Use per-subscriber cursor rows, a head row and a receipt ring.
- Use per-stream delivery ledger rows.
- Serialize digests as bytes.
- Validate only on load.

**M23. One async lock covers the whole QoS channel, including across durable commits.** *Confirmed.*

Publish, complete and receive hold `state` across storage commits and page reads (`qos_pipe.rs:1260–1353`, `1538–1555`, `1842–1889`), so the channel runs one storage round trip at a time.

Client admission uses `try_lock` and returns `Busy` whenever a consumer holds the lock (`admission.rs:414–417`), which gives spurious rejections under normal load.

Fix: an awaited lock for admission, per-subscriber cursors, and commits made outside the lock using a single-writer token.

**M24. Each message is fully re-serialized three or more times per hop just to measure its size.** *Confirmed.*

`encoded_size` is computed in the bounded pipe, the journal budget, batching (which also clones the envelope and port to measure them) and the retained and QoS pipes. Each of those traversals includes the full lineage chain.

Fix: cache the wire size on the immutable `ChangeEvent`, and have the size functions take `(&PortId, &ChangeEnvelope)`.

**M25. The retained pipe delivers one message at a time.** *Confirmed.*

Each delivery costs a durable commit and a JSON decode. The store caches encoded bytes, not decoded envelopes, and `is_idle` decodes a whole envelope just to check whether it is empty (`retained_pipe.rs:195–196`, `540–570`; `retained_store.rs:765–828`).

Fix: cache decoded envelopes, add `has_after`, and use cumulative acknowledgement.

**M26. An ordinary stop/rebind race permanently fences the durable store.** *Confirmed path; impact plausible.*

`retained_store.rs:557–668`: dropping the receiver revokes the generation. A concurrent append then fails its generation check after staging, `WriteFence::drop` fences the store, and the open transaction is not rolled back until shutdown.

Fix: check the generation before `begin()`, roll back explicitly, and fence only when the commit outcome is genuinely ambiguous.

**M27. Receiver and retention logic exists in two or three copies.** *Confirmed.*

The batch and single-delivery receiver machinery (`BatchCollection`, `BatchLease`, `BatchAck`, the `receive_batch` loop) is duplicated between `retained_pipe.rs:329–593` and `qos_pipe.rs:1596–1901`. The copies have already diverged.

The retention-window and byte-budget arithmetic is implemented three times (memory store, indexed store, QoS state). `JournalBudget::apply` trusts that `remove` matches.

Fix: a generic `LeasedReceiver` and one `RetentionWindow` type.

**M28. The wire version is checked only after a strict full decode.** *Confirmed.*

`binary_codec.rs:79–82` uses `#[serde(deny_unknown_fields)]`. A newer frame that adds a field fails with a generic `Decoding` error instead of `Version(n)`, and the persisted JSON format has the same problem.

Fix: decode the header and version first, then dispatch on the version. Add golden-bytes tests for v1.

**M29. Ranked input wakes every task on every change, and one stalled receiver blocks all inputs.** *Confirmed wakeups; blocking plausible.*

`ranked_pipe.rs:406,453` calls `notify_waiters()` on every push and pop. A receiver can make progress only when the global head is its own rank.

Fix: per-binding `Notify`, and either enforce or document a single merged polling task.

### Runtime and instance lifecycle

**M30. Adding a component can race `start()`, leaving it deferred while the instance runs.** *Plausible.*

The add paths read `is_running()` without the lifecycle lock. `start()` takes per-kind snapshots and sets `running` partway through. An `auto_start` query added during `start()` can therefore never start. Per-component start and stop calls also bypass the lifecycle lock (check-then-act).

Five separate serialization points coordinate instance lifecycle, with an undocumented lock order:
- `lifecycle`;
- `mutations`;
- `running: Arc<RwLock<bool>>`;
- the management queue;
- the controller queue.

Fix: one instance-level lock or actor that owns `running` (as a `watch` or atomic), and document the lock order.

**M31. A dropped `Management` leaves a detached task that retries forever.** *Confirmed loop; store-ownership race plausible.*

`management/runtime.rs:937–952` loops `graph.shutdown()` and `store.close()` with a fixed 1-second sleep and no bound or backoff, and nothing owns it. If `Management::open` fails, `build()` returns `Err` while that driver still owns the store, so an immediate retry can hit `AlreadyOwned`.

Fix: close explicitly on the open-failure path, and bound the retry with a deadline and backoff, reporting the outcome.

**M32. `shutdown()` can lose an error and skip closing management.** *Confirmed.*

`lib_core.rs:550–558`: `runtime_shutdown?; computation_shutdown?;` drops the second error if both fail. Any failure skips `management.close()`, leaving the configuration store owned.

Fix: collect all outcomes in `OperationFailures`, and always attempt `close()`.

### Documentation, tests and process

**M33. The branch is too large to review or merge as one change.** *Confirmed.*

It is 116 commits across components (427 files), lib (336) and core (53), with about 378k added lines in `.rs` files. No plan exists to split it.

Suggested decomposition, each part with its own CI gate:
1. core-types and computation-contracts;
2. the graph runtime behind the existing API;
3. adapters and API changes;
4. native SDK, FFI and host;
5. each connector family;
6. docs.

**M34. CI test runs were made serial by this branch.** *Confirmed.*

`.github/workflows/test.yml:40` now excludes `drasi-lib` and `lib-integration-tests` from the workspace run and adds `--test-threads=1`. `lib/tests/run-runtime-parity.sh` adds it to at least 10 of its 35 profiles. Contributors cannot reproduce CI with plain `cargo test`, and quarantine validation depends on the local calendar date.

CW-37 describes this as deferred feedback, but it is a regression introduced here.

Fix:
- Isolate the tests that genuinely share ports, files or globals (for example with `serial_test` or isolated temp directories and ports).
- Restore parallel runs.
- Move to a nextest profile or a `cargo xtask test` with a feature matrix.

**M35. Documentation is pinned to the feature branch.** *Confirmed.*

- There are 13 links to `blob|tree/agentofreality-parallel-computation-graph` across 6 docs.
- The README install snippet is now a path dependency, and 8 places say "in this branch".
- Docs cite specific commit hashes, which a squash-merge will orphan.

All of these break after merge. Fix: point to `main` or tags, restore a crates.io snippet, and add a CI grep guard.

**M36. The documentation set sprawls, overlaps, and reads as internal process logs.** *Confirmed.*

Scale and overlap:
- `lib/docs` is 9.5 MB, with about 20k lines of markdown in 18 files plus a single 5,760-line developer guide.
- At least five documents explain ownership, ports and pipes from different angles.
- Four are dated plans or historical records whose own headers say they are not current.

Session language in user documentation:
- "user decision" and "deferred by the user" appear throughout.
- References to `cleanup-stage4-performance/` "session artifacts" that are not in the repo.
- About 80 hedging phrases such as "not proof" and "is not a fix".

Inconsistent figures: different documents give conflicting test counts.

Fix:
- Keep one user book (mdBook, in chapters), rustdoc as the reference, one maintainer architecture document, and one tracker.
- Move plans, backlog and evidence to `design-documents` or issues.
- Remove the session language.

**M37. Rustdoc coverage is low, and the lint configuration hides problems.** *Confirmed (counts are heuristic).*

- 517 of 776 public items in non-test `lib/src/computation` have no `///`, and 307 of 570 in computation-contracts.
- There is no `missing_docs` lint.
- The workspace sets `unused = "allow"`. In about 85k new lines, dead code and unused dependencies (for example the unused `serde_bytes` in `lib/Cargo.toml:99`) cannot be detected.
- About 300 `unsafe {}` blocks in the FFI crates have no `// SAFETY:` comment.

Fix: enable `missing_docs`, `clippy::undocumented_unsafe_blocks` and `clippy::missing_safety_doc`, and remove `unused = "allow"` for drasi-lib.

**M38. Generated artifacts are committed stale, and the crate would ship them.** *Confirmed staleness; package contents plausible.*

- `runtime-architecture.json` (2.0 MB) omits 15 current source files, and CI never runs its `--check`.
- `runtime-dependencies.csv` is 4.8 MB and the generated HTML is 1.4 MB.
- `lib/Cargo.toml` has no `include`/`exclude`, so about 9.5 MB of docs would be published, which risks the crates.io size limit.

Fix: generate these as CI artifacts or add the check, and add an `include` list.

**M39. Performance is not compared against `main`.** *Confirmed.*

The cited baseline commit `cb155557` is on the branch, not on `main`, and the raw results are "session artifacts" that are not in the repository. Replacing the runtime needs a reproducible before/after comparison against what users run today.

**M40. 40 of the developer guide's 63 Rust blocks are never compiled.** *Confirmed.*

Accuracy is currently good: all 16 sampled examples matched the code. But it is maintained by hand. The other markdown docs are entirely `rust,ignore`. The "first application" starter is 3,500 lines with its own lockfile, and it must sit beside a checkout named exactly `drasi-core`.

Fix: convert the blocks to compiled anchors or `no_run` doctests, and add a 50–100-line in-crate example.

**M41. The unsafe/FFI audit, fuzzing and Miri/loom coverage are missing.** *Confirmed (counts).*

About 970 added diff lines involve `unsafe`, concentrated in `vtable_gen.rs`, `loader.rs`, `export.rs` and `transport.rs`. CW-41 defers the audit. Lib docs mention no Miri, loom or fuzzing.

Specific FFI hardening gaps:
- Exported release, wake and send callbacks do not null-check (`export.rs:884,994,1620`; `transport.rs:126,541`).
- Manual `unsafe impl Send/Sync` have no justification. The ABI says calls are serialized, but the host does not enforce it.
- `pub unsafe fn` helpers let the caller choose any lifetime.

---

## Low severity and idiom

| Area | Issue | Location |
|---|---|---|
| Errors | `Display` repeats `{source}` while also using `#[source]`, so causes print twice; `Cleanup` shows `Debug` text to users | `graph.rs:75,143,177`; `specification.rs:491` |
| Errors | `GraphError::Pipe { edge: 0 }` reports a fake edge index | `topology.rs:293,345`; `reconcile.rs:1737`; `addition.rs:624` |
| Errors | `Aborted` variants are never produced, and errors round-trip `GraphError` → anyhow → downcast | `controller.rs:1856,2070,3016,3042` |
| Errors | Two identical anyhow→`IndexError` carriers | `middleware_recovery.rs:175`; `output_bindings.rs:68` |
| Correctness | `open_run` sets `Starting` before fallible steps, so a failure leaves the graph unstartable | `graph.rs:1860–1868` |
| Correctness | `Unbind` with `RemovalPolicy::Reject` never rejects | `reconcile.rs` Unbind arm |
| Correctness | Resource dependencies on undeclared resources are silently ignored | `resources.rs:55` |
| Correctness | Rejected additions and resources accumulate until dispose; a poisoned-lock path drops the rejection | `controller.rs:5505–5516`; `addition.rs:686–722` |
| Correctness | Reaction adapter has two deprovision paths, and its claim flag is released in `Drop` | `plugin_reaction.rs:1049,1191–1224` |
| Correctness | Dropping a delivery retirement guard retires the owner permanently | `delivery/retirement.rs:122–137` |
| Correctness | `ConsumerProgressSet::changed()` clones receivers, so a loop that never calls `snapshot()` spins | `consumer_progress.rs:137–155` |
| Correctness | A new `Earliest` QoS subscriber can stall the producer under backpressure, with no metric or warning | `qos_pipe.rs:861–899` |
| Correctness | Two status stores per component, and `set_status` is not atomic; rustdoc removed when the type moved | `channels/component_status.rs:226–296` |
| Correctness | Instance node in `get_graph` is hard-coded `Running`, and native transformers are omitted | `runtime/snapshot.rs:95` |
| Correctness | `SourceTransactions` relies on an outer `timeout()` dropping a storage commit midway | `query.rs:1820–1826` |
| API | Builder `with_management` silently overwrites earlier management settings; no `#[must_use]` on builders | `builder.rs:162–193` |
| API | `ManagementStatus.converged` field differs from the `converged()` method | `management/mod.rs:180–193` |
| API | Component kinds are strings (`"source"`, `"query"`), and `_ => Reaction` fallbacks exist | `runtime/mod.rs:1171,1280`; `snapshot.rs:160` |
| API | Inspection topology publishes `Debug` strings as data | `inspection.rs:~280–300` |
| API | Config key casing mixes `queryLanguage`/`outboxCapacity` with `auto_start`, and the factory uses `language`/`outbox_capacity` | `config/schema.rs:570–624`; `query.rs:2041` |
| API | `BroadcastPipe` is a single-receiver drop-oldest ring, not a broadcast | `broadcast_pipe.rs:162–209` |
| API | Workspace dependencies `drasi-computation-*-mssql` have no `version`, so publishing fails | root `Cargo.toml` |
| API | No MSRV (`rust-version`) declared, only an exact toolchain pin | `rust-toolchain.toml` |
| Design | `RelationshipPolicy` is a bag of booleans; the same predicate is re-derived in four places | `lifecycle.rs:720+` |
| Design | Desired-state sets and maps are `Vec`s, so duplicates are caught late and lookups are linear | `topology.rs:78–104` |
| Design | `InstanceSlot` has 7 mutexes and 4 atomics refreshed together, so readers can see a torn mix | `controller.rs:49–64` |
| Design | `build()` duplicates `registry::publish`; nine-argument constructor | `graph.rs:1246–1272` |
| Design | `InputProgress` uses `Deref`/`DerefMut` to imitate inheritance | `query.rs:243–260` |
| Performance | Repeated full edge scans; a 4,096-entry map cloned per input; linear search inside a BFS; batching policy revalidated every loop | `controller.rs:1287–1527,2824`; `input_progress.rs:201`; `consumer_progress.rs:61`; `sink_batching.rs:348` |
| Performance | Each in-flight peer handler wakes on every control-plane change | `controller.rs:2376–2401` |
| Performance | Binary decoder copies every payload instead of slicing the input `Bytes` | `binary_codec.rs:348–353` |
| Performance | Ranked receivers and QoS `check()` take two mutexes per call, three to five times per delivery | `qos_pipe.rs:1003–1025,1517` |
| Idiom | Boolean parameters and boolean map keys (`force`, `deprovision`, `subscriptions`, `management_access`, `pending_auto_start: BTreeMap<usize,bool>`) | `controller.rs`, `graph.rs` |
| Idiom | Three hand-rolled hex encoders, each allocating one `String` per byte | `query.rs:275`; `transaction_state.rs:33`; `plugin_reaction.rs`; `legacy_index.rs:41` |
| Idiom | Port IDs re-parsed from string literals per event (`PortId::try_new("out")?`) | `query.rs:1182,1878`; `plugin_source.rs:897` |
| Idiom | `match Some(command) { … None => unreachable!() }`; `&Option<T>` parameters; needless `mut` on reference bindings | `controller.rs:5523,6059,802` |
| Idiom | `async fn`s with no `.await`; test-only `quiesce_before_stop` in production code | `controller.rs:2029–2064,5114–5198` |
| Idiom | `{ error?; unreachable!() }`; should return `Result<Infallible>` | `plugin_source.rs:857,868` |
| Idiom | `io::Error::new(ErrorKind::Other, e)` should be `io::Error::other(e)` | `control.rs:895` |
| Idiom | Topology version `1` hard-coded in four places; revision increment repeated about six times | `reconcile.rs:441,1316`; `topology.rs:445,464` |
| Idiom | `serde_json` writes `[u8; 32]` digests as number arrays | QoS and delivery metadata |
| Idiom | A boxed future through `async_trait` on every send and receive; offer `try_send`/`try_receive` fast paths | `pipe.rs:107–218` |
| Docs | README make target `build-native-network-plugin` should be `build-native-network-plugins` | `lib/README.md:1264` |
| Docs | Crate docs link to the unpublished `drasi-computation-plugin-sdk` on docs.rs | `lib/src/lib.rs:29` |
| Tests | Test helpers duplicated (`fn id` ×10, `fn definition` ×16); about 56 separate integration test binaries | `lib/tests/*.rs` |
| Tests | Remaining fixed sleeps ("Give query time to process") and absence checks based on time | `reaction_recovery_e2e.rs:469–762`; `computation_query_input_ports.rs:685` |

---

## Remaining-work list: assessment and additions

### Assessment of `computation-graph-remaining-work.md`

- **It doesn't say what blocks a merge.**
  - 42 items: 25 High, 13 Medium, 3 Conditional, 1 Future. There is no "blocks merge / blocks release / later" classification, no owners and no milestones.
  - The introduction says what is *not* a merge prerequisite, but never what *is*.
- **Exit criteria are often unmeasurable.**
  - "Selected" appears 14 times without saying which.
  - CW-34's exit is "changed areas have clear responsibilities".
  - CW-35 refers to review concerns without listing them.
- **Its status contradicts other records.**
  - The headline says the readiness slice is complete, while the requirement ledger (`lib/tests/runtime_parity/requirements.tsv`) shows 20 covered, 23 partial and 5 unqualified. The document never links that ledger.
  - CW-37 calls serial testing deferred, although this branch introduced it.
- **Items for other repositories are mixed in:** Server UI, VS Code type-checks, the Learning Trading test and `createdReaction`, and test-infra lint.

**Recommendation**
- Add **Gate**, **Owner** and **Issue** columns.
- Limit merge gates to about 5–8 items.
- Replace "selected" with explicit scenarios.
- Link the requirement ledger.
- Move cross-repository items to their own trackers.

### Items to add

Each item has a suggested gate: **Merge** (before merging to `main`), **Release** (before a release that advertises a stable API) or **Later**.

| # | Item | Gate | Source findings |
|---|---|---|---|
| 1 | **Contain per-component controller errors.** Remove `?` on per-component and stale conditions in `run`/`advance_*`/`complete`; process lifecycle groups in isolation; add a lease-contention test | Merge | H1 |
| 2 | **Bind reconciliation to the approved preview, and type the outcome.** Compare or reserve from the executed plan; return the executed impact sets; add `ReconcileOutcome::NotCommitted`; keep the primary error when resume fails | Merge | H2, H3 |
| 3 | **Instance start/stop contract.** Report query start failures; roll back or report a partial start; never leave `is_running()` true after `Err`; map Quiesced/Quiescing to a non-Running status; make add/start/stop share one lifecycle lock | Merge | H4, H5, M30 |
| 4 | **Re-snapshot consistency.** Publish requested re-snapshot differences, including deletes, as sequenced output, or start a new generation; add a restart test | Merge | H6 |
| 5 | **Bound lineage and deep values.** Depth cap, iterative `Drop`, decode/drop test on a 2 MiB stack (extends CW-19) | Merge | H7 |
| 6 | **One structural validator.** Every entry point (`add_component`, `BindStream`, `ControlConnections`, `SetTopology`, `build`, `DesiredTopology::build`) uses it, covering streams, roles, references, acyclicity including subscriptions, and leftover bindings; property test | Merge | M2 |
| 7 | **Unify the controller event loop.** One step function for `run` and every reconciliation wait | Merge | M1 |
| 8 | **Panic and error audit on public paths.** `Result` instead of `expect` in the manager facades and `get_graph`; enable `clippy::expect_used`; stop discarding errors with `.ok()`; keep typed causes across adapters (no `Arc<str>` failures) | Merge | H8, M17 |
| 9 | **Release hygiene.** Version `0.10.0`; curated CHANGELOG; migration guide (APIs, configuration keys, persisted state and reset, Server/plugin pairing); `#[deprecated]` shims for `from_source*`; `serde(alias = "sources")`; correct the README | Merge | H10 |
| 10 | **Documentation independent of the branch.** Remove branch links, "in this branch" wording and commit pins; restore the crates.io install snippet; CI grep guard | Merge | M35 |
| 11 | **Restore parallel CI** and a plain `cargo test` path for drasi-lib; remove the calendar-date dependency (re-rank CW-37) | Merge | M34 |
| 12 | **Split the branch** into reviewable PRs, each with its own CI gate | Merge | M33 |
| 13 | **Generated artifacts and packaging.** CI `--check` or stop committing the JSON/CSV/HTML; add `include` to `lib/Cargo.toml`; check the `cargo package` size | Merge | M38 |
| 14 | **API stabilization.** Explicit re-exports; `#[non_exhaustive]`; private newtype fields; hide `im`; structured error variants with a stable `kind()`; rename or hide the `Legacy*` adapters; group the `DrasiLib` methods; `cargo-semver-checks` in CI; document what `v1` promises | Release | H9 |
| 15 | **Rustdoc and lints.** `missing_docs` on `computation`, `management` and contracts; remove `unused = "allow"` for drasi-lib; remove unused dependencies; verify the docs.rs build for the new crates | Release | M37 |
| 16 | **Performance baseline against `main`.** Criterion or `query-perf`, with methodology and results checked in | Release | M39 |
| 17 | **Hot-path storage write amplification.** Per-row QoS cursor, head and receipt rows; per-stream delivery ledger rows; per-entry time-merge storage; cached query progress and identity; cached encoded size; validator walk instead of codec round-trips; notification instead of polling (extends CW-28/CW-32) | Release | M10, M13, M14, M22, M24, M25 |
| 18 | **QoS concurrency.** Commit outside the channel lock; per-subscriber cursors; awaited admission lock; roll back rather than fence on revocation before commit | Release | M23, M26 |
| 19 | **Plugin host locks.** No async mutex held across plugin awaits; stop can pre-empt a hung start; single-owner observation receiver; stop-during-start test | Release | M18 |
| 20 | **Component trait surface.** Capabilities struct or delegation helper (fix `CheckpointedSink`); alternatives as sub-objects or separate traits; written policy on `async_trait` and `Send`/`Sync` | Release | M19, M20 |
| 21 | **FFI safety, fuzzing and model checking.** Promote the CW-41 audit to a release gate; null checks on exported callbacks; `SAFETY` comments plus clippy lints; justify `Send`/`Sync` or enforce serialization; `cargo-fuzz` targets for the envelope, binary, graph, query and wire codecs; Miri for unsafe helpers; loom models for the pipe, acknowledgement and journal primitives | Release | M41 |
| 22 | **Wire and persisted format versioning.** Header-first version dispatch; golden-bytes tests; written migration policy (complements CW-18) | Release | M28 |
| 23 | **Time-merge ordering decision.** Per-producer FIFO k-way merge, or a documented option with tests; persist admission time | Release | M11, M12 |
| 24 | **Activation and construction deadlines.** Extends CW-06; typed `CreationTimeout`; configurable resource timeout | Release | M4 |
| 25 | **Management cleanup bounds.** Close on open failure; bounded retry with backoff; `shutdown()` aggregates every error and always closes | Release | M31, M32 |
| 26 | **Declare an MSRV** and test it; feature-flag matrix (`cargo hack --each-feature`) | Release | Low table |
| 27 | **Decompose the large modules.** Controller (`Operations`, `handle_command`, run-state enum), reconcile (pure stages), `ContinuousQueryTransformer` (ledgers, one commit path), `runtime/mod.rs`, `management::Driver::commit`; shared pipe receiver and `RetentionWindow` (makes CW-34 concrete) | Later | M9, M15, M27 |
| 28 | **Consolidate the documentation.** One mdBook user guide, rustdoc reference and one maintainer architecture document; move plans, backlog and evidence out of the crate; compile the guide examples; minimal in-crate example | Later | M36, M40 |
| 29 | **Test-suite consolidation.** `tests/common`; fewer test binaries; replace remaining fixed sleeps with completion signals | Later | Low table |
| 30 | **Retire the duplicate legacy adapters** (fold into CW-63) | Later | M21 |

## Strengths worth keeping

- **Task ownership.** `Tasks<T>` keeps every join handle owned. Waiters are cancellation-safe, and panics are caught both while polling and while dropping the future. Only one `tokio::spawn` exists in the reviewed scope.
- **Fencing.** Generations, epochs and attachments are checked consistently, so old handles cannot act on replacements.
- **Query atomicity.** Output, input progress, projection and delivery sequence are staged in one transaction. Non-atomic mode is fenced by a durable marker.
- **Delivery semantics.** Acceptance, durability, handling and ambiguity are kept distinct, and fencing fails closed.
- **Decoder hardening.** The MessagePack pre-scan bounds lengths and nesting and rejects trailing bytes.
- **Determinism.** The graph model uses `BTreeMap`/`BTreeSet` throughout.
- **Documentation accuracy.** Every type, signature, default and configuration key that was spot-checked matched the code. The developer book's compiled-excerpt mechanism is a good pattern to extend.
- **Test discipline.** Few sleeps, paused time used widely, every `#[ignore]` justified, and tests that assert behaviour rather than just "no panic".
