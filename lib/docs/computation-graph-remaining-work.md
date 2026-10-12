# ComputationGraph consolidated outstanding-work inventory

**Assessment date: 12 October 2026.**
**Status: the single current inventory, including the pre-review backlog and the
design/code-review findings.**

ComputationGraph is implemented and is the only execution runtime on these
branches. Remaining work concerns correctness, failure containment, operation
integrity, qualification, efficiency and separately scoped integrations. This is
not a proposal to replace the runtime again.

This document consolidates the CG-01 through CG-28 backlog, the shared-Core issue
assessment, the runtime design/code review and its Phase 0 follow-ups, the
source-time merger work, and the error-reporting/observability plan. Overlapping
findings are grouped into **CW** work packages. The original identifiers remain
in the [tracking crosswalk](#tracking-crosswalk); they are not additional tasks.
Completed work is recorded separately, not left in the open list.
The 55 stable entries include investigations, qualification, optional decisions
and explicitly retired scopes; they are not 55 active tasks or confirmed defects.

The [maintainer's guide](computation-graph-maintainers-guide.md) explains the
implementation. The [older backlog](computation-graph-backlog.md) and
[reliability plan](computation-graph-reliability-plan.md) retain design and
historical evidence, but are not separate active work queues.

## Contents

- [Scope and evidence](#scope-and-evidence)
- [Completed work and qualification snapshot](#already-complete-do-not-reopen-as-missing-features)
- [Recommended sequence](#recommended-sequence)
- [ComputationGraph runtime integrity](#computationgraph-runtime-integrity)
- [Shared input, evaluator and plugin correctness](#shared-input-evaluator-and-plugin-correctness)
- [Persistence, wire formats and delivery integrity](#persistence-wire-formats-and-delivery-integrity)
- [Error reporting, observation and observability](#error-reporting-observation-and-observability)
- [Efficiency, resource bounds and Rust structure](#efficiency-resource-bounds-and-rust-structure)
- [Assurance and qualification](#assurance-and-qualification)
- [Server, connectors and solution delivery](#server-connectors-and-solution-delivery)
- [Optional capabilities and policy decisions](#optional-capabilities-and-explicit-policy-decisions)
- [Tracking crosswalk](#tracking-crosswalk)
- [Completion and maintenance rules](#completion-and-maintenance-rules)

## Scope and evidence

The assessment uses the existing feature branches after merging `main`:

| Repository | Implementation baseline |
|---|---|
| drasi-core | [`493358f7`](https://github.com/drasi-project/drasi-core/commit/493358f7e11b596f255f921ce5aae8223854b34f) |
| drasi-server | [`f97bb59`](https://github.com/drasi-project/drasi-server/commit/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf) |
| learning-drasi-server | [`7277082`](https://github.com/drasi-project/learning-drasi-server/commit/72770826a214f99b65aa85d092e9b919e08254d8) |
| test-infra | [`55ff7ee`](https://github.com/drasi-project/test-infra/commit/55ff7eeba41a4222c892c75af526a64d9c16b801) |

All four use `agentofreality-parallel-computation-graph`. These commits identify
the merged baseline; the current drasi-core checkout also includes the native
reliability changes recorded below. This describes checkout code, not the
contents or guarantees of a published release.

### Which graph does an item concern?

| Scope label | Meaning |
|---|---|
| **CG** | ComputationGraph execution, reconciliation, pipes, persistence or its ordinary-API adapters under `lib/src/computation/`. |
| **Shared** | The query evaluator, source/reaction bases, storage providers or plugin bridges used by CG. A defect here is not necessarily introduced by CG. |
| **Integration** | Server, UI, connector packaging, tutorials, test infrastructure or host-owned observability. |

The remaining `component_graph::ComponentGraph` is a read-only inspection
facade. Work on its event feed is observation work for the current runtime, not
repair of a second ComponentGraph executor. Ordinary source/query/reaction APIs
also execute through CG.

### Native-only target and work exclusions

**User-directed scope, 11 October 2026:** migrate away from legacy/ordinary
queries, components and plugins, including full migration of Drasi Server.
The target is native ComputationGraph components/query factories and the native
plugin model, not an indefinitely mixed ordinary/native system.

Do not build ordinary-to-native bridges, expand older plugin integration, repair
ordinary-only adapters as a separate investment, or add developer-convenience/
pitfall-prevention APIs and tooling. The engine must enforce containment even
when a plugin is faulty; it must not depend on authors choosing optional helpers.

Shared query evaluation, transactional storage, generation fencing, input/ABI
validation and owned cleanup remain in scope where native execution uses them.
These are runtime correctness mechanisms, not developer-convenience features.
ComputationGraph is already the sole executor: this migration removes older
component/adaptation surfaces, not another running ComponentGraph engine.

| Disposition | Inventory consequence |
|---|---|
| Retire older integration work | CW-15, CW-16 and CW-42 are retained as historical findings, not active work. The ordinary-to-native bridge portion of CW-43 is retired. |
| Keep only native/shared portions | CW-07, CW-10, CW-14, CW-17, CW-18, CW-23, CW-32 and CW-34 must not become ordinary-adapter repair projects. Their ordinary-only examples remain provenance, not tasks. |
| Exclude developer-assistance projects | General API-hygiene, convenience wrappers, advisory lint expansion and shared helpers whose purpose is preventing author mistakes are not deliverables. CW-35 is narrowed to actual native runtime invariants. |
| Qualify the target, not perpetual parity | CW-36 through CW-41 and CW-46 must build evidence for native execution. Do not expand ordinary/native parity matrices or ordinary-only test infrastructure. Existing checks can remain until their code is migrated. |
| Complete native migration | CW-43 retains native reconstruction recipes. CW-44 through CW-47 cover native plugins, full Server migration, native acceptance and native solution delivery. |
| Preserve core correctness | CW-11/CW-12/CW-13 are shared evaluator work used by native queries, not older-query integration. Native lifecycle, persistence and failure containment remain priorities. |

This scope decision does not itself remove code, start the full migration or
authorize data deletion. Define explicit configuration/data cutover behavior
when implementing migration; do not silently discard state or break the current
checkout while it still contains ordinary paths. Existing connector-compliance
and release issue queues are not automatically added to this work.

### Required plugin-failure contract

**User-directed requirement, 11 October 2026:** a plugin's terminal error or
panic must not terminate the process or ComputationGraph. The accepted logical
node remains present in Failed/Error, with its definition, configuration,
diagnostics and cleanup state inspectable without calling the failed plugin.
Failure is not implicit removal.

The controller and repair APIs remain responsive. Required dependents may be
blocked, including an affected shared recovery domain, while unrelated runnable
branches continue. Supported repair can reconstruct a new runtime generation
under the same logical node ID after safe cleanup; it must preserve valid
durable progress and output obligations. A terminal execution failure is not a
permanently unrepairable logical node.

Explicit stop may acknowledge the failure under its documented policy, but
cleanup alone is not repair and only explicit removal deletes the node.
Invalid requests rejected before acceptance are distinct from accepted plugin
declarations whose construction or execution fails.

CW-03/CW-04/CW-10 and the native migration in CW-44/CW-45 must implement this
contract; CW-41 qualifies the native plugin boundary. Do not implement an older
factory API or ordinary SDK bridge to achieve it.
Reporting graph shutdown accurately is not enough.
In-process catch boundaries can contain unwinding panics, not native aborts or
memory faults. Meeting the full native-crash guarantee requires an isolation
architecture that keeps the node and graph owner outside the failed worker;
that mechanism is not selected or implemented here.

### Status and priority

- **Open:** the implementation gap remains in the inspected path.
- **Completed:** the scoped implementation and its stated regression evidence
  are present; broader guarantees remain in their own work packages.
- **Deferred:** still needed, but explicitly excluded from the current work.
- **Partial:** a stated part is implemented; the entry identifies what remains.
- **Investigate:** a plausible mechanism or reported scenario needs a focused
  reproduction or a contract decision. It is not a confirmed production failure.
- **Qualification:** the capability exists; additional observable evidence is
  required for the stated guarantee.
- **Proposed / decision:** a feature or policy is not selected or implemented.
- **Retired:** excluded by the native-only scope. This means not pursued, not
  fixed; IDs and evidence are retained for traceability.

**P1** means address before relying on the affected correctness or recovery
guarantee. **P2** is an important follow-on improvement. **P3** is use-case-driven.
These are priorities, not delivery estimates. A work package can contain a
confirmed gap and a separately identified investigation; they must not be closed
by the same assumption.

Current status was reconciled against implementation changes after the review
and the post-merge evidence. Speculative review findings remain investigations.
Historical line-coverage percentages and issue reproductions are not presented
as newly measured results.

## Already complete: do not reopen as missing features

| Work | Current disposition and remaining boundary |
|---|---|
| PC-01: shared paged-journal trimming | Fixed. Retention uses journal metadata rather than the current payload-cache page. General retention modeling and broader fault schedules remain CW-34/CW-36/CW-39. |
| QX-02: recovery read error authorizes reset | Fixed. A configuration-hash read failure is not treated as evidence of a changed configuration. |
| QX-01: deprovision reuses output identities | Fixed. Recreated query output uses a newer generation. Native deprovision also writes an in-progress marker; QX-04 is narrowed to CW-20, not still a missing-marker defect. |
| LI-01: incomplete instance stop cannot be retried | Fixed. Stop attempts remaining components, retains failures and permits a retry. A richer lifecycle/status contract remains CW-10. |
| PX-12: failure after accepting a reaction row | Fixed. The dead post-enqueue annotation no longer turns accepted work into a failed delivery. |
| GM-01: committed reconciliation reported as rejected | Fixed. Post-commit failures remain in the committed report. CW-01/CW-02 additionally cover precommit integrity and complete resumption attempts. External effects are not an all-or-nothing deployment transaction. |
| CW-01/CW-02: topology commit and producer resumption | Completed. Fallible commit prerequisites precede mutation; resumption attempts every eligible member and preserves all scoped errors. |
| CW-04: returned native failures and control transport | Completed. Terminal errors retain nodes and the controller; structured classification, bounded retry behavior and coalesced graph status are consistent. This is not CW-03 panic containment. |
| CW-05: graceful processing boundaries | Completed for native execution. Admitted input, continuations and graph-owned admission writes drain before stop; deadline failure retains ownership and force cancellation remains explicit. |
| CW-11: source mutation integrity | Completed. Source-time stale changes are reported/skipped, duplicate inserts upsert, live cross-kind collisions fail, and transactional deletion frontiers survive reconstruction. Persisted native query semantics have an explicit incompatible-version boundary. |
| PX-01: undecodable live plugin change is skipped | Fixed. The live change stream fails with a typed error. Initial-snapshot decode loss and missing metrics remain CW-15/CW-26. |
| Failed-component stop and concurrent handle requests | Merge adaptations support explicit stop after recoverable lifecycle failures. Single-component starts/stops queue; a later stop supersedes a queued start. This is only part of CW-07's operation-integrity requirement. |
| Recovery foundations | Whole-path assertions, durable admission, consumer completion, coordinated bootstrap, transactional query state/output, complete source transactions and managed configuration exist. Do not build second implementations for CG-01/02/03/04/07/18. |
| CG-05/06/19 and named configuration contracts | The bounded lifecycle mailbox, explicit storage-listing errors, supported state-store ownership, explicit configuration housekeeping and named M01-M07 contracts exist. Broader lifetime, host and policy work remains separately identified. |
| Source-time merger | Bounded source-time ordering, late-event policies, memory/durable modes and reconstruction exist. It is not a universal stale-mutation guard, source-watermark protocol or global ordering of unseen events. |
| Aggregate identity, expression, serialization and state-store fixes arriving from `main` | Equivalent functionality was already in the feature code. Their merge is not a new closure of the separate aggregate-existence, null-average, snapshot-vtable or reaction-context findings. |

The seven Phase 0 fixes are in
[`70960b95`](https://github.com/drasi-project/drasi-core/commit/70960b9575c5202163fd2338d839cd1f567f7a2d).
They remain in the merged baseline.

### Qualification snapshot

All 28 configured runtime-parity profiles have passing **post-merge baseline** results across
the full run and the eight container-profile reruns. Execution evidence is local
under `target/runtime-conformance/` and
`target/runtime-conformance-containers/`; use the latter for the rerun profiles.
The initial full-run aggregate still records the unavailable-container failures;
the subset rerun is not, by itself, another complete matrix.

The native reliability changes have separate targeted qualification: retained
errors and independent branches on both Tokio flavors; real native cdylib
consumer failures; blocked handlers, durable continuations, journal commits and
stop deadlines; six precommit rejection schedules and coherent successful
publication; an independent 384-mutation model across six RocksDB reconstructions;
and mixed-source grouped joins covering 32 accepted and 96 stale changes through
eight reconstructions. Configuration-version rejection preserves saved rows.
The shared evaluator and RocksDB/Redis-backed conformance suites are also part
of this pass. These are not a rerun of every one of the 28 profiles, a native
process-isolation qualification, or a measured line-coverage percentage.

The task to execute the full compatibility coverage after adding the source-time
merger is complete. Broad qualification is not:

| Ledger state | Contracts |
|---|---:|
| Covered | 20 |
| Partial | 23 |
| Unqualified | 5 |

These are the current [48 requirement rows][requirements], not completion
percentages or 48 independent missing features. Passing existing tests does not
satisfy the additional scenarios described by partial/unqualified rows.
The wider replacement-qualification gate remains unmet.
These counts describe the existing mixed-surface ledger. Native migration must
bind the required contracts to native paths before claiming native-only
qualification; historical ordinary coverage is not a substitute.

<a id="recommended-order-and-completion-rules"></a>

## Recommended sequence

| Order | Work | Why first |
|---|---|---|
| Deferred P1 | CW-03 | Panic containment/isolation is explicitly deferred by the user. Returned-error hardening does not establish the native-crash guarantee. |
| Completed selected slice | CW-01/CW-04/CW-05/CW-11, with coupled CW-02 | Atomic topology commit, retained returned failures, safe graceful boundaries and source mutation integrity are implemented with fault/recovery evidence. |
| Next native runtime work | Native portions of CW-06/CW-07/CW-08/CW-10/CW-17 | Independent deadlines, owned multi-step updates, cleanup, truthful lifecycle surfaces and worker ownership remain. |
| Alongside fixes | CW-12/CW-13/CW-18 through CW-21; native CW-36 through CW-41 | Correct shared semantics and qualify native failure/recovery guarantees, not older integration. |
| Migration | Native CW-43 through CW-47 | Complete native recipes, required plugins and full Drasi Server/solution migration. Do not implement the retired ordinary bridge or factory API. |
| After measurement / separate feature selection | Native/shared CW-22 through CW-35, CW-40 and CW-48 through CW-55 | Add required operability and measured improvements without old-ABI expansion or developer-assistance projects. Core fault diagnostics need not wait for a full exporter project. |

This is not a strict dependency chain or a request to launch parallel sessions.
In particular, version/identity decisions precede rollout of changes to stored
query or aggregate state.

The selected five were **CW-03, CW-04, CW-05, CW-11 and CW-01**.
The user deferred CW-03; the other four are implemented. CW-02 was addressed
alongside safe stop/reconciliation because resumption must attempt every paused
owner. Native reconstruction recipes remain separate migration work, not an
ordinary-to-native bridge.

<a id="correctness-address-first"></a>
<a id="close-the-remaining-runtime-hardening-gaps"></a>

## ComputationGraph runtime integrity

### CW-01: Make desired-topology commit atomic

**CG | P1 | Completed.** `commit_desired` validates generation/index counters,
prepared ordering, every affected slot lease and peer attachments before changing
live topology. It holds the actual peer-topology lock through the non-fallible
mutation phase and releases plugin-backed transports/retired owners outside
that lock. Inspection publishes one coherent desired/observed revision.

**Evidence:** four counter-exhaustion cases, a leased slot and a stale peer
attachment preserve exact previous snapshot/slot/control identities and counters.
A successful commit publishes once and fences the old control attachment.
Existing live reconciliation/churn cases remain in the regression suite.

**Boundary:** earlier draining, stop hooks and external cleanup are not rolled
back. Postcommit realization failures retain `committed = true`. Panics and
process aborts are not made transactional by this change.

**Start here:** [reconciliation][reconcile]. Tracking: GM-07 and
GM-09 (Phase 0 partial-commit follow-up).

### CW-02: Resume every eligible paused producer

**CG | P1 | Completed with CW-05.** Resumption attempts every eligible paused
member, aggregates launch failures with component identities and leaves failed
members visibly quiesced/unavailable. A resume error cannot discard the original
quiesce failure.

**Evidence:** injected operation-epoch exhaustion verifies multiple retained
failures and a healthy member launched after an earlier failure. Graceful-stop
deadline tests verify retained work and successful later retry.

**Start here:** `resume` in [reconciliation][reconcile].
Tracking: GM-10 (Phase 0 resumption follow-up); remaining edge of GM-01.

### CW-03: Contain component panics and publish driver failure

**CG | P1 | Deferred by the user.** Component operations are polled within the controller.
A panic can end the driver without executing its normal failure publication.
Invariant-dependent indexing and `expect` calls also require classification,
rather than assuming every failure deserves a process- or graph-wide response.

**Done when:** plugin/component panics become typed failures on retained logical
nodes. The graph controller, inspection and repair remain usable; unrelated
branches continue. Verify repair under the same logical node ID and rejection
of stale-generation completions. Fence affected shared recovery domains without
removing nodes or terminating the controller. A graph shutdown with a better
error message does not satisfy this contract. Separately protect and report
actual host/controller defects on every driver exit path.

**Start here:** [instance driver][instance], [controller][controller],
[query execution][query]. Tracking: GC-03, GC-06, GC-15, GM-11, QX-11,
RT-09 and the panic portion of LI-11.

### CW-04: Use one failure policy and preserve usable control channels

**CG | P1 | Completed for returned failures.** One exhaustive structured policy
classifies graph work and preserves typed causes through context/report wrappers.
Unknown failures, invalid output, corruption, failed rollback, uncertain pipe
effects and partial fan-out are terminal. A component-returned failure does not
authorize ending the controller, including when a scoped `start()` has no other
runnable members. Accepted failed nodes remain inspectable and repairable.

Control-handler errors retain degraded diagnostics and rearm the inbox.
Graph-generated status coalesces per connected peer/direction/category outside
command queues; coordinated public fan-out remains all-or-none. Stale
attachments invalidate either form of message.

**Retry policy:** a managed reconciliation request permits at most one attempt
per retryable member. There is no autonomous reconstruction retry/backoff loop.
Delivery retains its explicit attempt limit and delay; budget exhaustion is
terminal rather than resetting that budget through reconciliation. Retryability
never proves rollback, successful cleanup or safe same-owner reuse.

**Evidence:** both runtime flavors cover terminal errors/invalid output,
independent branch progress, handler failure followed by another custom message,
and explicit repair. Actual native consumers preserve partial progress while an
independent branch completes. Wrapped causes, native status/flag conflicts,
uncertain outcomes, budgets and saturated control queues have focused tests.
Host/driver panic protection remains CW-03, not completed here.

**Start here:** [controller][controller], [control transport][control],
[delivery runner][delivery]. Tracking: GC-01, GC-04, GC-05, PC-05 and
the retry/classification portion of DP-08.

### CW-05: Pause and stop at safe processing boundaries

**Native CG | P1 | Completed for graceful processing boundaries.** Component,
selection and instance stops quiesce/drain before aborting startup or invoking
stop hooks. Admitted transformer/sink work, continuations, forwarding and
acknowledgement finish together. Graph-owned source admission cancels only its
idle request wait, not an active journal/producer-metadata transaction.

A graceful deadline reports failure without aborting the in-flight operation;
eligible producers resume and cleanup remains retryable. Pending activations
cannot disappear from their start report during drain. `GraphControl::cancel()`
and dropping a run remain explicit force cancellation, which can fence storage
and require reconstruction. No timeout can preempt noncooperative synchronous
plugin code.

**Evidence:** blocked bounded-input handlers on both runtime flavors, stop/restart
with exact outputs, deterministic timeout/retry, real RocksDB continuation
checkpoints, pre/postcommit live replacement, and graceful versus forced native
journal admission/registration with queued requests.

The ordinary plugin subscription-setup investigation is excluded by the
native-only scope; no older integration/helper work is claimed as completed.

**Start here:** [processing loop][graph], [controller][controller],
[plugin source adapter][plugin-source]. Tracking: GC-02, GC-09, DP-01,
PX-04 (setup race requires focused reproduction).

### CW-06: Give lifecycle phases independent, enforceable deadlines

**CG | P1 | Open; CPU-bound deadline behavior needs qualification.**
Construction, settling, startup and cleanup do not have consistently separate
budgets. Slow construction/start can stall control work; one slow cleanup must
not consume everyone else's opportunity to stop. Tokio timeout alone cannot
preempt evaluation that never yields.

**Done when:** expose and document appropriate phase/per-component budgets,
retain cleanup ownership on timeout, and attempt independent cleanup work.
Characterize CPU-heavy source transactions and either add cooperative deadline
checks or state the precise supported limit. Test cancellation during creation
so `Creating` is not left as a misleading final status.

**Start here:** [graph builder/processing][graph], [controller][controller],
[reconciliation][reconcile], [query execution][query].
Tracking: GC-07, GC-09, GC-11, GM-08, QX-06.

### CW-07: Own and compensate multi-step updates

**Native-only scope:** own native reconciliation and accepted management
operations through cancellation. Do not repair the ordinary source/query update
saga as a separate project. RT-01's ordinary-path example below is historical;
apply its operation-integrity requirement to native paths actually retained.

**CG native operations | P1 | Native qualification / hardening.** Single-component
handle requests already queue. Native live changes and accepted management
operations must retain ownership through cancellation, preserve newer user
intent and report applied versus unfinished work. Do not infer that every native
path has the ordinary update saga's defect.

**Done when:** accepted native updates serialize conflicting intent and complete,
compensate where supported, or report precise degraded state. Resume only what
the operation still owns. Qualify native cancellation, failed replacement,
stop/update/remove races and addition during startup; fix counterexamples without
reworking retiring ordinary APIs.

**Start here:** [runtime update operations][runtime],
[instance lifecycle][lib-core]. Tracking: RT-01, RT-02; LI-05 is an
investigation, not an established startup-loss incident.

### CW-08: Make cleanup, management close and rejected resources reliable

**CG management/runtime | P1 | Open.** Several cleanup paths still stop at an
early error or lose another failure. Cancelled management close can leave a
handle that cannot be cleanly retried; failed open/drop cleanup also needs owned,
observable termination. Raw rejected reconciliation must not silently drop
graph-owned resources that require asynchronous shutdown.

**Done when:** cleanup attempts all independent owners, preserves all causes,
and remains retryable after cancellation. Rejection returns supplied resources
or awaits their cleanup. Management open failure cannot race store release;
background cleanup has bounded policy/status instead of an invisible retry loop.
Root and scoped driver shutdown must retain failures across retries.

**Start here:** [management runtime][management-runtime],
[instance driver][instance], [runtime][runtime], [reconciliation][reconcile].
Tracking: GM-02, RT-07, RT-14, LI-03, LI-04, LI-10.

### CW-09: Make topology validation and ownership explicit

**CG | P2 | Open, with contract investigations.** Addition and reconciliation
do not consistently treat dangling subscriptions. Live addition can leave
`allow_incomplete` in exported topology. The `attached/` resource-name convention
can collide with user IDs, and readiness explanations can identify misleading
dependencies.

**Done when:** every supported entrypoint follows the same declared policy;
export/reconstruction cannot silently weaken requested validation. Track attached
resource ownership explicitly or reserve generated namespaces safely. Test
add-before-source, orphaning, export/rebuild and user-chosen IDs.

**Start here:** [addition][addition], [reconciliation][reconcile],
[topology][topology]. Tracking: GM-03, GM-15, GM-09 (original namespace finding),
and GC-14's readiness explanation.

### CW-10: Align status, read handles and public operation outcomes

**Native-only scope:** retain native query/graph status, retained failed-node
inspection, driver outcomes and the migrated Server API contract. Repairs solely
for ordinary/native facade parity, including RT-03/RT-04/RT-05's ordinary paths,
are retired rather than prerequisites for native migration.

**CG native APIs and migrated Server | P1 | Partial.** Native status and driver
failure must be truthful, failed nodes inspectable, and native query reads fenced
by generation. Retain the management convergence/startup outcome work that affects
native operation. Ordinary facade status derivation and
`release_persistent_handles` parity are retired repair objectives.

**Done when:** native direct reads, subscriptions and Server responses use the
same authoritative state. Closed/replaced owners cannot expose another
generation's data. Native start/shutdown reports distinguish partial success,
and not-found/duplicate operations preserve their public category. A retained
failed node remains inspectable without invoking the failed plugin.

**Start here:** [ordinary query adapter][runtime-query],
[inspection][runtime-inspection], [runtime][runtime], [public lifecycle][lib-core].
Tracking: RT-03 through RT-06, LI-02, LI-09, LI-11's instance status,
LI-13's convergence contract.

## Shared input, evaluator and plugin correctness

### CW-11: Reject stale mutations and define duplicate-element behavior

**Shared evaluator | P1 | Completed.** Every evaluated mutation is checked by
source-scoped identity and **source `effective_from` time**. Strictly older
inserts/updates/deletes warn and are skipped; processing continues and native
input progress still commits. Equal-time differences follow admitted order;
identical full elements are no-ops. Insert is full-row upsert, Update keeps sparse
property merging, and a live node/relationship identity conflict fails before
modification. Invalid input timestamps and inconsistent saved frontiers fail
explicitly.

Deletion of even an absent element saves a frontier in the actual result-index
transaction; recreation clears it atomically. Reconstruction therefore cannot
resurrect deleted state from stale input. Frontier records bypass caches without
rollback hooks. There is no automatic expiry without a source watermark/retention
contract.

**Evidence:** stale/current inputs, exact duplicate/upsert aggregates, all six
insert/update/delete orders, source-scoped IDs, equal-time/full-row semantics and
both cross-kind directions; real RocksDB model/reopen, cancellation and mixed-batch
rollback; shared join/aggregate/backend conformance.

**Upgrade boundary:** native query configuration identities are version 5/6 and
include the mutation-semantics version. Earlier identities are rejected without
discarding their rows. Explicit reset/rebootstrap or a separately designed
migration is required, subject to pending-output protection. Live rows alone
cannot reconstruct older deletion history. The source-time merger is not needed
to activate this guard.

**Start here:** [shared evaluator][evaluator].
Tracking: [#244][i244], [#507][i507], [#805][i805].

### CW-12: Preserve real aggregate rows and remove genuinely empty groups

**Shared evaluator/storage | P1 | Open; metadata reclamation needs qualification.**
A real zero-count group must receive its initial Add and survive non-last
contributor removal. An absent group must disappear, even if its numerical
identity happens to be zero. Null-average snapshots must restore correctly.

**Done when:** track contributor existence separately from numeric values;
produce correct Add/Update/Delete changes across nested aggregates and restart.
Cover nulls and supported average types, and verify retired group metadata is
reclaimed under high-cardinality churn. Decide stored-state compatibility before
changing aggregate representations.

**Start here:** [query parts][query-parts], [average][average].
Tracking: [#384][i384], [#498][i498], [#806][i806], [#411][i411].
Candidate work: [PR #409][p409], [PR #907][p907]; neither is a blanket cherry-pick.

### CW-13: Correct matching, scope and list-comprehension behavior

**Shared evaluator/parser | P1 | Open, with broader optional cases to qualify.**
An optional member condition must not incorrectly remove its unmatched team.
Outer variables must remain available where the language permits them.
Membership inside a comprehension must match ordinary membership. A relationship
from A to B must not prove a connection from A to C.

**Done when:** repair the reproduced predicate/scope/comprehension cases and
check relationship endpoints. Exercise independent optional candidates,
converging paths, deletion/reparenting and both supported solver configurations.
Preserve narrow optional cases that already work; do not classify the whole
subsystem as missing or close it from one successful example.

**Start here:** [evaluator][evaluator], [query parts][query-parts],
[path solver][solver]. Tracking: [#795][i795], [#796][i796], [#797][i797],
[#798][i798], [#807][i807], [#808][i808], [#812][i812].
Candidate work: [PR #904][p904], [PR #912][p912], [PR #915][p915].

### CW-14: Enforce source-history and subscription ownership contracts

**Native-only scope:** verify native source progress, retained history and
bootstrap handover. Do not implement the cited SourceBase/ordinary subscription
patches. Those findings are historical evidence of failure classes, not proof
that the native implementation has the same defect.

**Native sources and shared recovery storage | P1 | Native qualification.**
A checkpoint beyond retained source history must not look like successful
catch-up. Resume ownership and initial/live handover must be correct in the
native protocol. The historical crossed-filter/dispatcher findings concern
SourceBase and do not establish identical native defects.

**Done when:** native paths detect unavailable history and violated progress
contracts, bind resume state to the correct owner, and qualify bootstrap/live
handover, cancellation and concurrent subscriptions against actual native
implementations. No ordinary dispatcher/filter patch is part of this work.

**Start here:** [SourceBase][source-base], [plugin source adapter][plugin-source].
Tracking: [#721][i721], [#722][i722], [#723][i723], [#919][i919], [#891][i891].
[PR #913][p913] addresses only the related receiver-readiness portion.

### CW-15: Make plugin snapshot and source discontinuities visible

**Retired ordinary-integration work.** The remaining findings concern the
ordinary bootstrap callback and borrowed ordinary source adapter. They are not
fixed, but will not be pursued as older integration work.

Native bootstrap and streams must still fail explicitly rather than conceal
loss; prove that contract through CW-03/CW-04/CW-39/CW-41 without assuming the
ordinary defect exists in native code or adding another compatibility bridge.

**Historical evidence:** [plugin change/bootstrap receiver][change-receiver],
[ordinary source adapter][legacy-source].
Tracking: PX-13 (Phase 0 bootstrap follow-up), PX-03; remaining boundary of PX-01.

### CW-16: Complete retained plugin ownership and lifecycle contracts

**Retired ordinary-SDK integration work.** Do not expand or repair the older
snapshot vtable, ordinary reaction push context, ordinary subscription response,
subscriptions-complete callback or boolean durability bridge as a separate
investment. Their historical defects are not marked fixed.

Native ABI ownership, terminal-failure handling and lifecycle qualification stay
required under CW-03/CW-04/CW-41/CW-44. Native ownership must be sound without
requiring authors to opt into convenience helpers.

**Historical evidence:** [snapshot proxy][snapshot-proxy], [source proxy][source-proxy],
[reaction proxy][reaction-proxy], [state-store proxy][state-proxy].
Tracking: [#646][i646], [#648][i648], [#649][i649], [#774][i774],
[#815][i815], [#896][i896]; [PR #903][p903]/[PR #906][p906] are candidate work.

### CW-17: Register every worker's owner before spawning

**Native-only scope:** enforce ownership in the native engine, native
components/providers and migrated Server. Do not sweep old SourceBase/ReactionBase
callers or build a developer-facing helper library. Existing internal ownership
primitives are implementation tools for runtime guarantees.

**Native engine/components/providers | P1 | Open contract.** The engine must
retain native work ownership before execution and through termination. The
reviewed ordinary setter/bootstrap/introspection examples are retired-path
findings, not a request to migrate their callers to another helper API.

**Done when:** native scopes cannot overwrite unjoined work, lose cleanup
ownership or reuse resources while old work survives. Cover spawn/setup failure
and concurrent stop. Do not assume arbitrary plugin-spawned work is controllable
because an optional helper exists; require a proven lifetime boundary or the
isolation architecture needed by the containment contract.

**Start here:** [owned workers][workers], [SourceBase][source-base],
[ReactionBase][reaction-base], [introspection source][graph-source].
Tracking: LI-06.

## Persistence, wire formats and delivery integrity

### CW-18: Own and version payload, retry and configuration identity

**Native-only scope:** stabilize the formats and identities used by native
execution. The ordinary config-hash change below is a cutover consideration,
not a request to improve or perpetually support the retiring ordinary adapter.

**CG and shared compatibility surfaces | P1 | Open.** Positional serialization
of evolving Core types can change without changing the graph schema descriptor.
Retry identity depends on storage encoding, and query identity depends on DTO
serialization. Upgrade can therefore refuse replay, trigger a reset or decode
the wrong shape.

**Done when:** use explicit, owned format and identity contracts, version
dispatch before strict decoding, and golden bytes/digests. Define supported
old-format reading or deliberate rejection/rebuild. Keep telemetry and retry
attempt metadata from changing committed business identity.

The merge explicitly adds source rank to ordinary query configuration hashes.
Even unchanged declarations can have a different fingerprint from a pre-merge
store. Qualify that upgrade with its configured recovery/bootstrap policy;
ordinary post-merge restart coverage is not old-state compatibility evidence.

**Start here:** [graph codec][graph-codec], [query codec][query-codec],
[output identity][output-identity], [query configuration][query-configuration],
[ordinary config hash][config-hash]. Tracking: PX-02, PX-07, PX-09, QX-03.

### CW-19: Bound decoded structure and recursive destruction

**CG codecs and shared values | P1 for process-aborting input | Open.**
Byte limits alone do not bound nested lists, lineage or annotation depth.
Recursive decoding or Drop can exhaust the stack even for a bounded payload.

**Done when:** specify depth/count/byte limits before allocating or recursing;
make deep-chain destruction safe; reject oversize/deep input with typed errors.
Exercise construction, decode, validation, cancellation and destruction, including
fuzzed structures and supported plugin boundaries.

**Start here:** [graph codec][graph-codec], [query codec][query-codec],
[envelopes][envelope], [typed values][value-codec].
Tracking: PX-05, PX-06.

### CW-20: Qualify interrupted deprovision across providers

**CG recovery | P1 | Partial / qualification.** Native deprovision now advances
generation and writes an in-progress reset marker. The review's assertion that
no marker exists is obsolete. Some provider clearing operations remain separate
commits, so the larger interrupted-wipe contract still needs explicit evidence.

**Done when:** fail or kill processing before/after each clear and generation/
marker write, reopen each supported provider, and prove that partial state is
never resumed as a valid query. Verify explicit retry/reset completion, including
providers with weaker optional checkpoint methods. Further factoring is CW-34,
not a second request to add the existing marker.

**Start here:** [query recovery][query-recovery], [runtime query][runtime-query],
[recovery cases][query-recovery-tests]. Tracking: narrowed QX-04; QX-01 is closed.

### CW-21: Resolve remaining admission, receipt and acknowledgement edge cases

**CG durable producers/pipes | P1 | Investigate and harden.** These are distinct
review mechanisms, not one confirmed loss incident:

- Merger eligibility is checked with separate clock reads; cross an idle boundary
  and prove that every acknowledged admission was actually saved.
- Consumer replay uses a wall-clock fallback when input lacks a timestamp;
  establish whether supported inputs can reach it and change retry identity.
- Middleware confirmation checks identity/sequence less strictly than the
  merger; verify content before releasing retained output.
- The merger retains only the latest receipt per source; specify/reproduce what
  happens when an upstream replays older acknowledged positions.
- A transaction already fenced before a new write can still produce an
  "unknown acceptance" classification even when that write was never attempted.

**Done when:** each case has an explicit contract and a targeted regression or
documented non-applicability. Distinguish rejected, committed and uncertain work.
Never make old or conflicting input succeed by silently treating it as new.

**Start here:** [merger][time-merge], [consumer recovery][consumer-recovery],
[middleware recovery][middleware-recovery], [QoS pipe][qos].
Tracking: DP-06, DP-07, DP-09, DP-11, PC-09.

## Error reporting, observation and observability

<a id="reconcile-error-reporting-and-consumption"></a>

### CW-22: Reconcile error reporting and consumption

**CG, Shared and Integration | P1 contract; separately scoped implementation |
Open.** Returned errors, graph failures, lifecycle events, logs, metrics and
recovery signals must describe the same incident without becoming competing
status authorities.

**Done when:**

- Distinguish rejected operations, degraded/failed components, recovery activity
  and diagnostic messages. Preserve typed causes and all cleanup failures;
  do not replace them with strings or classify all failures as HTTP 500.
- Correlate instance, graph, component generation and operation. Explain whether
  processing stopped, work is retained, cleanup is incomplete or recovery ended.
- Cover built-in/native plugin components, native queries, driver exits, management and
  failed cleanup. Specify first occurrence, repeated failure and clearing.
- Report lossy ranked admission and stream discontinuities. Publish a skip only
  after cursor advancement succeeds; a warning before failed acknowledgement is
  not evidence of a completed skip.
- Supply useful redacted native causes through Server/UI, not only failure phase.
  Do not turn every error log into a component-state failure.
- Demonstrate a Rust supervisor and an external consumer detecting, diagnosing,
  resynchronizing and routing an alert. Retry/reconstruction policy and alert
  destinations remain explicit.

**Start here:** [public errors][errors], [controller][controller],
[pipe errors/loss][qos], [retained receiver][retained-pipe], [Server routes][server-routes].
Tracking: the pre-review error-consumption item; GM-10 (original cause-loss
finding), PC-04, PC-06, PC-07, PX-10, RT-11, LI-02; supports GC-01/CW-04.

### CW-23: Make observation loss, instance identity and host controls explicit

**Native-only scope:** implement native observation, host-owned failed-node
inspection and migrated Server diagnostics. Do not invest in repairing the
ordinary introspection SourceBase adapter or add convenience APIs merely to
prevent application-author mistakes. Its lag finding remains historical evidence.

**Native inspection, Shared logs and Server | P1 | Open.** Native snapshot/feed
handoff, coalesced updates, revisions and log identity need coherent semantics.
The ordinary introspection source's lag defect is historical evidence, not the
implementation target.

**Done when:** provide an atomic or explicitly resynchronizable snapshot/feed
contract with generation/revision fencing, bounded retention and visible gaps.
Rebuild native observation state after a gap. Use structured log keys and release
instance-owned registry state. Define slow-consumer and reconnect behavior.
Decide whether durable incident history is external or a separate Drasi feature.

Retain the native recovery-completed/skipped/reset notification requirement.
The older introspection SourceBase opt-in builder request is retired; do not
add that convenience API as a step toward native observation. An exporter alone
does not supply the native reporting contract.

**Start here:** [introspection source][graph-source],
[component logs][component-logs], [inspection publication][inspection],
[management runtime][management-runtime].
Tracking: LI-07, LI-08, GM-13; [#632][i632], [#355][i355].

### CW-24: Establish application-owned observability and logging

**Integration across Core, native plugins and Server | P2 | Proposed.**
Implement the [reviewed observability proposal][observability-design] through
`tracing` and `metrics` facades. The embedding application owns subscriber,
recorder, filtering, sampling, collection, export and flush; `drasi-lib` must not
silently install them or create another telemetry engine.

**Done when:** composable logging initialization works with an existing host
subscriber and preserves current history/subscriptions. Define `off`, `basic`,
`debug` and `persistence` profiles, safe fields, instance/queue identity and
bounded cardinality. Preserve existing inspection metrics and the opt-in
Profiler. Resolve unused profiling APIs and dependency modernization deliberately;
do not assert an advisory is fixed merely by moving an exporter.
The `off` profile must not disable existing application/component logs.
Whether context is forwarded without recording spans is a separate host choice.

**Start here:** [tracing layer][tracing-layer], [metrics][metrics],
[component logs][component-logs]. Tracking: observability package A and shared
requirements R1/R3/R4/R5/R8/R9; [#633][i633].

### CW-25: Carry trace context and valid timing through the complete path

**CG, Shared SDK and Integration | P2 | Proposed.** Validate inbound W3C context
at HTTP/gRPC/Kafka and Server control ingress before conversion discards it.
Preserve it across native queues, fan-out, continuations and the native plugin ABI
and actual outbound sends. Retries need new attempt spans, not changed recovery
identity.

**Done when:** actual static/dynamic pipelines retain trace ID and correct
changing parent/span IDs. Bound links for multi-cause batches; do not merge
unrelated historical join traces or split a business transaction for a trace cap.
Carry at most a versioned compact origin sidecar in durable state, never live
spans. Negotiate optional ABI extensions without changing frozen base tables.

Use a defined monotonic clock authority for internal durations, including across
FFI. End-to-end timing ends at actual handler completion, not enqueue or HTTP 202.
Expire timing ownership on cancellation/replacement; do not invent a duration
across process restart. Resolve trace caps, `tracestate`, sampling and replay-link
policy before implementation.

**Start here:** [envelopes][envelope], [plugin source][plugin-source],
[native SDK][native-sdk], [network components][network].
Tracking: observability packages B/D/E; R2/R7/R10.

### CW-26: Implement the shared signal catalogue without double counting

**CG, Shared SDK and Integration | P2 | Proposed.** Standard instrumentation
belongs at the actual source, evaluator, pipe and reaction boundaries. Plugins
reuse host/SDK instrumentation and may add bounded author instrumentation; they
do not each configure exporters.

The initial metrics commitment is:

| Instrument | Type |
|---|---|
| `drasi.component.up` | Gauge |
| `drasi.source.events_dispatched` | Counter |
| `drasi.query.events_processed` | Counter |
| `drasi.query.results_emitted` | Counter |
| `drasi.reaction.results_processed` | Counter |
| `drasi.queue.depth` | Gauge |
| `drasi.queue.depth_max` | Gauge |
| `drasi.queue.blocked_enqueues` | Counter |
| `drasi.reaction.checkpoint_lag_events` | Gauge |
| `drasi.queue.drops` | Counter |
| `drasi.errors` | Counter |
| `drasi.query.engine.duration` | Histogram |
| `drasi.pipeline.end_to_end.duration` | Histogram |

**Done when:** enabled measurements are unsampled and reach aggregation before
lossy export; cache handles outside processing loops. Use bounded component/
source/query/reaction and phase/change/error-kind labels, never payloads, error
messages or trace IDs. Resolve common instance and actual-queue identity rather
than collapsing distinct series. Export durations in seconds; preserve exact
counter, gauge and histogram semantics across FFI. Engine time excludes commit;
bootstrap and live work are distinct.

Standard spans cover source production/dispatch/batching, query receipt/
evaluation/dispatch, actual reaction handling, control operations, bounded
bootstrap supervision and non-empty future drains. Logs preserve typed safe
attributes and originating trace/span context independently of trace sampling.
Completed plugin spans retain their original IDs, times, status, events and links.

Follow-on instruments include plugin up/calls/duration/errors, required stage
latencies and provider interactions, plus the review's decode failures, late/
held/discarded input, capacity refusals, snapshot cost, journal/outbox bytes,
panics/retries and lost logs. Explicitly select additional candidates rather
than treating all proposed future metrics as initial commitments.

**Start here:** [metrics][metrics], [pipe metrics][pipe-metrics],
[query execution][query], [ReactionBase][reaction-base].
Tracking: observability packages C/D/H; R6/R7; PX-01's missing counter.

### CW-27: Provide isolated OTLP and local-telemetry consumption

**Integration | P2 | Proposed.** Supply an optional application-owned adapter
using established OTel aggregation/context machinery. OTLP-only, local
Source/reaction-only and combined operation must work without a network loopback
or a telemetry query becoming a prerequisite for external export.

**Done when:** aggregate once and fan out immutable metric batches/completed
records through independently bounded destinations. Local publication preserves
cumulative totals, histogram buckets, span links and complete approved series
identity. Retention emits deletions. Slow export or monitoring cannot block
business work or silently discard raw metric updates.

Prevent recursive ingestion across async/FFI boundaries; isolate instances and
make dropped records visible. Preserve secrets/payload redaction. Configure
Server startup and explicit drain/flush ownership, demonstrate all three modes,
and measure off/basic/debug overhead against an agreed budget. Monitoring final
shutdown may require a separately owned monitoring instance, never an implicit
second runtime inside the library. Resolve the proposal's local projection and
referenced mobility-design dependency before building incompatible projections.

**Start here:** [observability design][observability-design],
[tracing layer][tracing-layer], [OTel source][otel-source].
Tracking: observability packages F/G and acceptance requirements from A-H.
The new OTel tutorial correlates application telemetry; it does not complete
CW-24 through CW-27.

<a id="performance-and-resource-management-improvements"></a>

## Efficiency, resource bounds and Rust structure

### CW-28: Improve durable source storage and persistent hygiene

**Shared WAL/index providers | P2 | Open / measurement.** Introduce bounded
group commit instead of requiring one durable transaction per source event.
Measure multi-query RocksDB log/disk growth, remove obsolete relationship
adjacency references and qualify high-cardinality aggregate metadata cleanup.
Provide per-query archive selection without changing other users of a provider.

**Done when:** batching has explicit size/latency bounds and acknowledges only
durably committed work. Measure throughput, tail latency, disk growth and crash
recovery together. Historical slowdown claims are not current benchmarks.
Reassess the reported Garnet count race against the present transaction contract
instead of applying its obsolete implementation-specific fix.

**Start here:** [redb WAL][redb-wal], [RocksDB provider][rocksdb],
[index factory][index-factory].
Tracking: [#668][i668], [#782][i782], [#309][i309], [#635][i635],
[#411][i411], [#276][i276].

### CW-29: Extend byte budgets and choose transformer admission behavior

**CG | P2 | Feature / decision, with pressure qualification.** Shared
transactional QoS still reserves by event count. Query outboxes/live rows also
need explicit byte accounting and safe capacity limits. A full merger currently
fails rather than automatically waiting; that is a documented policy, not an
implemented backpressure hook.

**Done when:** define count/byte/oversized-event behavior, then preserve capacity
reservation before the state/output transaction. Add a readiness/backpressure
contract only where selected; distinguish oversized input, full buffers and late
input. Characterize query-catalog fan-out blocking and slow subscribers before
changing its acceptance contract. These limits are not total-process-memory caps.

**Start here:** [shared QoS][shared-qos], [query output state][query-state],
[merger][time-merge], [results catalog][catalog].
Tracking: DP-02, QX-09, QX-12 (investigate); ledger Q08.

### CW-30: Reduce whole-state persistence and replay round trips

**CG and Shared storage | P2 | Open improvement.** Merger/progress snapshots
and QoS receipt metadata can be rewritten in full for small changes.
`WalReplaySource` reads a whole backlog; catch-up and consumer sequence allocation
perform extra durable operations per item.

**Done when:** measure the path, then introduce bounded pages, incremental
records, efficient byte encoding or contiguous replay confirmation where useful.
Keep logical identity, retention floors and uncertain-commit fencing unchanged.
Never batch acknowledgements beyond proven durable handling or move writes
outside their actual shared transaction.

**Start here:** [merger persistence][merge-recovery], [delivery][delivery],
[WAL replay][wal-source], [query delivery][query-delivery], [QoS][qos].
Tracking: PC-03, DP-03, DP-04, DP-07's sequence cost, QX-10.

### CW-31: Publish changes and wake work proportionally

**CG | P2 | Open improvement.** Per-item whole-graph cloning/publication and
event translation grow with graph size. Startup repeatedly traverses dependencies.
Idle WAL/future queries poll; broad change notifications wake unrelated waiters.

**Done when:** batch coherent publications, use suitable structural sharing and
specific notifications, and wait on actual append/due-work changes. Preserve
observable progress, desired/observed revision coherence, fairness and timely
shutdown. Benchmark graph-size scaling and idle overhead, not only throughput.

**Start here:** [controller][controller], [registry][registry],
[runtime events][runtime-events], [query scheduling][query-scheduling].
Tracking: GC-08, GC-14, GM-12, RT-08, RT-13, DP-04, QX-08.

### CW-32: Remove measured hot-path duplication

**Native-only scope:** profile retained native execution paths. Do not optimize
advisory-identity/schema/status round trips that exist only in retiring ordinary
adapters; their review examples do not authorize compatibility work.

**CG and Shared bridges | P2 | Open improvement.** Candidate costs include
repeated schema/status FFI calls, unused advisory identity, repeated validation/
encoding, receipt scans, repeated envelope hashing and per-byte hex allocations.
Canonical float bits recovered indirectly through hashing deserve an explicit
representation contract, not an incidental optimization.

**Done when:** cache immutable metadata at valid lifecycle boundaries; reuse
encoded bytes/digests and focused wakeups without bypassing untrusted-input
validation. Replace duplicated encoding helpers, preserve float/identity golden
vectors and demonstrate unchanged outputs and recovery behavior.

**Start here:** [plugin source][plugin-source], [graph codec][graph-codec],
[QoS][qos], [delivery][delivery], [canonical values][canonical].
Tracking: PC-08, DP-08's duplicate identity work, PX-08, PX-11,
PX-13 (original float-canonicalization finding).

### CW-33: Bound retired topology metadata

**CG | P2 | Open / measurement.** Removed component slots/tombstones can retain
configuration and recovery metadata even after active resources are released.
Flat task/file counts in a bounded replacement soak do not establish bounded
long-lived graph metadata.

**Done when:** measure repeated add/remove with unique and reused IDs, compact
retired slots safely and preserve generation fencing and required diagnostics.
Distinguish leaked data from intentionally retained recovery obligations.

**Start here:** [reconciliation][reconcile], [graph slots][graph].
Tracking: GM-04, GC-12.

### CW-34: Consolidate duplicated algorithms and explicit state machines

**Native-only scope:** refactor only retained native algorithms where this
supports correctness or measured cost. Do not unify retiring ordinary adapters
with native code or extract public helpers to steer developer behavior.

**CG; selected Shared helpers | P2 | Open design work.** The review identifies
duplicated retention windows, durable progress/receipt guards, query commit hooks,
deprovision logic and driver loops. Large controller/reconcile/pipeline functions
hide state across parallel collections and mode booleans.

**Done when:** extract shared mechanisms along existing ownership boundaries,
starting with duplication tied to actual defects. Use explicit component/query
states; remove unreachable construction branches; preserve native execution and
atomic/non-atomic contracts. Do not add a generic abstraction merely to shorten a
file. The older shared adaptive-batcher helper requests are retired; native
batching behavior belongs to the required native connector migration.

**Start here:** [controller][controller], [reconciliation][reconcile],
[query][query], [QoS][qos], [delivery][delivery].
Tracking: GC-10, GM-05, GM-06, PC-02, DP-05, QX-07, RT-14,
PX-14's large methods; [#582][i582], [#584][i584].

### CW-35: Tighten Rust API and ownership invariants

**CG native internals | P2 | Narrowed; developer-assistance portions retired.**
Keep only demonstrated native runtime correctness needs: truthful inspection,
checked arithmetic, actual lock/ownership order and retirement safety.
General API-hygiene/attribute passes, convenience wrappers, module cosmetics and
preventive helper projects are not active deliverables.

**Done when:** the native engine itself enforces the required invariants under
failure, cancellation and malformed plugin input. Preserve fail-closed recovery
freezes and correct resource ownership. Do not turn an unproven bypass or a
developer style preference into a new feature project.

**Start here:** [lifecycle types][lifecycle], [retirement][retirement],
[shared QoS][shared-qos], [runtime inspection][runtime-inspection].
Tracking: GC-13, GM-11's arithmetic, GM-14, GM-16, PC-09's cast, PC-10,
PX-14's API surface, RT-10, RT-12, RT-15, LI-13.

<a id="recovery-and-operational-qualification"></a>

## Assurance and qualification

### CW-36: Make tests prove behavior, not just execute named cases

**CG and Shared | P1 | Qualification improvement.** Protected names/outcomes
are useful but cannot detect weakened assertions. High line coverage also did
not prevent the reviewed failure scenarios.

**Done when:** requirements identify concrete observables and independent
oracles. Add mutation checks, generative retention/delivery/merger models, codec
fuzzing and narrowly scoped concurrency models. Match exact error variants;
replace weak "not yet" assertions with gated boundaries. Evidence markers can
aid reporting but are not themselves proof that an assertion is meaningful.

**Start here:** [qualification tooling][parity-tooling],
[protected contracts][contracts]. Tracking: TS-01, TS-03, TS-04, TS-10;
model checks also support PC-10.

### CW-37: Make timing, completion and test environments deterministic

**Native-only scope:** use deterministic controls and harnesses needed to prove
native runtime behavior. Retire ordinary reaction-loop test repair, ordinary
query test-control integration and developer-facing testing-convenience APIs.
Adapt only the native portions of candidate work when needed for acceptance.

**Native runtime and shared evaluator tests | P2 | Qualification.** Replace
timing guesses in retained native acceptance paths with actual processing/
checkpoint completion. The inspected [query-clock controls][timing-control] are
not integrated; adapt native test functionality only if required. Their frontier
ends at query processing and immediate output acceptance, not completion of
arbitrary downstream effects.

**Done when:** test query input application, controlled due work and actual
native consumer completion separately. Retain current-thread support and the
concurrency/deadlock scenarios required for the native engine; isolate global
state where necessary for reliable evidence. No public testing-helper project or
ordinary reaction-loop repair is required. Shared evaluator range edge cases
remain relevant to native queries; do not merely pin an unsafe panic.

**Start here:** [library tests][lib-tests], [ReactionBase][reaction-base],
[runner checks][runner-checks].
Tracking: TS-02, TS-05, TS-06, TS-08, TS-09; [#245][i245],
[#461][i461], [#482][i482], [#880][i880], [#483][i483].
Candidate work: [PR #246][p246], [PR #887][p887].

<a id="finish-qualifying-the-source-time-merger"></a>

### CW-38: Protect merger contracts and prove real process-crash recovery

**Native-only scope:** protect native merger, middleware, graph and consumer
contracts. Do not add protected obligations solely for retiring ordinary
reaction-recovery or compatibility binaries.

**CG native | P1 | Partial / qualification.** Merger tests already execute in the
normal matrix, but their named contracts are not protected in the contract
inventory. Protect corresponding retained native middleware, graph and consumer
contracts, not retiring ordinary recovery binaries.

**Done when:** bind meaningful cases into the protected inventory. Kill a
separate merger process around input admission, output save and confirmation;
verify exact buffered input, held late events, receipts, replay identity and
output obligations after reconstruction. Existing real-storage reconstruction
and injected cancellation are not every required process-exit case.

**Start here:** [merger tests][merge-tests], [merger recovery tests][merge-tests-recovery],
[contracts][contracts]. Tracking: TS-07 and the source-time merger inventory.
Durable-window cost is CW-30/CW-40, not another missing implementation.

### CW-39: Extend combined recovery and lifecycle scenarios

**Native-only scope:** all new scenarios target native query/component paths
and the providers they actually use. Existing ordinary evidence remains a
baseline record, not a requirement to expand ordinary/native parity.

**CG and Shared providers | P1 | Qualification.** Preserve the existing
transaction, retained-delivery, managed-configuration and crash-recovery
foundations. Extend combinations, rather than implement these mechanisms again.

| Area | Evidence still needed |
|---|---|
| Initial/live query recovery | Independently delayed/failed snapshots, concurrent live changes, missing source progress/history and failed state/output writes through supported entrypoints. T01, T04, S01, S04. |
| Timers and transactions | Whole-query clock jumps with persistence/restart, repeated large bursts plus traffic, and remaining provider/transaction combinations. T06, T07. |
| Live topology | Complete invalid-definition matrix, prolonged unavailable dependencies and combined cancellation/replacement/fan-out schedules. G01, G03. |
| Retained/multicast history | More corrupted record/history combinations and prolonged disconnected consumers, preserving exact required obligations. Q01, Q07, Q08. |
| Consumer effects | Lost replies combined with cancellation and failed progress writes; fresh-trigger and snapshot-maintaining consumers together. R01, R03. |
| Providers and connectors | Exact PostgreSQL/bootstrap/receiver crash boundaries and additional supported products/offset failures. S01-S05, R04; framework evidence is not general connector certification. |
| Managed restoration | M01-M07's named Core/redb contracts are covered. New custom resolvers/provider contracts need their own failure evidence; do not reopen those rows merely because broader Server work remains. |

**Done when:** assert actual rows, saved positions, replayed effects, failure
visibility and released owners against independent expectations.
Tracking: CG-01, CG-20 through CG-24; [#393][i393], [#394][i394],
[#743][i743], [#818][i818]. [PR #745][p745] is candidate scenario coverage,
not evidence that its historical failures remain current.

### CW-40: Measure sustained behavior and current performance

**Native-only scope:** measure native execution and the fully migrated Server.
Do not optimize or requalify ordinary execution or the older HTTP/adaptive
comparison solely to improve retiring components.

**CG and Integration harnesses | P2 | Qualification.** Existing bounded soaks,
fast-path comparisons and workload matrices are useful foundations. They do not
prove many-day persistent operation, native allocation/RSS bounds or production
latency objectives, and their older percentages are not today's measurements.

**Done when:** record current binary/provider/plugin provenance, exact output,
throughput, tail latency, CPU, allocations, disk, tasks and file/socket ownership.
Vary graph/query count, payload, durable buffer/wait size, fan-out and outages.
Refresh relevant cache/provider measurements after the `main` LRU changes.
The historical ordinary HTTP/adaptive observation is not a native defect or an
active measurement task. Use native results to select optimization and
CPU-isolation work.

**Start here:** [workload runner][workload-runner],
[resource soak][resource-soak], [comparison guide][comparison-guide].
Tracking: CG-21, CG-22, CG-27; G06/Q08/T07; [#681][i681].

### CW-41: Qualify independently built plugin lifetimes and compatibility

**Native SDK and Integration | P1 | Qualification.** Broaden malformed metadata/
buffer, incompatible-version, cancellation, revoked-storage, reused-handle,
restart and late-callback schedules. Sustained per-instance DLL ownership remains
unqualified; intentionally process-pinned libraries are not themselves leaks.
Test supported native ABI versions, not expansion of the ordinary plugin ABI.

**Done when:** real separately built libraries preserve supported contracts,
release per-instance state, reject incompatible capabilities before use and
cannot access revoked owners. Include repeated multi-instance churn and native
allocation accounting, not only host/plugin builds from one workspace.

**Start here:** [Host SDK tests][host-tests], [native SDK][native-sdk].
Tracking: CG-05, CG-06, CG-07, CG-25; A01-A05.

<a id="integration-and-productization"></a>

## Server, connectors and solution delivery

### CW-42: Expose ordinary factory-based creation

**Retired by the native-only decision.** Do not add another ordinary
factory/convenience API. Use native graph-owned declarations/factories so a
failed construction remains an inspectable node. Existing factory mechanisms
are a foundation for migration, not something to re-expose through older APIs.

**Historical evidence:** [ordinary operations][ordinary-ops].
Native construction: [addition][addition].
Tracking: CG-08.

### CW-43: Complete ordinary-to-native wiring and reconstruction recipes

**Native recipes retained; ordinary-to-native bridge retired.** Do not implement
mixed ordinary/native query or plugin wiring. Native components/providers,
including the source-time merger, still need the recipes and dependency/version
reporting required to reconstruct complete native Server deployments.

**Done when:** native definitions reconstruct through native factories and
resources, with replacement/restart fencing. Exports distinguish rebuildable
native definitions from opaque application objects and reject missing recipes.
No older-component adapter is part of this completion criterion.

**Start here:** [pipeline assembly][pipeline], [managed configuration][managed-guide],
[Server native configuration][server-computation]. Tracking: CG-10, CG-15.

### CW-44: Select native plugin and connector breadth explicitly

**Native SDK and Integration | P1 for the migration target | Required migration.**
Admission, progress, coordinated bootstrap and consumer completion already exist.
Migrate the required production component/provider set to native plugins.
Complete native services, packaging and registration needed by supported Server
solutions, including required database/network capabilities. Built-in native
queries already exist; migration does not require a dynamically loaded query
engine merely to avoid ordinary queries.

Native HTTP/gRPC parity still needs selected templates, batching, multi-item
gRPC output, snapshot/recovery behavior, advertised TLS and completion-service
exposure. General connector adoption is not implied by framework/reference
implementations.

**Done when:** publish the supported native combinations and verify required
solution behavior, failure containment and recovery without an ordinary
adapter fallback. Reject unsupported configuration. Do not repair every older
plugin or build permanent compatibility bridges before migration. Existing
MySQL/MQTT requests are inputs to native capability selection, not separate
ordinary-plugin improvement projects.

**Start here:** [native SDK limitations][native-sdk], [network components][network],
[Server native configuration][server-computation].
Tracking: CG-02, CG-03, CG-07, CG-11, CG-12; [#44][i44].
Related acceptance follow-ups: [#238][i238], [#301][i301], [#426][i426].

### CW-45: Complete supported Server operations and native UI controls

**Integration | P1 for the migration target | Full native Server migration.**
Move query/component creation, plugin loading, configuration, lifecycle,
inspection, snapshots/results, management and UI operations onto the native
model. Durable desired-state acceptance, receipts and reconstruction are
foundations to retain, not permission to keep ordinary execution underneath.

**Done when:** the supported Server workflows operate through native components
and plugins with no ordinary runtime/adaptation dependency. Preserve one graph
authority, truthful failed-node inspection, revision-safe changes, reconstruction
and required result behavior. Include native editing/lifecycle controls and
accepted/starting/ready/failed presentation. Define configuration/data cutover
explicitly; the existing React package alone is not a completed migration.

**Start here:** [Server routes][server-routes], [native UI][native-ui],
[React package][react-package]. Tracking: CG-04, CG-09, CG-14.

### CW-46: Qualify Server contracts and truthful recovery-test verdicts

**Integration | P1 | Qualification.** Exercise delayed/failed startup, durable
acceptance, native/custom APIs, named snapshots, observer reconnect gaps and
stale generations through REST/SSE. Include the real strict-HTTP rejection,
visible error and catch-up sequence, and the Building Comfort reset/bulk-update
cases rather than assuming a shared engine fix establishes their outcome.
Run these scenarios on the native component/plugin implementations; older
plugin-specific repairs are not a prerequisite or a second acceptance target.

The merged test-infra comparator, producer metadata and SIGKILL schedule are
useful progress. Its documented terminal-boundary verification remains
unfinished: quiet output, ingress drain and snapshot equality do not establish
that all query processing and delivery completed. Scheduled jobs target
`main`/nightly artifacts, not automatically these CG branches.

**Done when:** identify the native query factories, pipe profiles and actual
binary/plugin provenance, and demonstrate no ordinary-adapter fallback.
Establish terminal processing/delivery evidence in baseline and
recovered runs, recapture compatible goldens and enforce the intended state and
delivery verdicts. Bind complete Server workflows into the ledger. Never change
an incomplete capture flag merely to obtain a pass.

**Start here:** [Server tests][server-tests], [recovery comparison][recovery-comparison],
[scheduled recovery boundaries][scheduled-recovery].
Tracking: CG-26; H01-H05; [#791][i791], [#881][i881], [#929][i929],
[#935][i935]. [PR #948][p948] contributes cases, not release/connector proof.

### CW-47: Finish templates, stock-Server examples and tutorial acceptance

**Integration | P3 for packaging; P2 for acceptance coverage | Partial.**
Native resources/topology are not a complete solution-template surface.
The embedded GPU lab works; the stock-Server equivalent remains separate.
The merged OTel tutorial adds application-telemetry correlation, not shared
instrumentation of Drasi itself.
All target templates, examples and tutorial deployments use the native Server
and native plugins, not an older/newer integration layer.

**Done when:** templates preserve instance/resource identity and validate
reconstruction; the stock-Server lab has reproducible setup, readiness and reset.
Extend tutorial automation to exact expected query results and external effects,
with bounded completion and useful diagnostics. Preserve the Getting Started
evaluator's existing result checks; do not reduce acceptance to process startup.

**Start here:** [Server solution docs][server-readme], [stock GPU lab][gpu-server],
[tutorial evaluation][tutorials], [OTel tutorial][otel-tutorial].
Tracking: CG-16, CG-17, CG-28.

<a id="optional-capabilities-and-explicit-design-decisions"></a>

## Optional capabilities and explicit policy decisions

These are not automatic prerequisites for a reliable current runtime. Select the
contract and cost before implementation; documenting a limit does not implement
the optional feature.

### CW-48: Stronger event-time guarantees

**CG / Shared source contracts | P3 | Decision.** Source watermarks could
declare earlier input complete; correction/retraction could repair results after
late input. A strict global time/rank option is also different from the current
tie-break among buffered events.

**Done when selected:** define watermark, idle-source, equal-time, late-event,
replay and downstream correction semantics. Clarify that a lower-rank event
arriving after an equal-time output need not precede it under the current
contract. Never substitute wall-clock receipt time for source change time.
Tracking: DP-10 and the pre-review event-time extensions.

### CW-49: Opt-in poison-input quarantine

**CG query recovery | P3 | Decision.** Deterministic evaluator failure can
recur on restart because fail-closed recovery retains the input. That behavior
does not authorize silently skipping it.

**Done when selected:** specify explicit fail versus quarantine/skip policy,
durable diagnostic identity, progress/output atomicity and operator actions.
Always expose the offending stream/position through CW-22. Keep the safe default
and test that quarantine never silently acknowledges unrelated work.
Tracking: QX-05.

### CW-50: Independent native-node CPU execution

**CG | P3 | Measurement-led decision.** Ordinary queries have independently
driven nested graphs; native nodes in one graph share cooperative scheduling.
Existing measurements do not establish a universal scheduler bottleneck.

**Done when selected:** demonstrate the representative scaling need, then design
controlled execution isolation while preserving ordering, fairness, cancellation
and one authoritative graph owner. CW-06's deadline truthfulness does not depend
on selecting a scheduler rewrite. Tracking: CG-13.

### CW-51: Rate/burst limits and larger acknowledgement windows

**CG pipes/protocols | P3 | Decision.** Capacity bounds retained work, not events
per second. A rate limiter needs wait/reject/drop and burst semantics. More
outstanding deliveries need a proven acknowledgement-latency bottleneck and a
safe contiguous progress contract.

**Done when selected:** specify limits and measurable outcomes, then qualify
fairness, cancellation, replay and bounded receipts. Do not assume current pipes
already provide rate limiting or that widening a window is only a performance
flag. Tracking: pre-review rate/window decision; DP-11's replay-window boundary.

### CW-52: Wider atomic visibility and architectural extensions

**CG and Integration | P3, or P1 for a specifically required guarantee |
Decision.** Complete source transactions are atomic at their declared query
boundary. Atomic visibility across several queries or external effects requires
a larger participant/commit protocol.

Graph feedback cycles, distributed execution, all-or-nothing live-deployment
rollback, unloading libraries during execution and automatic state migration
are separate designs. Exactly-once arbitrary remote effects requires destination
cooperation; a local receipt does not provide it.

**Done when selected:** specify participants, ownership, visibility, failures,
buffering and recovery before changing the current guarantee.
Tracking: CG-18 and the pre-review architectural boundaries.

### CW-53: Shared query-language and value capabilities

**Shared evaluator/parsers | P2/P3 | Separately scoped features/decisions.**
Retain the requests for natural logs/exponentials, GQL list collection, stable
aggregate-list ordering, string min/max, correlated EXISTS, staged GQL matching,
row expansion, supported clause order, UNION, SHA-256, string escapes and
geospatial types. Obtain a reduced non-JSON before-value case before claiming
that entire value family is broken.

**Done when selected:** define incremental result and persistent-state semantics,
share implementation across language registries where appropriate, and add
exact result/restart cases. General ORDER BY/LIMIT/SKIP remain intentionally
unsupported; graph fan-in is not query UNION and middleware unwind is not query
UNWIND. Changing the default language requires its own compatibility decision.

**Start here:** [Cypher registry][cypher-functions], [GQL registry][gql-functions],
[Cypher parser][cypher-parser], [GQL parser][gql-parser].
Tracking: [#211][i211], [#212][i212], [#217][i217], [#357][i357],
[#516][i516], [#598][i598], [#800][i800], [#801][i801], [#802][i802],
[#803][i803], [#804][i804], [#892][i892], [#895][i895], [#916][i916],
[#917][i917]. [PR #218][p218] is candidate scalar-function work.

### CW-54: Provider-atomic compare-and-swap

**Shared StateStore and SDK | P3 | Feature decision.** Managed-configuration
revision checks do not supply a general provider-atomic CAS operation to
components/plugins.

**Done when selected:** define supported value/version comparison and durability,
implement it in actual providers, and negotiate the plugin capability explicitly.
Test concurrent callers and failure/uncertain outcomes; do not implement CAS as
an unprotected get followed by set.
Tracking: [#918][i918].

### CW-55: Choose additional configuration-retention and key policies

**CG management / Shared providers / Integration | P3 | Decision.**
Explicit bounded redb receipt batches, revision-conditional snapshot deletion
and atomic live-record key rotation are implemented. Default indefinite receipts,
including in-memory management, are an intentional contract.

**Done when selected:** define bounded in-memory receipt semantics, automatic
snapshot-age policy, external key-provider integration or secure-erasure/backup
requirements separately. Preserve stale-retry rejection and uncertain-key
fencing. Deleting configuration snapshots must not rewind processing progress;
rotating live records does not re-encrypt old backups.
Tracking: CG-19's optional policies, LI-12.

## Tracking crosswalk

### Original CG backlog

| Original ID | Current disposition |
|---|---|
| CG-01 | Whole-path assertions implemented; broader qualification CW-39. |
| CG-02 | Native admission implemented; adoption CW-44, qualification CW-39/CW-46. |
| CG-03 | Native consumer progress/completion implemented; CW-39/CW-44. |
| CG-04 | Managed configuration implemented; remaining Server surfaces CW-45/CW-46. |
| CG-05 | Lifecycle mailbox fixed; remaining callback/lifetime qualification CW-41. |
| CG-06 | Listing-error and supported state-store ownership contracts fixed; CW-41. |
| CG-07 | Bootstrap settings/context and native coordination implemented; CW-41/CW-44. |
| CG-08 | Retired: no new ordinary factory/convenience API (CW-42). |
| CG-09 | CW-45. |
| CG-10 | Ordinary-to-native bridge retired (CW-43). |
| CG-11 | Completed shared services are not reopened; remaining breadth CW-44. |
| CG-12 | CW-44. |
| CG-13 | CW-40/CW-50; no scheduler rewrite selected. |
| CG-14 | CW-45. |
| CG-15 | Native reconstruction recipes retained (CW-43). |
| CG-16 | CW-47. |
| CG-17 | Embedded lab implemented; stock-Server packaging CW-47. |
| CG-18 | Complete source transactions implemented; wider visibility CW-52. |
| CG-19 | Explicit housekeeping implemented; optional policies CW-55. |
| CG-20 | Named Core/redb contracts covered; broader provider scenarios CW-39. |
| CG-21 | CW-39/CW-40; concrete newly reviewed runtime defects are CW-01 onward. |
| CG-22 | CW-29/CW-36/CW-39. |
| CG-23 | CW-37/CW-39. |
| CG-24 | CW-39/CW-46. |
| CG-25 | CW-41. |
| CG-26 | CW-46. |
| CG-27 | CW-40. |
| CG-28 | CW-47. |

### Complete runtime-review crosswalk

Every review severity is accounted for here, including design/low-priority
findings. Grouping does not make every item a confirmed bug or require the exact
implementation suggested by the original reviewer.
The native-only scope and each item's retirement/narrowing take precedence:
mapping an old finding to an entry does not make ordinary-only repair or
developer-assistance work active again.

Three identifiers were reused in the Phase 0 follow-up notes. In this document
**GM-09 (original)** means the attached-resource namespace, while **GM-09
(Phase 0)** means partial commit. **GM-10 (original)** means stringified causes;
**GM-10 (Phase 0)** means incomplete resumption. **PX-13 (original)** means float
canonicalization; **PX-13 (Phase 0)** means initial-snapshot decode loss.
The unique CW identifiers remove that ambiguity.

| Review group | Disposition |
|---|---|
| Controller | GC-01 -> CW-04; GC-02 -> CW-05; GC-03 -> CW-03; GC-04 -> CW-04; GC-05 -> CW-04; GC-06 -> CW-03; GC-07 -> CW-06; GC-08 -> CW-31; GC-09 -> CW-05/CW-06; GC-10 -> CW-34; GC-11 -> CW-06; GC-12 -> CW-33; GC-13 -> CW-35; GC-14 -> CW-09/CW-31; GC-15 -> CW-03. |
| Reconciliation | GM-01 -> Phase 0 complete, residual CW-01/CW-02; GM-02 -> CW-08; GM-03 -> CW-09; GM-04 -> CW-33; GM-05 -> CW-34; GM-06 -> CW-34; GM-07 -> CW-01; GM-08 -> CW-06; GM-09 original -> CW-09, Phase 0 -> CW-01; GM-10 original -> CW-22, Phase 0 -> CW-02; GM-11 -> CW-03/CW-35; GM-12 -> CW-31; GM-13 -> CW-23/CW-31; GM-14 -> CW-35; GM-15 -> CW-09; GM-16 -> CW-35. |
| Pipes | PC-01 -> Phase 0 complete; PC-02 -> CW-34; PC-03 -> CW-30; PC-04 -> CW-22; PC-05 -> CW-04; PC-06 -> CW-22; PC-07 -> CW-22; PC-08 -> CW-32; PC-09 -> CW-21/CW-35; PC-10 -> CW-35/CW-36. |
| Delivery/merger | DP-01 -> CW-05; DP-02 -> CW-29; DP-03 -> CW-30; DP-04 -> CW-30/CW-31; DP-05 -> CW-34; DP-06 -> CW-21; DP-07 -> CW-21/CW-30; DP-08 -> CW-04/CW-32; DP-09 -> CW-21; DP-10 -> CW-48; DP-11 -> CW-21/CW-51. |
| Query execution | QX-01 -> Phase 0 complete; QX-02 -> Phase 0 complete; QX-03 -> CW-18; QX-04 -> partially addressed, CW-20; QX-05 -> CW-22/CW-49; QX-06 -> CW-06; QX-07 -> CW-34; QX-08 -> CW-31; QX-09 -> CW-29; QX-10 -> CW-30; QX-11 -> CW-03; QX-12 -> CW-29 investigation. |
| Plugins/codecs | PX-01 -> live failure path complete, counter CW-26; PX-02 -> CW-18; PX-03 -> CW-15; PX-04 -> CW-05; PX-05 -> CW-19; PX-06 -> CW-19; PX-07 -> CW-18; PX-08 -> CW-32; PX-09 -> CW-18; PX-10 -> CW-22; PX-11 -> CW-32; PX-12 -> Phase 0 complete; PX-13 original -> CW-32, Phase 0 -> CW-15; PX-14 -> CW-34/CW-35. |
| Runtime facades | RT-01 -> CW-07; RT-02 -> handle queuing implemented, remaining CW-07; RT-03 -> inspection narrowed, remaining CW-10; RT-04 -> CW-10 investigation; RT-05 -> CW-10; RT-06 -> CW-10; RT-07 -> CW-08; RT-08 -> CW-31; RT-09 -> CW-03; RT-10 -> CW-35; RT-11 -> CW-22; RT-12 -> CW-35; RT-13 -> CW-31; RT-14 -> CW-08/CW-34; RT-15 -> CW-35. |
| Library integration | LI-01 -> Phase 0 complete; LI-02 -> CW-10/CW-22; LI-03 -> CW-08; LI-04 -> CW-08; LI-05 -> CW-07 investigation; LI-06 -> CW-17; LI-07 -> CW-23; LI-08 -> CW-23; LI-09 -> CW-10; LI-10 -> CW-08; LI-11 -> CW-03/CW-10; LI-12 -> CW-55 decision; LI-13 -> CW-10/CW-35. |
| Tests | TS-01 -> CW-36; TS-02 -> CW-37; TS-03 -> CW-36; TS-04 -> CW-36; TS-05 -> CW-37; TS-06 -> CW-37; TS-07 -> CW-38; TS-08 -> CW-37; TS-09 -> CW-37; TS-10 -> CW-36. |

### Pre-review topics and observability packages

| Source topic | Current home |
|---|---|
| Stale/duplicate/cross-kind input; aggregate existence/nulls; matching | CW-11/CW-12/CW-13. |
| Source history, resume filters and receiver readiness | CW-14. |
| Runtime follow-ups after Phase 0 | CW-01/CW-02/CW-06/CW-15/CW-26. |
| Merger protection, process exits, durable-window measurements | CW-38/CW-30/CW-40; full compatibility execution is complete. |
| Group commit, shared byte admission, disk hygiene, archive selection | CW-28/CW-29. |
| Recovery, timers, replacement, journals, consumers and sustained workloads | CW-37/CW-39/CW-40/CW-41. |
| Error reporting/consumption, recovery notifications, introspection | CW-22/CW-23. |
| Ordinary factories/bridges and retained ordinary SDK defects | CW-42 and ordinary CW-43/CW-16 are retired, not fixed. |
| Native recipes, full Server migration, plugins, templates/tutorials | Native CW-43 through CW-47. |
| Observability A: contract and ownership | CW-24; reporting contract prerequisite CW-22/CW-23. |
| Observability B: context and clocks | CW-25. |
| Observability C: runtime instrumentation | CW-26. |
| Observability D: native plugin bridge | CW-25/CW-26/CW-41; ordinary ABI expansion retired. |
| Observability E: protocol continuity | CW-25. |
| Observability F: dual output/local Source | CW-27. |
| Observability G: Server integration and qualification | CW-27/CW-46. |
| Observability H: extended instrumentation | CW-26; candidates remain explicitly selected. |
| Optional language, event time, execution, rate/window and wider guarantees | CW-48 through CW-55. |

## Completion and maintenance rules

One completed narrow fix does not close its whole work package. Record the
specific remaining boundary, as for QX-04 and RT-02 above. A supported deliberate
limitation is not a defect; changing it needs a selected contract.

For correctness work, add a regression that fails for the demonstrated reason
before the fix and checks the actual result, saved progress or released owner.
For qualification, update the [requirement ledger][requirements] and
[protected contracts][contracts] with named evidence and its provenance. Do not
promote a passing test name, current line-coverage percentage, quiet receiver or
successful process startup into a stronger guarantee.

For persisted formats, source ordering, aggregate representation and plugin
contracts, record upgrade/rejection/rebuild behavior before rollout. Existing
stored data must not be silently reinterpreted. An opaque or uncertain provider
cannot satisfy a stronger durability assertion by default.

Candidate PRs and the timing branch are reuse leads from the earlier assessment.
Their live GitHub state is not asserted here; the inspected local baseline
determines whether their behavior is present. Recheck the actual patch,
dependencies, stored-state contract and scope before adapting it.

Update this document when implementation or evidence changes. Keep the CW IDs
stable, preserve aliases, and update linked guides for behavior changes instead
of maintaining competing active inventories.

[requirements]: ../tests/runtime_parity/requirements.tsv
[contracts]: ../tests/runtime_parity/computation-contracts.tsv
[parity-tooling]: ../tests/runtime_parity/
[runner-checks]: ../tests/runtime_parity/check-runner.sh
[lib-tests]: ../tests/
[controller]: ../src/computation/v1/graph/controller.rs
[reconcile]: ../src/computation/v1/graph/reconcile.rs
[addition]: ../src/computation/v1/graph/addition.rs
[graph]: ../src/computation/v1/graph.rs
[instance]: ../src/computation/instance.rs
[runtime]: ../src/computation/runtime/mod.rs
[runtime-query]: ../src/computation/runtime/query.rs
[runtime-inspection]: ../src/computation/runtime/inspection.rs
[runtime-events]: ../src/computation/runtime/events.rs
[lib-core]: ../src/lib_core.rs
[ordinary-ops]: ../src/lib_core_ops/
[management-runtime]: ../src/management/runtime.rs
[managed-guide]: managed-configuration.md
[topology]: ../src/computation/v1/graph/topology.rs
[lifecycle]: ../src/computation/v1/lifecycle.rs
[control]: ../src/computation/v1/control.rs
[registry]: ../src/computation/v1/graph/registry.rs
[inspection]: ../src/computation/v1/inspection.rs
[retirement]: ../src/computation/v1/graph/retirement.rs
[query]: ../src/computation/v1/query.rs
[query-recovery]: ../src/computation/v1/query_recovery.rs
[query-delivery]: ../src/computation/v1/query_delivery.rs
[query-scheduling]: ../src/computation/v1/query_scheduling.rs
[query-state]: ../src/computation/internal/query_state.rs
[query-configuration]: ../src/computation/v1/query_configuration.rs
[config-hash]: ../src/queries/config_hash.rs
[query-recovery-tests]: ../tests/computation_query_recovery.rs
[catalog]: ../src/computation/v1/query_catalog.rs
[pipeline]: ../src/computation/v1/pipeline.rs
[evaluator]: ../../core/src/query/evaluator.rs
[query-parts]: ../../core/src/evaluation/parts/mod.rs
[average]: ../../core/src/evaluation/functions/aggregation/avg.rs
[solver]: ../../core/src/path_solver/mod.rs
[graph-codec]: ../src/computation/v1/graph_codec.rs
[query-codec]: ../src/computation/v1/query_codec.rs
[output-identity]: ../src/computation/v1/output_identity.rs
[envelope]: ../src/computation/v1/envelope.rs
[value-codec]: ../src/computation/internal/typed_change/value_codec.rs
[canonical]: ../src/computation/internal/typed_change/canonical.rs
[delivery]: ../src/computation/v1/delivery.rs
[consumer-recovery]: ../src/computation/v1/consumer_recovery.rs
[middleware-recovery]: ../src/computation/v1/middleware_recovery.rs
[time-merge]: ../src/computation/v1/time_merge.rs
[merge-recovery]: ../src/computation/v1/time_merge/recovery.rs
[merge-tests]: ../tests/computation_time_merge.rs
[merge-tests-recovery]: ../tests/computation_time_merge/recovery.rs
[wal-source]: ../src/computation/v1/wal_source.rs
[qos]: ../src/computation/v1/qos_pipe.rs
[shared-qos]: ../src/computation/v1/qos_pipe/shared.rs
[retained-pipe]: ../src/computation/v1/retained_pipe.rs
[pipe-metrics]: ../src/computation/v1/pipe_metrics.rs
[plugin-source]: ../src/computation/v1/plugin_source.rs
[legacy-source]: ../src/computation/v1/legacy_source.rs
[source-base]: ../src/sources/base.rs
[reaction-base]: ../src/reactions/common/base.rs
[workers]: ../src/context/workers.rs
[graph-source]: ../src/sources/component_graph_source.rs
[component-logs]: ../src/managers/component_log.rs
[tracing-layer]: ../src/managers/tracing_layer.rs
[metrics]: ../src/metrics/
[errors]: ../src/error.rs
[change-receiver]: ../../components/host-sdk/src/proxies/change_receiver.rs
[source-proxy]: ../../components/host-sdk/src/proxies/source.rs
[reaction-proxy]: ../../components/host-sdk/src/proxies/reaction.rs
[snapshot-proxy]: ../../components/plugin-sdk/src/ffi/snapshot_fetcher_proxy.rs
[state-proxy]: ../../components/plugin-sdk/src/ffi/state_store_proxy.rs
[native-sdk]: ../../components/computation-plugin-sdk/README.md
[host-tests]: ../../components/host-sdk/tests/
[network]: ../../components/computation-plugins/network/README.md
[otel-source]: ../../components/sources/otel/
[redb-wal]: ../../components/wals/redb/src/provider.rs
[rocksdb]: ../../components/indexes/rocksdb/
[index-factory]: ../src/indexes/factory.rs
[workload-runner]: ../tests/measure-core-workloads.py
[resource-soak]: ../examples/core_resource_soak.rs
[cypher-functions]: ../../functions-cypher/src/lib.rs
[gql-functions]: ../../functions-gql/src/lib.rs
[cypher-parser]: ../../query-cypher/src/lib.rs
[gql-parser]: ../../query-gql/src/lib.rs
[observability-design]: https://github.com/drasi-project/design-documents/tree/77b155319fd8160496ad059f6fdcb1e6bc93e43d/drasi-lib/tracing-logging
[timing-control]: https://github.com/drasi-project/drasi-core/blob/b8d1f38a63d53a134b069546d349800d18b0571b/lib/docs/query-test-control.md
[server-routes]: https://github.com/drasi-project/drasi-server/blob/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/src/api/v1/routes.rs
[server-computation]: https://github.com/drasi-project/drasi-server/blob/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/src/computation.rs
[server-readme]: https://github.com/drasi-project/drasi-server/blob/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/README.md
[server-tests]: https://github.com/drasi-project/drasi-server/tree/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/tests
[native-ui]: https://github.com/drasi-project/drasi-server/blob/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/ui/src/components/inspector/ComputationDetails.tsx
[react-package]: https://github.com/drasi-project/drasi-server/blob/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/dev-tools/react/README.md
[gpu-server]: https://github.com/drasi-project/drasi-server/blob/f97bb596f0d2590f63e93659d5da4bb7fdbf3ebf/examples/gpu-cluster-lab/server/README.md
[comparison-guide]: https://github.com/drasi-project/test-infra/blob/55ff7eeba41a4222c892c75af526a64d9c16b801/e2e-test-framework/examples/building_comfort/local/native_comparison/README.md
[recovery-comparison]: https://github.com/drasi-project/test-infra/blob/55ff7eeba41a4222c892c75af526a64d9c16b801/e2e-test-framework/examples/recovery_comparison/README.md
[scheduled-recovery]: https://github.com/drasi-project/test-infra/blob/55ff7eeba41a4222c892c75af526a64d9c16b801/e2e-test-framework/examples/building_comfort/dynamic/scheduled_recovery.md
[tutorials]: https://github.com/drasi-project/learning-drasi-server/blob/72770826a214f99b65aa85d092e9b919e08254d8/README.md
[otel-tutorial]: https://github.com/drasi-project/learning-drasi-server/blob/72770826a214f99b65aa85d092e9b919e08254d8/tutorials/otel-observability/README.md
[i44]: https://github.com/drasi-project/drasi-core/issues/44
[i211]: https://github.com/drasi-project/drasi-core/issues/211
[i212]: https://github.com/drasi-project/drasi-core/issues/212
[i217]: https://github.com/drasi-project/drasi-core/issues/217
[i238]: https://github.com/drasi-project/drasi-core/issues/238
[i244]: https://github.com/drasi-project/drasi-core/issues/244
[i245]: https://github.com/drasi-project/drasi-core/issues/245
[i276]: https://github.com/drasi-project/drasi-core/issues/276
[i301]: https://github.com/drasi-project/drasi-core/issues/301
[i309]: https://github.com/drasi-project/drasi-core/issues/309
[i355]: https://github.com/drasi-project/drasi-core/issues/355
[i357]: https://github.com/drasi-project/drasi-core/issues/357
[i384]: https://github.com/drasi-project/drasi-core/issues/384
[i393]: https://github.com/drasi-project/drasi-core/issues/393
[i394]: https://github.com/drasi-project/drasi-core/issues/394
[i411]: https://github.com/drasi-project/drasi-core/issues/411
[i426]: https://github.com/drasi-project/drasi-core/issues/426
[i461]: https://github.com/drasi-project/drasi-core/issues/461
[i482]: https://github.com/drasi-project/drasi-core/issues/482
[i483]: https://github.com/drasi-project/drasi-core/issues/483
[i498]: https://github.com/drasi-project/drasi-core/issues/498
[i507]: https://github.com/drasi-project/drasi-core/issues/507
[i516]: https://github.com/drasi-project/drasi-core/issues/516
[i582]: https://github.com/drasi-project/drasi-core/issues/582
[i584]: https://github.com/drasi-project/drasi-core/issues/584
[i598]: https://github.com/drasi-project/drasi-core/issues/598
[i632]: https://github.com/drasi-project/drasi-core/issues/632
[i633]: https://github.com/drasi-project/drasi-core/issues/633
[i635]: https://github.com/drasi-project/drasi-core/issues/635
[i646]: https://github.com/drasi-project/drasi-core/issues/646
[i648]: https://github.com/drasi-project/drasi-core/issues/648
[i649]: https://github.com/drasi-project/drasi-core/issues/649
[i668]: https://github.com/drasi-project/drasi-core/issues/668
[i681]: https://github.com/drasi-project/drasi-core/issues/681
[i721]: https://github.com/drasi-project/drasi-core/issues/721
[i722]: https://github.com/drasi-project/drasi-core/issues/722
[i723]: https://github.com/drasi-project/drasi-core/issues/723
[i743]: https://github.com/drasi-project/drasi-core/issues/743
[i774]: https://github.com/drasi-project/drasi-core/issues/774
[i782]: https://github.com/drasi-project/drasi-core/issues/782
[i791]: https://github.com/drasi-project/drasi-core/issues/791
[i795]: https://github.com/drasi-project/drasi-core/issues/795
[i796]: https://github.com/drasi-project/drasi-core/issues/796
[i797]: https://github.com/drasi-project/drasi-core/issues/797
[i798]: https://github.com/drasi-project/drasi-core/issues/798
[i800]: https://github.com/drasi-project/drasi-core/issues/800
[i801]: https://github.com/drasi-project/drasi-core/issues/801
[i802]: https://github.com/drasi-project/drasi-core/issues/802
[i803]: https://github.com/drasi-project/drasi-core/issues/803
[i804]: https://github.com/drasi-project/drasi-core/issues/804
[i805]: https://github.com/drasi-project/drasi-core/issues/805
[i806]: https://github.com/drasi-project/drasi-core/issues/806
[i807]: https://github.com/drasi-project/drasi-core/issues/807
[i808]: https://github.com/drasi-project/drasi-core/issues/808
[i812]: https://github.com/drasi-project/drasi-core/issues/812
[i815]: https://github.com/drasi-project/drasi-core/issues/815
[i818]: https://github.com/drasi-project/drasi-core/issues/818
[i880]: https://github.com/drasi-project/drasi-core/issues/880
[i881]: https://github.com/drasi-project/drasi-core/issues/881
[i891]: https://github.com/drasi-project/drasi-core/issues/891
[i892]: https://github.com/drasi-project/drasi-core/issues/892
[i895]: https://github.com/drasi-project/drasi-core/issues/895
[i896]: https://github.com/drasi-project/drasi-core/issues/896
[i916]: https://github.com/drasi-project/drasi-core/issues/916
[i917]: https://github.com/drasi-project/drasi-core/issues/917
[i918]: https://github.com/drasi-project/drasi-core/issues/918
[i919]: https://github.com/drasi-project/drasi-core/issues/919
[i929]: https://github.com/drasi-project/drasi-core/issues/929
[i935]: https://github.com/drasi-project/drasi-core/issues/935
[p218]: https://github.com/drasi-project/drasi-core/pull/218
[p246]: https://github.com/drasi-project/drasi-core/pull/246
[p409]: https://github.com/drasi-project/drasi-core/pull/409
[p745]: https://github.com/drasi-project/drasi-core/pull/745
[p887]: https://github.com/drasi-project/drasi-core/pull/887
[p903]: https://github.com/drasi-project/drasi-core/pull/903
[p904]: https://github.com/drasi-project/drasi-core/pull/904
[p906]: https://github.com/drasi-project/drasi-core/pull/906
[p907]: https://github.com/drasi-project/drasi-core/pull/907
[p912]: https://github.com/drasi-project/drasi-core/pull/912
[p913]: https://github.com/drasi-project/drasi-core/pull/913
[p915]: https://github.com/drasi-project/drasi-core/pull/915
[p948]: https://github.com/drasi-project/drasi-core/pull/948
