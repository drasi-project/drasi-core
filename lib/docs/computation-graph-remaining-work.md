# ComputationGraph remaining-work inventory

**Assessment date: 10 October 2026.**
**Implementation baseline:** [`1a6b970b`](https://github.com/drasi-project/drasi-core/commit/1a6b970b57c30e06f90e5718360bd489d2d63731)
on `agentofreality-parallel-computation-graph`.

The ComputationGraph runtime is largely implemented. The remaining work is
mainly about producing correct answers in more cases, proving recovery under
more failures, improving persistent-storage performance, reconciling error
reporting and consumption, and making the capabilities easier to use through
Server and plugins. It is not a proposal for another runtime rewrite.

The graph already owns component execution and cleanup. Transactional query
state, retained delivery, recovery checks, managed configuration, complete
source transactions and bounded source-time merging also exist. An item below
about testing one of these capabilities does not mean the capability is missing.

This inventory separates:

- **Correctness work:** known or code-supported problems that can produce
  wrong results, lose recovery information or associate it with the wrong owner.
- **Qualification:** the implementation exists, but more evidence is needed
  before relying on a particular failure, load or deployment guarantee.
- **Feature/integration work:** a useful capability or supported access path
  is not implemented.
- **Optional extensions:** decisions to make only when a use case requires them.

The current [qualification ledger](../tests/runtime_parity/requirements.tsv)
contains 48 contracts:

| State | Count | Meaning |
|---|---:|---|
| Covered | 20 | The named behavior has test evidence, not blanket certification of its subsystem. |
| Partial | 23 | Some cases are covered; the ledger identifies the remaining combinations. |
| Unqualified | 5 | The complete required evidence has not been established in the ledger. |

These are not feature-completion percentages or the result of a new test run.
The last recorded full compatibility run predates the source-time merger.

The [maintainer's guide](computation-graph-maintainers-guide.md) explains how
the implementation works. The [detailed backlog](computation-graph-backlog.md)
retains the CG-01 through CG-28 identifiers, historical evidence and engineering
ratings. This document is the consolidated, plain-language inventory of what
remains; it does not replace the executable qualification ledger.

## Contents

1. [Correctness: address first](#correctness-address-first)
2. [Recovery and operational qualification](#recovery-and-operational-qualification)
3. [Finish qualifying the source-time merger](#finish-qualifying-the-source-time-merger)
4. [Performance and resource-management improvements](#performance-and-resource-management-improvements)
5. [Integration and productization](#integration-and-productization)
6. [Optional capabilities and explicit design decisions](#optional-capabilities-and-explicit-design-decisions)
7. [Recommended order and completion rules](#recommended-order-and-completion-rules)

## Correctness: address first

These issues affect solutions using ComputationGraph, but several live in
Drasi's shared query evaluator or retained source infrastructure. They were
not necessarily introduced by the runtime change. Runtime delivery can be
correct while the evaluator still produces the wrong answer.

The issue and PR references below come from the recent issue/code review.
Referenced PRs are potential implementations to adapt, not claims that their
fixes are integrated here or that their current GitHub state was rechecked
while writing this inventory.

### Prevent older changes from replacing newer state

An order has advanced to **Shipped**, then an older **Paid** update arrives.
Applying that update must not turn the order back to Paid. An older delete
must not remove a newer version either. The earlier characterization reproduced
both stale update and stale delete behavior in the shared evaluator.

**Remaining work:** implement a safe policy for ordinary query inputs using
the time the data changed at its source. Define the treatment of stale,
equal-time and replayed changes, and keep indexes and query results consistent
when rejecting or otherwise handling an event. Receipt time, processing time
and an arbitrary property in the record are not substitutes for source time.

The new merger orders inputs only where it is explicitly connected. Its
fail-and-retain policy does not add a universal stale-mutation guard to every
query. See [#244](https://github.com/drasi-project/drasi-core/issues/244).

### Handle repeated inserts and conflicting identities safely

If the same customer is inserted twice, a customer count must not become two.
Likewise, an identifier already used by a node must not silently be reused for
a relationship while leaving the old node's query results behind. Both failure
classes were reproduced.

**Remaining work:** define and implement the insert/upsert behavior for an
existing element, reject incompatible node/relationship identity collisions,
and verify the resulting counts and rows before and after restart. Recognizing
a replayed stream position is not the same as recognizing an existing business
element.

See [#507](https://github.com/drasi-project/drasi-core/issues/507) and
[#805](https://github.com/drasi-project/drasi-core/issues/805).

### Keep aggregate rows correct, including zero and null values

Three distinctions currently need work. A department with no occupants may
still need a real result row saying `occupiedSeats = 0`. A group whose last
record was deleted may need to disappear entirely. A group whose records
legitimately sum to zero must not disappear merely because its number is zero.
There is also a null-average snapshot failure relevant to persistent restart.

**Remaining work:** track whether a group has real contributors independently
of its numeric value, produce the correct Add/Update/Delete result changes,
and correct average snapshot semantics for nulls and supported value types.
Exercise nested aggregates and reconstruction from persistent state.

Relevant issues are [#384](https://github.com/drasi-project/drasi-core/issues/384),
[#498](https://github.com/drasi-project/drasi-core/issues/498) and
[#806](https://github.com/drasi-project/drasi-core/issues/806).
[PR #409](https://github.com/drasi-project/drasi-core/pull/409) and
[PR #907](https://github.com/drasi-project/drasi-core/pull/907) contain candidate
work. They require adaptation and explicit decisions about existing stored
aggregate formats; they are not safe blanket cherry-picks.

### Correct optional matching, variable scope and relationship matching

"List every team, including teams with no members" must retain empty teams.
A condition on optional members should not accidentally remove their team.
A variable defined earlier in a query must remain available where the language
allows it. Checking list membership inside a comprehension must give the same
answer as the equivalent ordinary membership check.

**Remaining work:** fix the reproduced optional-predicate, correlated-variable
and list-comprehension cases, and validate relationship endpoints in cyclic
matches. A relationship from A to B must not be reused as evidence of a
relationship from A to C. The endpoint problem is code-supported; the earlier
review did not run every cyclic-match scenario.

See [#795](https://github.com/drasi-project/drasi-core/issues/795),
[#796](https://github.com/drasi-project/drasi-core/issues/796),
[#797](https://github.com/drasi-project/drasi-core/issues/797),
[#798](https://github.com/drasi-project/drasi-core/issues/798) and
[#807](https://github.com/drasi-project/drasi-core/issues/807).
[PR #912](https://github.com/drasi-project/drasi-core/pull/912) and
[PR #915](https://github.com/drasi-project/drasi-core/pull/915) provide related
candidate fixes with dependencies and stored-state compatibility implications.

### Report missing source history instead of pretending recovery succeeded

A query remembers processing change 100, but its recovered source log only
contains changes through 20. That is missing history, not evidence that there
is nothing left to process. A source that ignores a requested resume position
also needs to be detected rather than silently trusted.

**Remaining work:** make these recovery gaps explicit, preserve the evidence
needed for diagnosis, and prevent the query from treating an unsupported resume
as successful recovery. Test lost/recreated logs and incorrect source resume
behavior separately from normal replay.

See [#721](https://github.com/drasi-project/drasi-core/issues/721) and
[#723](https://github.com/drasi-project/drasi-core/issues/723). Here a
**checkpoint** is saved processing progress, and a **source log** is the history
that makes replay from that progress possible. One does not replace the other.

### Register each subscription and its resume filter together

Two queries can subscribe to one source while asking to resume from different
positions. Their saved-position filters must not be crossed when registration
happens concurrently. Separately, the existence of a dispatcher does not prove
that a receiver is ready to accept startup data.

**Remaining work:** assign the subscriber identity and its filter atomically,
and base readiness on the real receiver boundary. Exercise concurrent
registration, early emissions, cancellation and restart.

See [#919](https://github.com/drasi-project/drasi-core/issues/919).
[PR #913](https://github.com/drasi-project/drasi-core/pull/913) addresses the
related receiver-readiness problem; it does not fix missing source history or
the crossed-filter race.

## Recovery and operational qualification

The mechanisms in this section already exist. The task is to combine more
failure conditions and measure the exact outcome, fixing any defects revealed.
A successful startup or "no panic" is not enough: assertions must check actual
rows, replayed work, saved progress and released resources.

| Area | Remaining work and what success means |
|---|---|
| Query and initial-data recovery | Combine delayed/failed initial snapshots, concurrent live changes, missing progress for one source, unavailable retained history and failed state/output writes. Restart must produce the expected rows without silently skipping data. Cover the supported ordinary/native entrypoints and providers. Ledger: T01, T04, S01, S04. |
| Timers and persistent state | Move the clock forward and backward while scheduled work exists, restart, and repeat large timer bursts alongside live traffic. Due work must neither disappear nor advance unrelated source progress. Existing timer-source and bounded-burst tests cover only parts of this combination. Ledger: T06, T07. |
| Live replacement and cleanup | Extend bounded replacement tests to prolonged unavailable dependencies and combined cancellation/replacement schedules. Old instances must stop owning resources before replacements use them; unrelated components and required deliveries must survive. Extend invalid-definition coverage across public entrypoints too. Ledger: G01, G03. |
| Retained and multicast journals | Broaden damaged-record/history combinations and prolonged disconnected-consumer scenarios. Required history must remain available, and corruption must remain an error rather than an empty journal. Check real whole-pipeline/provider combinations, not only individual pipe declarations. Ledger: Q01, Q07, Q08. |
| Consumer recovery | Combine successful remote effects, lost replies, caller cancellation and failed progress writes. Verify that retry resumes unfinished work without skipping later operations. Combine consumers that trigger only on new changes with consumers that maintain a complete current snapshot. Ledger: R01, R03. |
| Long-running behavior | Run sustained persistent workloads and measure throughput, slowest-response behavior, disk growth, process memory, native allocations, tasks and file/socket handles. Existing bounded soaks do not establish many-day stability or production latency targets. Ledger: G06, Q08, T07. |
| Deterministic test controls | Adapt the query-clock controls from the timing branch and replace remaining fixed sleep/poll budgets with actual processing and checkpoint completion. A test should advance a five-minute deadline without sleeping five minutes, and wait for the boundary it is asserting. |

The inspected timing implementation is documented at
[`b8d1f38a`](https://github.com/drasi-project/drasi-core/blob/b8d1f38a63d53a134b069546d349800d18b0571b/lib/docs/query-test-control.md).
It has not been integrated into this branch. Its query-completion boundary
does not prove that every downstream external effect has completed.
Related reaction-test work exists in
[PR #887](https://github.com/drasi-project/drasi-core/pull/887); adapting its
completion assertions must preserve the current graph lifecycle and ordering.

## Finish qualifying the source-time merger

**Implemented and delivered:** bounded source-time ordering, explicit late-event
policies, memory and durable modes, recovery tests, and the feature/maintainer
documentation were committed and pushed in
[`1a6b970b`](https://github.com/drasi-project/drasi-core/commit/1a6b970b57c30e06f90e5718360bd489d2d63731).
Committing and pushing those changes is no longer an open inventory item.

| Remaining work | Plain-English completion criterion |
|---|---|
| Protect its regression contracts | Add named merger cases to the protected contract inventory. Normal Cargo test discovery already includes the test target; the additional protection makes accidental deletion of critical coverage detectable. |
| Test actual process termination | Kill a separate process around saving input, saving output and confirming delivery. Reconstruct it and verify the exact buffered events, late-event hold and output obligations. Existing real-storage tests cover reconstruction and injected commit/cancellation failures, not all these process-exit points. |
| Measure realistic durable windows | Establish practical combinations of event size, buffer size, wait time and input rate. Durable mode saves a whole bounded snapshot on state transitions, so its cost grows with the retained buffer. Measure before deciding to introduce incremental or paged persistence. |
| Refresh the complete compatibility result | Run the full branch-wide matrix after the merger addition and record the new result. Focused merger checks do not replace the full matrix or close the broader qualification ledger. |

See the [source-time merge guide](computation-graph-time-merge.md),
[behavior tests](../tests/computation_time_merge.rs) and
[recovery tests](../tests/computation_time_merge/recovery.rs).
A standard factory/Server recipe remains integration work, listed below.

## Performance and resource-management improvements

These improvements must preserve durability and recovery. Making writes faster
by acknowledging unsaved data is not an acceptable performance fix.

| Work | Problem and remaining implementation |
|---|---|
| Durable source-log group commit | The retained redb source log commits individual appends. Saving a bounded group in one disk transaction could spread the cost across several events. Define batching/latency bounds and acknowledge each event only after its data is durably committed. Measure throughput, tail latency and crash recovery together. [#668](https://github.com/drasi-project/drasi-core/issues/668). |
| Byte-aware shared-journal admission | Shared transactional QoS currently reserves capacity by event count. A hundred small events and a hundred large events can have very different memory costs. Add byte reservations while preserving capacity-before-transaction ordering and the atomic state/output commit. This is not a total-process-memory limit. |
| Persistent storage hygiene | Investigate RocksDB log/disk growth across multiple queries, and clean obsolete relationship references from node adjacency sets. Bounded graph queues do not prove bounded disk use. Establish expected retention and compare actual disk growth and restart behavior. [#782](https://github.com/drasi-project/drasi-core/issues/782), [#309](https://github.com/drasi-project/drasi-core/issues/309). |
| Per-query archive selection | Allow a persistent query to choose whether historical element versions are archived, rather than requiring differently configured provider instances. Preserve the behavior of other queries sharing the provider. [#635](https://github.com/drasi-project/drasi-core/issues/635). |

The slowdown reported in the source-log issue is not a current benchmark for
every ComputationGraph deployment. Use the existing
[workload and performance evidence](componentgraph-vs-computationgraph.md#performance-and-qualification-boundaries)
as a baseline, then measure the specific path being changed.

## Integration and productization

These items are part of the wider inventory, **not an expansion of the current
core-only implementation phase**. Plugin additions/removals/porting, SDK
expansion, broader Server/UI work and the proposed observability work remain
separately scoped. Listing an item does not authorize its implementation.

| Area | Remaining work and user-visible outcome |
|---|---|
| Ordinary component creation | Offer factory-based source/reaction creation through the ordinary APIs. If a constructor fails, the graph should retain the requested component, show why it failed and support retry. The existing API that accepts a preconstructed object should remain available. CG-08. |
| Ordinary-to-native connections | Let Server configuration connect an ordinary query directly to a native transformer or sink. Define the data format and connection lifetime so restarting/replacing either end cannot leave stale wiring. The reverse bridge, native query results to ordinary reactions, already exists. CG-10. |
| Reconstruction recipes | Add construction recipes and dependency/version reporting for selected supported components and providers, including a standard source-time merger recipe. An exported diagram of a live object is not enough to rebuild it after restart. Exports must clearly distinguish reconstructible definitions from opaque external objects. CG-15. |
| Server operations and UI | Expose more supported live-change previews and operations, named-snapshot management, and native lifecycle/editing controls. Show the difference between a request being saved, a component starting, and that component actually becoming ready. Reuse Core's existing owners rather than add another configuration authority. CG-04, CG-09, CG-14. |
| Server qualification | Test delayed/failed startup and durable acceptance through REST. Test reconnecting observers, including gaps in Server-Sent Events (SSE), explicit resynchronization and stale observers after replacement. Bind the complete workflow evidence into the ledger. H01-H03, H05. |
| Retained plugin SDK defects | Fix the identified snapshot-vtable allocation leak, retained reaction context after clean exit, and incomplete cleanup of malformed subscription responses. Complete the missing dynamic subscription-complete callback bridge. These are distinct from intentionally keeping plugin libraries loaded for the process lifetime. |
| Native plugin and connector breadth | Selectively add missing dynamic query services, supported plugin reconfiguration and required HTTP/gRPC features. Publish which bootstrap, batching, templates, snapshots and transport-security combinations are supported. Durable admission, coordinated bootstrap and consumer completion already exist; do not implement them again. CG-11, CG-12. |
| Plugin lifetime qualification | Exercise more malformed buffers/metadata, incompatible versions, cancellation, revoked storage calls, reused handles, restarts and late callbacks in separately built libraries. Add sustained ownership/accounting evidence; compiling host and plugin together is insufficient. A01-A05. |
| Error reporting and consumption | Reconcile returned errors, graph failures, lifecycle events, logs, metrics and management/recovery signals so applications and operators can identify the same problem, understand its impact and respond safely. Include ordinary and native components, Rust consumers, Server APIs and UI. See [scope and completion criteria below](#reconcile-error-reporting-and-consumption). |
| Shared observability and host controls | Implement the separately proposed correlated tracing, metrics and logging integration with collection/export owned by the embedding application. Existing status, inspection, logs and metrics remain useful and must be preserved. A unified recovery-event API and configurable introspection remain separate requests, not capabilities supplied merely by adding an exporter. |
| Examples, templates and test infrastructure | Support native topology/resources in solution templates, complete the stock-Server GPU lab, and broaden deterministic tutorial acceptance tests. Version the test-infra selection of QoS/query-host modes and assert where observed data came from. A process starting successfully is not proof that an example produced its expected results. CG-16, CG-17, CG-28; H04. |

The retained SDK findings are tracked in
[#646](https://github.com/drasi-project/drasi-core/issues/646),
[#648](https://github.com/drasi-project/drasi-core/issues/648),
[#649](https://github.com/drasi-project/drasi-core/issues/649) and
[#774](https://github.com/drasi-project/drasi-core/issues/774).
Related callback work exists in
[PR #903](https://github.com/drasi-project/drasi-core/pull/903).
Broader plugin adoption remains necessary even where the shared framework
services are implemented.

The telemetry direction is related to the
[observability design review](https://github.com/drasi-project/design-documents/pull/7).
Recovery notifications and opt-in introspection have separate acceptance
criteria; they should not be hidden inside one large instrumentation task.

### Reconcile error reporting and consumption

A native transformer can fail while Server shows only its failure phase and
the ordinary lifecycle-event feed has no matching event. An application may
also receive an error log without a component state change. These mechanisms
serve different purposes, but their coverage and relationship must be clear
enough that neither an operator nor an automated supervisor has to guess.

**Remaining work:** define and implement a consistent reporting and consumption
contract across Core, plugins, Server and UI. Preserve the graph as the authority
for runtime state; do not introduce a second mutable status registry or turn
every diagnostic error log into a component failure.

| Work | Plain-English completion criterion |
|---|---|
| Agree what each signal means | Distinguish a rejected operation, a failed or degraded component, recovery activity and a diagnostic message. Define how the corresponding errors, state, events, logs and metrics relate. Correlate them by instance, graph, component generation and operation where applicable. Preserve typed causes and multiple failures in Rust; provide stable classifications and safe detail across external interfaces. |
| Close reporting coverage gaps | Cover ordinary and native components, nested query graphs, graph-driver failures, configuration reconciliation and failed cleanup. Define when a failure is first reported, remains active, recurs or clears. A consumer must be able to tell whether processing stopped, work is retained, cleanup is unfinished or recovery completed. |
| Make consumption and loss explicit | Provide a documented snapshot/subscription and reconnect path. State which feeds coalesce and which preserve transitions. Surface broadcast gaps and dropped logs instead of silently continuing, and specify how consumers obtain fresh state. Define bounded retention and slow-consumer behavior. Decide explicitly whether durable incident history belongs in Drasi or an external consumer; current in-memory history is not durable replay. |
| Align Server and UI with Core | Expose useful, redacted native failure details rather than only the failure phase. Make native failure updates available without relying solely on ordinary component events. Distinguish lifecycle activity from diagnostic logs, and process liveness from application readiness/health. Present current failure, impact and supported actions consistently without exposing secrets or stale replacement state. |
| Make safe action straightforward | Document and demonstrate an embedding application's supervisor and an external Server consumer: detect a failure, obtain context, resynchronize after a gap and route an alert. Make tracing setup work with an application-owned subscriber and make capture loss observable. Explain when retry, cleanup, reconstruction or reconciliation is appropriate; a retryable label alone must not trigger unsafe restart. Keep alert destinations and automatic recovery policy explicit, not implicit side effects of logging. |

**Qualification:** inject creation, processing, recovery and cleanup failures,
then verify what each supported Rust/API/UI consumer actually receives.
Exercise repeated failures, recovery, slow subscribers, reconnects, process
restart, multiple instances and component replacement. Assert correct identity,
failure detail, gap detection and recovery of current state, not merely that a
message was logged. Prove that slow or disconnected observers cannot create
unbounded retained work or silently lose required failure evidence.

This is separate from choosing a telemetry exporter. The reporting and
consumption contract must guide the shared observability integration, and any
durability or automatic-action guarantees need their own acceptance criteria.
Adding this inventory item does not start the broader implementation phase.

## Optional capabilities and explicit design decisions

These are not prerequisites for every use of the current design. Select them
only when the required behavior and its cost are clear.

| Capability | What would be different, and why it is a separate decision |
|---|---|
| More query-language features | Correlated `EXISTS`, `UNWIND`, `UNION`, additional GQL stages/functions, string aggregates and deterministic collection ordering broaden expressiveness. Connecting runtime nodes does not implement the language semantics or their incremental recovery behavior. |
| Stronger event-time processing | Explicit source watermarks would let sources state that earlier events are complete. Correcting/retracting results after late input is another capability. The merger currently offers bounded waiting and explicit late policies, not either broader guarantee. |
| Atomic visibility across several consumers | A complete source transaction is already atomic at its declared query-processing boundary. Making several downstream queries or external effects become visible together needs a larger participant/commit protocol. It is not a missing flag on an ordinary pipe. |
| Independent native-node CPU execution | Ordinary queries already have independently driven nested graphs. Native nodes in one graph share cooperative scheduling. Introduce additional execution isolation only if representative measurements demonstrate a scaling need; preserve graph ownership and cancellation rules. |
| Rate/burst limits and acknowledgement windows | Queue capacity bounds retained work; it does not enforce events per second. A rate limiter needs an explicit wait/reject/drop policy. Allowing more unacknowledged deliveries needs evidence that acknowledgement latency is a bottleneck and a safe progress contract. Neither extension is currently selected. |
| Larger architectural extensions | Feedback cycles, multi-machine execution, unloading plugin libraries during execution and automatic migration of stored state each require an explicit design. They are not small cleanup tasks needed to complete the existing runtime. |

Automatic all-or-nothing deployment rollback and exactly-once arbitrary remote
effects are also not current guarantees. A destination may perform an effect
before its reply is lost; avoiding repetition requires destination cooperation,
such as stable idempotency keys or a shared transaction.

## Recommended order and completion rules

1. **Fix shared correctness problems.** Prioritize stale/duplicate input,
   aggregate rows, matching and source recovery/subscription integrity.
2. **Finish merger qualification and refresh branch-wide evidence.** Its
   implementation and commit/push are already complete; protected contracts,
   process-exit coverage and realistic durable measurements remain.
3. **Extend recovery and sustained-load qualification.** Test exact saved
   progress, results and resource ownership across combined failures.
4. **Address measured storage bottlenecks.** Keep acknowledgements after
   persistence and confirm that improvements survive restart.
5. **Select integration and optional work explicitly.** Server, plugins,
   observability and new architectural guarantees should have their own scope
   and acceptance criteria. Reconcile error-reporting and consumption contracts
   before building their exporter and UI integrations.

Keep evidence and implementation status separate when updating this document.
A test-only change may close a qualification contract without adding a feature.
A working feature may still need long-running qualification. A branch fix
does not establish that a published release contains it.

When reusing evaluator fixes, decide how existing persisted query state is
handled before rollout. A changed aggregate representation or matching rule
may require a version check and deliberate rebuild/replay; do not silently
reinterpret old state. Update the
[qualification ledger](../tests/runtime_parity/requirements.tsv) and
[protected contracts](../tests/runtime_parity/computation-contracts.tsv) with
the named evidence, preserving the established CG identifiers where applicable.
