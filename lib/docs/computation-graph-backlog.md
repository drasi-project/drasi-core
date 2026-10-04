# ComputationGraph: issues, gaps, and next-phase work

**Assessment date: 2026-10-03.** This document turns the earlier repository
assessment into a planning guide. It preserves all 28 items from that assessment,
but explains the practical problem, what completion would mean, and the likely
difficulty and disruption of the work.

This is **not an approved roadmap, a set of delivery estimates, or a claim that
every listed area is broken**. It separates known implementation problems,
missing capabilities, and areas where more evidence is needed. The priority and
risk ratings are engineering judgments, except where an existing qualification
requirement already assigns a priority.

The assumed goal is a dependable ComputationGraph-based Server that people can
configure, restart, and operate confidently. A demonstration, an embedded-only
application, or a deliberately nonpersistent pipeline can reasonably choose a
smaller scope.

## How to read the ratings

| Rating | Meaning |
|---|---|
| **P1: address before relying on the affected guarantee** | Important correctness, recovery, or operational work. It does not necessarily block uses that do not need that capability. |
| **P2: important next-phase improvement** | Broadens useful support, makes operation easier, or resolves an important design question. |
| **P3: optional or use-case-driven** | Valuable packaging, demonstration, or convenience work after the underlying capabilities are ready. |
| **Low complexity** | A localized change with a clear approach and limited integration work. |
| **Medium complexity** | Several interacting changes or substantial tests, but an established design to build on. |
| **High complexity** | Cross-component or cross-repository work, changes to stored data or plugin contracts, or a substantial design decision. Split these items before estimating delivery time. |
| **Low disruption risk** | Mostly documentation, tests, measurement, presentation, or isolated additions. Existing execution behavior should not need to change. |
| **Moderate disruption risk** | Changes an established path, but can probably be introduced incrementally with focused compatibility checks. |
| **High disruption risk** | Touches restart behavior, live component replacement, scheduling, stored state, or host/plugin compatibility. A careful rollout and failure-path coverage are necessary. |

**Complexity and disruption are different.** A large new test suite can be complex
but low-risk for existing users. A small-looking change to the data exchanged with
a plugin can be much more disruptive. These ratings describe the proposed work,
not the severity of a currently demonstrated incident. For testing-only items,
the risk rating does not include fixes for bugs the tests might discover.

### A few terms used below

- **Ordinary components** use the existing Source, Query, and Reaction APIs.
  **Native components** use the general ComputationGraph interfaces. Both run in
  the same ComputationGraph-based system; they are not two competing engines.
- **Durable** means saved in storage with the required failure-survival
  guarantee, rather than merely placed in an in-memory queue.
- A **checkpoint** is saved progress: the position from which work should resume
  after a restart. **Bootstrap** is the initial loading of existing data before
  or alongside processing live changes.
- A **plugin boundary**, also called an FFI or ABI boundary, is the contract
  between the host and a separately compiled plugin library. Changing one side
  without a compatible change or version check on the other can break plugins.
- **Qualification** means collecting evidence for a stated behavior, including
  failures and restarts. A passing normal-operation test is not enough to prove
  all of those cases.

## Overall state

The [tracked qualification ledger][ledger] contains 48 scenarios:

| State | Count | What that means |
|---|---:|---|
| Covered | 11 | The named contract has test evidence; this is not a claim about the entire subsystem. |
| Partial | 27 | Some relevant behavior is tested, but the remaining cases are explicitly listed. |
| Blocked | 4 | A missing capability prevents the requested contract from being fully established. |
| Unqualified | 6 | The required qualification evidence is not yet established. |

These are **not feature-completion percentages or the results of a new test run**.
The four blocked scenarios are CG-01 through CG-04 below; all four are already P1
in the ledger. The 28 planning items group related work and therefore do not
correspond one-to-one with the 48 scenarios.

The working system already has a single root graph per DrasiLib instance,
ordinary query execution, native components, graph-owned lifecycle management,
transactional query state, recovery mechanisms, and significant failure testing.
This backlog is about the remaining boundaries and confidence gaps, not a proposal
to rebuild those capabilities.

## Foundation regression evidence

The foundation suite now exercises construction, connections, delivery rules and
failure recovery together, rather than relying only on isolated happy-path tests.
The additions are tests and measurement tooling; they do not change production
runtime behavior or introduce a new runtime owner.

| Area | What the tests establish |
|---|---|
| Schema and ports | Every one of the 512 capability declarations is checked against every one of the 512 requirement sets, at both endpoints and with three capacity declarations. Every port-direction pair, schema identity field, change-operation/image combination, and sink-completion requirement set is checked. Invalid declarations fail rather than being silently repaired. |
| Component construction and connection | 1,024 combinations cover eight upstream pipe profiles, eight downstream profiles, and all eight direct/factory construction choices for source, transformer and sink, on both single-threaded and multithreaded Tokio. Tests assert exact values, ordering, parent identities, deferred factory construction, one start/stop per component, and actual component destruction. |
| Pipe behavior | All eight volatile profiles are checked for receiver ownership, cancellation, graceful draining, final-sender closure and invalid resource bindings. Separate pressure and overflow tests check exactly when capacity is released, which events are retained or discarded, and the reported gaps and metrics. Private unit tests also force exhausted counters and failed internal ownership locks. |
| Transaction failure | Eighteen schedules inject failure or cancellation at every participant position in one-, three- and five-stage transactions. Fresh RocksDB-backed reconstruction verifies rollback, retry, input progress and each participant's committed state. Later participants must not run after a failure. |
| Connected crash recovery | A real source, two-stage transactional transformer, continuous query and sink run with capacity-one durable pipes. Four retained/QoS downstream combinations are crashed in a separate process after an external effect but before acknowledgment. A new process reconstructs the graph and checks exact saved rows, participant counters, replay identities and committed-input deduplication. |
| Retained storage errors | Ten injected read, transaction-start and staged-write failures preserve previously committed records and progress. An I/O failure cannot look like empty history. Interrupted writes prevent further use until cleanup and recovery. Invalid providers, payloads, acknowledgment positions and exhausted sequence counters are rejected. |
| Persisted delivery rules | Fourteen malformed metadata/journal cases must fail before delivering or accepting new work. Rejected reconfiguration must preserve both subscribers' pending work. Eight deterministic retained/QoS commit-revocation schedules verify explicit uncertainty and exact accepted/handled positions after reopening real storage. Durable pruning preserves strict gap errors until the subscriber explicitly permits skipping; retired subscriber identities cannot be silently reused. |

The eight volatile profiles are bounded, broadcast with lag reporting, ranked
blocking, ranked drop-newest, retained-memory backpressure, retained-memory
prune-oldest, QoS-memory backpressure and QoS-memory prune-oldest. Strict gaps are
used for the pruning profiles. The connection matrix uses enough capacity to
avoid accidental loss in its exact-output checks; the separate overflow tests
deliberately fill the lossy queues. Real query behavior is covered by the
connected durable pipeline, not by pretending the synthetic transformer is a
query.

**Acceptance, delivery and handling are different guarantees.** Bounded/ranked
queues free capacity on receipt; retained/QoS backpressure waits for handling
acknowledgment. A failed acknowledgment leaves work recoverable. A process can
perform an external effect and crash before confirming it, so that effect may be
repeated on recovery. These tests require stable logical identities for retries,
not arbitrary exactly-once external effects.

From the repository root, run:

```bash
bash lib/tests/run-computation-foundations.sh --coverage
```

This runs the complete default library unit/integration targets, selected
no-default-feature targets, RocksDB-backed recovery targets, and eight
cross-crate integration targets. It writes per-profile logs, a status file,
LLVM HTML/JSON/LCOV reports, and `foundation-coverage.json` under
`target/foundation-qualification/`. The reports are local build artifacts and
are regenerated rather than committed. Do not edit Rust sources during a
measurement; rerun after changing them.

Production coverage is measured separately for nine schema, port and pipe
modules. Inline test-only sections are excluded, but uncovered production lines
are not hidden. The report lists every uncovered line and records whether full
line coverage was achieved. Branch coverage is not measured. To make a
less-than-100% result fail explicitly:

```bash
python3 lib/tests/runtime_parity/foundation_coverage.py \
  target/foundation-qualification/coverage.json \
  --output target/foundation-qualification/foundation-coverage.json \
  --require-full
```

### Measured result: 2026-10-04

All four selected profiles passed: **1,562 Rust test executions**, including
repeated cases across build profiles, not 1,562 distinct newly added tests.

| Profile | Passed | Ignored entries |
|---|---:|---:|
| Complete default library unit/integration targets | 1,394 | 1 diagnostic benchmark |
| Selected no-default-feature targets | 60 | 0 |
| Selected persistent library targets | 68 | 0 |
| Selected cross-crate integration targets | 40 | 5 subprocess-worker entrypoints |

The five worker entrypoints are invoked by their parent crash tests; their
ignored status prevents independent normal execution, not execution of the crash
scenarios. All 35 added or updated protected-contract entries have passing
results in these logs. The nine Python measurement/ledger tests and the affected
Rust formatting/lint checks also pass.

The clean production-only measurement covers **2,000 of 2,009 instrumented lines
(99.55%)**:

| Module under `lib/src/computation/v1/` | Covered lines | Line coverage |
|---|---:|---:|
| `data.rs` (schema and records) | 316 / 316 | 100% |
| `ports.rs` | 172 / 172 | 100% |
| `pipe.rs` (shared pipe contract) | 38 / 38 | 100% |
| `bounded_pipe.rs` | 121 / 121 | 100% |
| `broadcast_pipe.rs` | 102 / 103 | 99.03% |
| `ranked_pipe.rs` | 202 / 203 | 99.51% |
| `retained_pipe.rs` | 176 / 177 | 99.44% |
| `retained_store.rs` | 221 / 224 | 98.66% |
| `qos_pipe.rs` | 652 / 655 | 99.54% |

**The requested full-coverage target is not met.** The strict `--require-full`
check fails, as it should. The nine uncovered instrumented lines are:

- Error conversion when constructing capability sets: `broadcast_pipe.rs:55`,
  `ranked_pipe.rs:223`, `retained_pipe.rs:103`, and `qos_pipe.rs:80`.
- The retained-capacity accounting guard: `retained_store.rs:405-407`.
- QoS transaction-error mapping and cleanup-ownership failure:
  `qos_pipe.rs:556` and `qos_pipe.rs:770`.

Those paths are not excluded just to improve the percentage. Their current
coverage remains missing; passing nearby paths is not proof of their behavior.
These line numbers describe this measurement and must be regenerated after
source changes. The percentage does not describe the rest of ComputationGraph,
provider-defined schemas, every branch, or all possible concurrent schedules.

**This is not a production-readiness certificate.** The four blocked ledger
entries remain blocked: whole-pipeline recovery negotiation, native durable
ingress, native consumer recovery services, and Server-managed configuration.
This run also does not establish sustained-load/resource bounds, every live
replacement schedule, dynamically loaded plugin compatibility, or the full
Server/PostgreSQL/Garnet qualification. New protected test names have been added
to the existing runtime-parity inventory so removing a test cannot silently
erase its contract from that gate.

## Quick planning index

The foundation regression suite added after this assessment is described in
[Foundation regression evidence](#foundation-regression-evidence). Its passing
tests do not automatically close the remaining qualification items.

The category headings and detailed entries below use the same stable CG numbers.
The disruption column means risk **while making the change**.

| Item | Plain-English description | Priority | Complexity | Disruption |
|---|---|---|---|---|
| [CG-01](#cg-01-check-that-the-whole-pipeline-can-recover) | Check that the whole pipeline can recover | P1 | High | Moderate |
| [CG-02](#cg-02-make-native-network-input-survive-a-crash) | Make native network input survive a crash | P1 | High | High |
| [CG-03](#cg-03-establish-whether-a-native-consumers-saved-progress-is-durable) | Establish whether a native consumer's saved progress is durable | P1 | High | High |
| [CG-04](#cg-04-expose-durable-configuration-management-through-server) | Expose durable configuration management through Server | P1 | High | High |
| [CG-05](#cg-05-stop-losing-plugin-status-updates-when-the-host-is-busy) | Stop losing plugin status updates when the host is busy | P1 | Medium | Moderate |
| [CG-06](#cg-06-do-not-report-a-storage-listing-failure-as-an-empty-store) | Do not report a storage listing failure as an empty store | P1 | High | High |
| [CG-07](#cg-07-deliver-bootstrap-settings-and-context-to-plugins) | Deliver bootstrap settings and context to plugins | P1 | Medium | High |
| [CG-08](#cg-08-let-ordinary-components-be-created-inside-the-managed-lifecycle) | Let ordinary components be created inside the managed lifecycle | P2 | Medium | Moderate |
| [CG-09](#cg-09-expose-more-safe-live-graph-changes-through-server) | Expose more safe live graph changes through Server | P2 | High | High |
| [CG-10](#cg-10-connect-an-ordinary-query-directly-to-a-native-component) | Connect an ordinary query directly to a native component | P2 | High | High |
| [CG-11](#cg-11-complete-the-missing-native-plugin-services) | Complete the missing native plugin services | P2* | High | High |
| [CG-12](#cg-12-close-native-http-and-grpc-feature-gaps) | Close native HTTP and gRPC feature gaps | P2 | High | Moderate |
| [CG-13](#cg-13-decide-when-native-queries-need-independent-parallel-execution) | Decide when native queries need independent parallel execution | P2 | High* | High* |
| [CG-14](#cg-14-manage-native-components-from-the-ui) | Manage native components from the UI | P2 | Medium | Low |
| [CG-15](#cg-15-make-more-exported-configurations-usable-for-rebuilding-an-instance) | Make more exported configurations usable for rebuilding an instance | P2 | High | Moderate |
| [CG-16](#cg-16-include-native-components-in-solution-templates) | Include native components in solution templates | P3 | Medium | Moderate |
| [CG-17](#cg-17-finish-the-gpu-cluster-lab-for-the-standard-server) | Finish the GPU Cluster Lab for the standard Server | P3 | High | Moderate |
| [CG-18](#cg-18-preserve-a-complete-database-transaction-in-derived-context) | Preserve a complete database transaction in derived context | P2* | High | High |
| [CG-19](#cg-19-add-long-term-housekeeping-for-the-configuration-store) | Add long-term housekeeping for the configuration store | P2 | High | High |
| [CG-20](#cg-20-test-more-configuration-restoration-and-resource-replacement-failures) | Test more configuration restoration and resource replacement failures | P1 | High | Low |
| [CG-21](#cg-21-test-graph-lifecycle-cleanup-and-resource-use-more-thoroughly) | Test graph lifecycle, cleanup, and resource use more thoroughly | P1 | Medium | Low |
| [CG-22](#cg-22-test-delivery-rules-under-corruption-disconnection-and-heavy-load) | Test delivery rules under corruption, disconnection, and heavy load | P1 | High | Low |
| [CG-23](#cg-23-test-query-transactions-and-timers-in-more-failure-combinations) | Test query transactions and timers in more failure combinations | P1 | High | Low |
| [CG-24](#cg-24-test-source-and-reaction-recovery-across-more-real-world-cases) | Test source and reaction recovery across more real-world cases | P1 | High | Low |
| [CG-25](#cg-25-test-plugin-compatibility-and-cleanup-under-more-adverse-conditions) | Test plugin compatibility and cleanup under more adverse conditions | P1 | High | Low |
| [CG-26](#cg-26-qualify-the-server-apis-and-test-infrastructure) | Qualify the Server APIs and test infrastructure | P1 | High | Low |
| [CG-27](#cg-27-establish-a-current-performance-baseline) | Establish a current performance baseline | P2 | Medium | Low |
| [CG-28](#cg-28-automatically-check-the-tutorials-expected-behavior) | Automatically check the tutorials' expected behavior | P2 | Medium | Low |

The asterisks are important: the recovery-related parts of CG-11 are P1 dependencies;
CG-13's initial measurement is much smaller and safer than a scheduling redesign;
CG-18 becomes P1 for an application that requires whole-transaction observations.

## A. Missing capabilities that block specific guarantees

### CG-01: Check that the whole pipeline can recover

**Priority: P1. Complexity: High. Disruption risk: Moderate.**

**What is missing:** Individual components and connections can describe some of
their reliability properties, but there is no complete check that the entire
source-to-query-to-consumer path meets a requested recovery guarantee. For example,
a query might save its results correctly while its source cannot replay lost
input or its consumer saves progress only in memory.

**Why it matters and what completion means:** A user needs to know whether a
pipeline advertised as recoverable actually has all the necessary pieces. Add a
way to request that guarantee, check every relevant participant, and explain
exactly which component prevents it. Deliberately lossy or memory-only pipelines
must remain valid when the user chooses them.

**Why these ratings:** This is P1 before promising whole-pipeline recovery.
Complexity comes from combining several contracts, not adding one validation
flag. An opt-in check limits disruption, but new validation must not reject
existing intentionally nonpersistent configurations.

**Where/evidence:** Core; ledger **Q07** [records the missing combined check][ledger].
Depends on accurate source and consumer capabilities, including CG-02 and CG-03.

### CG-02: Make native network input survive a crash

**Priority: P1 for crash-surviving input. Complexity: High. Disruption risk: High.**

**What is missing:** A successful native HTTP or gRPC submission currently means
the input reached a bounded in-memory queue. It does not mean the input has been
saved durably or fully processed. If the process dies after acknowledging receipt
but before completing the work, the caller may believe the input was safely
received when it was not.

**What completion means:** Offer an explicitly durable input path: save accepted
input, identify its saved position, and replay it after restart without silently
skipping work. The plugin contract also needs the services for communicating
source recovery progress. Keep the current memory-only mode clearly distinguished.

**Why these ratings:** Important before substituting these connectors for a
replay-capable source. The work changes storage, acknowledgments, retry behavior,
and plugin interfaces. Getting the order wrong can lose input or introduce
unexpected repeated processing.

**Where/evidence:** Core native network components and native plugin SDK/host;
ledger **S03** and the [documented input guarantee][network]. The necessary plugin
work overlaps CG-11; it is not a separate second implementation.

### CG-03: Establish whether a native consumer's saved progress is durable

**Priority: P1 for recoverable consumers. Complexity: High. Disruption risk: High.**

**What is missing:** A consumer can record how far it has processed a stream, but
`ConsumerProgressStore` does not declare whether that record survives the required
failure. Native plugins also lack some of the services needed to use recovery
resources.

**Why it matters:** The system cannot confidently validate a recovery promise if
it cannot distinguish a saved position on persistent storage from one held only
in memory. After a restart, lost progress can cause work to be repeated; incorrect
progress can cause unfinished work to be skipped.

**What completion means:** Make the storage guarantee explicit, pass the required
recovery resources to native consumers, and exercise restart cases. This still
does not promise that every arbitrary external action happens exactly once.

**Why these ratings:** The issue directly affects recovery correctness. It spans
storage contracts and plugin interfaces, and changes when work can safely be
considered complete, making compatibility and failure testing essential.

**Where/evidence:** Core [consumer recovery contract][consumer-recovery], native
SDK/host, and ledger **R04**. Related to CG-01 and CG-11.

### CG-04: Expose durable configuration management through Server

**Priority: P1 for managed Server deployments. Complexity: High. Disruption risk: High.**

**What is missing:** Core can save a requested configuration and a request receipt
before attempting to create or change components. Server does not expose the
complete workflow for those saved requests, retries, reconciliation, and named
configuration snapshots. Saving Server YAML is not the same guarantee.

**Why it matters:** Suppose a user requests a change and the Server crashes before
responding. A saved request identifier should let the user find out whether the
change was accepted, rather than resubmit blindly. The requested configuration
should also survive components failing to start.

**What completion means:** Connect Server's configuration and API flows to Core's
existing management support. Clearly distinguish "request saved" from "components
ready," provide retry/status operations, and restore the accepted configuration
after restart.

**Why these ratings:** This is central to dependable managed deployments. It
crosses Server and Core and changes startup and configuration ownership. The main
danger is creating competing configuration authorities or a second runtime owner.
Start with an explicitly supported component subset rather than promise universal
restoration.

**Where/evidence:** Core [managed configuration][management], [Server routes][server-routes],
ledger **H02**. CG-08 and CG-15 broaden the component/provider coverage this can use.

## B. Known implementation problems

### CG-05: Stop losing plugin status updates when the host is busy

**Priority: P1. Complexity: Medium. Disruption risk: Moderate.**

**What happens now:** The Host SDK attempts to place lifecycle updates into an
observer queue without waiting. When the queue is full, it logs the problem and
discards the update. An observer can therefore miss a start, stop, or failure
transition and retain an outdated view of a plugin.

**What completion means:** Preserve an authoritative latest state or provide an
explicit way for observers to catch up. Do not solve the problem by indefinitely
blocking a plugin callback while waiting for a slow observer. Tests should fill
the queue deliberately and verify that the observer can still discover the
correct state.

**Why these ratings:** Operators must be able to trust component status, so this
deserves early attention. The change is reasonably localized, but concurrency
and callback behavior make it more than a queue-size adjustment. It can affect
status timing; this finding alone does not establish lost query data.

**Where/evidence:** Core [Host SDK lifecycle callback][callbacks], ledger **A02**.

### CG-06: Do not report a storage listing failure as an empty store

**Priority: P1. Complexity: High. Disruption risk: High.**

**What happens now:** When a state-store provider fails or panics while listing
keys, the host bridge returns an empty array. The receiving plugin treats that
array as a successful result. "The store has no keys" and "we could not read the
store" become indistinguishable.

**Why it matters and what completion means:** A caller must be able to stop,
report the failure, or retry instead of making decisions on a false empty result.
Add an explicit error result to the host/plugin contract and carry it correctly
through both sides. The associated ABI work also needs to finish forwarding
storage durability information and defining who retains and releases provider
handles.

**Why these ratings:** Silently misreporting a storage failure is a correctness
problem. The visible fix is small, but the current cross-library contract cannot
express the result properly. Versioning, old/new plugin combinations, and
provider lifetimes make this high-complexity, high-disruption work.

**Where/evidence:** Core [host state-store bridge][state-bridge],
[plugin state-store proxy][state-proxy], ledger **A03**. Other storage-operation
error paths already have coverage; this is not a claim that all storage failures
are swallowed.

### CG-07: Deliver bootstrap settings and context to plugins

**Priority: P1 where bootstrap uses those values. Complexity: Medium. Disruption risk: High.**

**What happens now:** The bootstrap plugin proxies accept subscription settings
but do not forward them. They also send server/source identifiers without the
complete bootstrap context properties. A plugin does not receive all the
information its caller supplied.

**Why it matters and what completion means:** A bootstrap provider that relies on
those values cannot reliably perform the requested initial load. Forward the
settings and context across both directions of the plugin boundary, document
which values are supported, and prove that non-default values actually reach the
provider. Providers that do not use these values may be unaffected today.

**Why these ratings:** The desired behavior is clear and the implementation is
bounded. However, changing the cross-library calling contract is potentially
disruptive: host and plugin changes must be compatible or rejected explicitly.
An incompatible plugin must not silently receive incomplete inputs.

**Where/evidence:** Core [host bootstrap proxy][bootstrap-host],
[plugin bootstrap proxy][bootstrap-plugin], and [library architecture guidance][lib-guidance].

## C. Incomplete features and integration

### CG-08: Let ordinary components be created inside the managed lifecycle

**Priority: P2. Complexity: Medium. Disruption risk: Moderate.**

**What is missing:** Ordinary source and reaction add APIs still take objects the
caller has already constructed. If construction fails before the object reaches
the graph, the graph cannot retain it as a visible failed declaration. Native and
managed paths already have factory-based construction: a recipe the runtime uses
to create an object.

**What completion means:** Offer the factory/recipe path through the ordinary APIs
without removing the existing object-based path. A failed creation should be
visible, diagnosable, retryable, and subject to the same cleanup rules as other
graph-managed work.

**Why these ratings:** This improves consistent management and reconstruction,
rather than repairing every existing addition. Reusing current factories keeps
complexity moderate. The main risk is changing when resources are acquired and
which owner must release them after cancellation or failure.

**Where/evidence:** Core ordinary APIs and [current design limits][design-limits].
Related to CG-04 and CG-15.

### CG-09: Expose more safe live graph changes through Server

**Priority: P2. Complexity: High. Disruption risk: High.**

**What is missing:** Core can preview and apply more kinds of graph changes than
Server exposes. Examples include replacing an implementation, changing supported
settings in place, reconnecting inputs, changing resources, and pausing after
in-progress work reaches a safe stopping point. Server currently exposes a narrower set, including
inspection, addition, start, stop, and removal.

**Why it matters and what completion means:** Operators should not have to drop
into a custom embedded program for supported live changes. Add deliberate Server
APIs with previews, clear failure reporting, and checks that the graph has not
changed since the user prepared the request.

**Why these ratings:** Core provides a foundation, but a safe operator-facing
workflow spans validation, configuration, lifecycle, and error handling.
Replacing live resources or connections can interrupt processing, so this is
not just exposing a few methods. Keep managed and unmanaged change paths
consistent; do not imply all-or-nothing deployment rollback.

**Where/evidence:** [Server routes][server-routes] and Core
[reconciliation implementation][reconcile]. Coordinate with CG-04.

### CG-10: Connect an ordinary query directly to a native component

**Priority: P2. Complexity: High. Disruption risk: High.**

**What is missing:** A native query can publish results through an outlet that an
ordinary reaction consumes. Server does not have an equivalent first-class
declaration for taking an ordinary Server-configured query's output and wiring it
directly to a native input. Native definitions are currently validated separately
and reject the external bindings this would require.

**Why it matters and what completion means:** A user should be able to build a
pipeline such as "ordinary query, then custom native transformer, then sink"
without writing a separate host program. Define the connection, its data format,
its lifetime, and what happens when either end is replaced or restarted.

**Why these ratings:** This is useful composition work, but it crosses ordinary
query ownership and native connection management. Incorrect handling can leave
stale connections or create another execution owner. Reuse the existing root
graph and the legitimate query-owned nested graph; do not solve the configuration
gap by introducing another application graph.

**Where/evidence:** [Server native configuration validation][server-computation]
and the [runtime ownership diagrams](runtime-architecture.md).

### CG-11: Complete the missing native plugin services

**Priority: P2 overall; P1 for the recovery pieces in CG-02/CG-03.
Complexity: High. Disruption risk: High.**

**What is missing:** A separately loaded native plugin does not yet have all the
capabilities available to an in-process component. A query cannot yet be hosted
as a dynamically loaded native plugin. Plugins also lack access to current query
results and saved undelivered output, a way to expose a source's saved restart
position, general access to graph-provided storage or other shared services, and
the ability to change settings without replacing the component.

**Why it matters and what completion means:** Some components can be written
inside the host program but cannot yet be packaged as equivalent dynamic plugins.
Add explicit, versioned services for the required capabilities, with clear rules
about who owns returned data and when a plugin is no longer allowed to use it.

**Why these ratings:** This is an umbrella item, not one small feature. Split it
by capability, doing the recovery prerequisites first. Host and plugin must agree
on the new contract, and callbacks can outlive the operation that created them.
Mistakes can break compatibility or object lifetimes across library boundaries.

**Where/evidence:** Core [native plugin SDK limitations][native-sdk].
Do not count CG-02/CG-03's shared ABI work twice when estimating.

### CG-12: Close native HTTP and gRPC feature gaps

**Priority: P2, depending on connector requirements.
Complexity: High as a group. Disruption risk: Moderate.**

**What is missing:** The initial native network components are not drop-in
replacements for every ordinary connector configuration. Remaining differences
include initial-data loading, adaptive batching, sending several gRPC sink items
in one batch, output templates, and full snapshot/recovery behavior. Native gRPC
TLS support is not advertised.

**Why it matters and what completion means:** Switching a working solution to
native connectors may remove a feature it relies on. Publish the supported
comparison, select the needed features, and implement them with behavior checks
against the ordinary connector. Reject unsupported configuration clearly rather
than ignore it. An application requiring encrypted gRPC must not assume the
current native connector satisfies that requirement.

**Why these ratings:** Each feature is manageable, but the combined scope is
large and needs independent acceptance criteria. Most can be added incrementally.
Batching and retries can change timing and repeated-delivery behavior; durable
input itself is the higher-risk work already tracked in CG-02.

**Where/evidence:** Core [native network component documentation][network].

### CG-13: Decide when native queries need independent parallel execution

**Priority: P2 investigation. Complexity: High for a redesign.
Disruption risk: High for a redesign; Low for measurement.**

**What differs today:** Ordinary queries own independently scheduled nested query
graphs. Several directly assembled native queries in one graph share that
graph's controller task. They do not automatically get the same arrangement for
using several CPU workers.

**Why it matters:** CPU-heavy native workloads may scale differently from ordinary
queries. That is a design boundary to measure, not evidence that native queries
produce incorrect results or that every native workload is slower.

**What completion means:** First benchmark comparable query counts and workloads.
Then either document the supported scheduling choice or design a controlled way
to give the necessary work independent execution. Preserve one application/root
graph per DrasiLib instance and explicit ownership of every task.

**Why these ratings:** The first step is relatively small. A scheduler change is
not: it can alter ordering, fairness, cancellation, cleanup, and resource use.
Do not start with a broad rewrite or treat "more application graphs" as the fix
for insufficient parallelism.

**Where/evidence:** Core [ordinary query ownership][query-runtime],
[controller scheduling][controller], and CG-27's measurement work.

### CG-14: Manage native components from the UI

**Priority: P2. Complexity: Medium. Disruption risk: Low.**

**What is missing:** The UI can show native components, but the native component
inspector explicitly says editing and lifecycle actions are unavailable there.
Some start/stop/removal operations already exist through Server's REST API.

**Why it matters and what completion means:** Operators should not need hand-made
API calls for routine supported actions. Add the appropriate controls and show
whether a request was accepted, is still starting, has completed, or failed.
Only expose editing operations that the Server and component actually support.

**Why these ratings:** This improves usability without requiring a new runtime
model. The work is mainly UI/API integration, so implementation disruption should
be low if it calls existing operations. User-triggered stop or removal still has
real operational consequences and needs clear confirmation and error reporting.
The risk rises if this is bundled with new backend mutation behavior.

**Where/evidence:** [Server native component inspector][native-ui].
Backend operations not yet exposed belong to CG-09, rather than being hidden
inside this UI task.

### CG-15: Make more exported configurations usable for rebuilding an instance

**Priority: P2. Complexity: High. Disruption risk: Moderate.**

**What is missing:** Some providers do not fully report their dependencies or
implementation/version information. An exported description also cannot recreate
an arbitrary object supplied by application code unless the host has a recipe for
constructing it.

**Why it matters and what completion means:** "I can inspect or export this graph"
must not be mistaken for "I can rebuild this graph on a clean restart." Extend
factory and provider recipes for specific supported types. Make exports identify
which parts are reconstructible, what versions/resources they require, and which
parts remain unavailable.

**Why these ratings:** This matters for migration, cloning, and managed restart,
but does not make every existing live graph incorrect. Broad coverage is
high-complexity because provider-specific construction and cleanup differ. It can
be added incrementally; do not attempt to serialize arbitrary live objects or
invent empty settings for missing configuration.

**Where/evidence:** Core [design limits][design-limits] and
[managed resource recipes][management]. Related to CG-04 and CG-08.

### CG-16: Include native components in solution templates

**Priority: P3. Complexity: Medium. Disruption risk: Moderate.**

**What is missing:** Server solution templates currently select ordinary sources,
queries, and reactions. General native topology and its resources are not a
complete template-deployment surface.

**Why it matters and what completion means:** A reusable solution should eventually
be able to include its native components and connections, not require a separate
manual installation step. Extend the template format and deployment path, including
resource recipes, instance-specific identifiers, and clear validation errors.

**Why these ratings:** This is useful packaging once the required native
configuration and reconstruction paths work. It is not necessary to fix
lower-level execution. Complexity is moderate, but careless ID rewriting or
resource sharing could make two deployments interfere with one another. Preserve
existing ordinary templates and build on CG-10/CG-15 where the solution needs them.

**Where/evidence:** [Server native configuration and solution boundaries][server-readme].

### CG-17: Finish the GPU Cluster Lab for the standard Server

**Priority: P3, unless this is the chosen next-phase demonstration.
Complexity: High. Disruption risk: Moderate.**

**What is missing:** The embedded GPU Cluster Lab is implemented. The tracked
standard-Server version is still a reserved layout, not a complete runnable
equivalent. It needs configuration that can rebuild the components, coordination
of initial loading and readiness, and a reliable reset procedure.

**Why it matters and what completion means:** A user should be able to run the
lab on the stock Server without maintaining the embedded host program. Completion
means a documented, repeatable setup and reset flow with checks for the actual
expected observations, not just successful process startup.

**Why these ratings:** The example is relatively isolated, but bringing all of
its custom behavior through supported Server configuration is substantial work.
Keep demonstration code changes separate from any shared runtime changes it
requires.

**Important correction:** The recorded embedded result says ordinary live
acceptance passed. Strict acceptance is false because complete database-transaction
publication is unsupported, as explained in CG-18. The older lag description
should not be repeated as a newly confirmed defect.

**Where/evidence:** [Reserved Server lab][gpu-server] and
[recorded embedded compatibility result][gpu-compatibility].

### CG-18: Preserve a complete database transaction in derived context

**Priority: P2 design decision; P1 if the application requires this guarantee.
Complexity: High. Disruption risk: High.**

**What is missing:** PostgreSQL changes from a committed multi-row transaction
arrive as individual row changes. A downstream derived context is not guaranteed
to publish all those changes together. For example, a transaction that changes
several assignments can briefly be observed as some assignments changed and
others not yet reflected, even though the database committed them together.

**What completion means:** If the application needs whole-transaction observations,
define how transaction boundaries travel from the source through processing and
when downstream context becomes visible. Specify acceptable latency, buffering,
failure behavior, and interaction with other streams before implementing it.

**Why these ratings:** This is an architectural guarantee, not a missing flag.
Changing publication boundaries can affect ordering, memory, latency, and
recovery. A `TransactionTransformer` making the processing of one input atomic
does not reconstruct a missing upstream multi-row transaction boundary.

**Where/evidence:** [GPU Lab compatibility result][gpu-compatibility].
This blocks that lab's strict acceptance, not all ordinary live operation.

### CG-19: Add long-term housekeeping for the configuration store

**Priority: P2 for long-lived managed deployments.
Complexity: High. Disruption risk: High.**

**What is missing:** The managed configuration store retains request receipts and
named snapshots without automatic history pruning. It also has no automatic
encryption-key rotation.

**Why it matters:** History can grow over time, and operators need a safe way to
replace encryption keys. However, deleting old receipts can change what happens
when an old request is retried, and losing access to the correct key can make
saved configuration unreadable.

**What completion means:** Define retention and retry guarantees, provide safe
history cleanup, and add a recoverable key-change procedure with explicit old/new
key handling. Test interruption at each stage and preserve the ability to read
the current configuration.

**Why these ratings:** Existing transactional acceptance is not shown to be broken.
The difficulty and disruption come from modifying long-lived encrypted data and
the meaning of saved requests. Separate pruning and key rotation into distinct
implementation tasks.

**Where/evidence:** [Managed configuration store lifecycle][management].
This is housekeeping for configuration, not query-event retention.

## D. Areas needing more evidence, not established defects

The following items ask for stronger tests or measurements. Many already have
substantial coverage. A P1 here means "collect the missing evidence before
claiming the affected guarantee," not "rewrite the subsystem."

### CG-20: Test more configuration restoration and resource replacement failures

**Priority: P1 for managed recovery; some extended cases are P2.
Complexity: High. Disruption risk: Low for the test work.**

**What remains uncertain:** Existing tests cover important restoration and
replacement cases, but not the complete set of unavailable or failing resources.
Examples include a provider taking too long to resolve, another process holding
a network port, a crash during listener replacement, missing secrets or provider
versions, and damaged saved records.

**What completion means:** Deliberately trigger those cases and verify that the
accepted configuration remains discoverable, unfinished work is not lost, and
cleanup remains owned. Also test simultaneous configuration changes/snapshots and
an old instance handle closing after a new owner has taken over.

**Why these ratings:** These are crucial cases for dependable managed restart.
The challenge is making failures happen at repeatable points across processes
and storage providers. Tests can be added without changing production behavior;
any newly found runtime defect needs its own impact assessment.

**Where/evidence:** Core and provider/Server integration; ledger **M03-M07**.

### CG-21: Test graph lifecycle, cleanup, and resource use more thoroughly

**Priority: P1 for lifecycle correctness; sustained accounting is P2.
Complexity: Medium. Disruption risk: Low for the test work.**

**What remains uncertain:** Invalid definitions need fuller coverage through both
ordinary and native entry points. Shutdown and draining also need combinations
where one change feeds several required consumers across several stages, a
consumer becomes unavailable, and the caller cancels an operation.

**Why it matters and what completion means:** The graph must either finish the
required work or clearly retain responsibility for it. Repeatedly adding,
replacing, and removing components should not gradually accumulate tasks,
sockets, open files, or memory. Extend the failure cases and add sustained
resource measurements, rather than infer absence of leaks from a short run.

**Why these ratings:** The contracts and existing tests give this work a clear
starting point. Most changes are test harnesses and measurements. Care is needed
to distinguish legitimate retained work from a leaked resource.

**Where/evidence:** Ledger **G01, G03, G06** and
[existing graph contract checks][contract-checks].

**New foundation evidence:** The construction matrix now executes all 64 pairs
of eight pipe profiles, each with all eight direct/factory construction choices
for source, transformer and sink. Both Tokio runtime flavors check exact values,
ordering, lineage, lifecycle calls and release of all three component instances.
This is 1,024 connected pipeline combinations, not a sustained leak measurement.

### CG-22: Test delivery rules under corruption, disconnection, and heavy load

**Priority: P1 for delivery correctness; larger capacity measurements are P2.
Complexity: High. Disruption risk: Low for the test work.**

**What remains uncertain:** Delivery rules cover blocking, dropping, replay,
acknowledgment and producer closure across all eight volatile pipe profiles.
New tests also reject ten kinds of damaged persisted QoS metadata, exhausted
in-memory sequence/generation counters, and destructive capacity/history changes.
Remaining cases include malformed saved records and history combinations,
consumers joining or leaving during traffic and process failure, and the exact
notification produced when data must be skipped.

**Why it matters and what completion means:** A slow or disconnected consumer
must not silently lose work under a lossless policy. A deliberately lossy policy
must make gaps observable. Exercise these rules with long disconnections, many
consumers, and measured storage/memory limits.

**Why these ratings:** Queue behavior combines ordering, saved progress,
membership, and capacity. A useful test needs an independent record of exactly
what each consumer was owed and what it completed, not simply a final event
count. That is complex testing, but does not itself require changing delivery.

**Where/evidence:** Ledger **Q01, Q03, Q06, Q08** and
[delivery guarantees](computation-graph-qos.md).

### CG-23: Test query transactions and timers in more failure combinations

**Priority: P1 for state/recovery correctness; fairness measurements are P2.
Complexity: High. Disruption risk: Low for the test work.**

**What remains uncertain:** Failure and cancellation now run at every participant
position in one-, three- and five-stage transactions, with real RocksDB
reconstruction and exact state/progress assertions. Connected source ->
transaction -> query -> sink pipelines also survive actual process exit across
all four downstream retained/QoS pipe combinations. Remaining combinations
include live replacement during interrupted transactions; failures during
initial loading, retention, or writes; and mixed source ordering with restart.

**What completion means:** Verify that a failed transaction cannot leave only
part of its state committed and that replay produces the correct results.
For time-driven work, add failures while saving schedules, clock changes, large
bursts of due timers, and measurements of cancellation responsiveness and whether
one busy query starves another.

**Why these ratings:** These behaviors directly affect correctness, but
deterministic timing and exact expected-result checks make testing difficult.
The existing implementation should be preserved unless evidence identifies a
specific defect; do not treat this as a request to replace the evaluator.

**Where/evidence:** Ledger **T01, T02, T04-T07** and
[transaction behavior](computation-graph-transactions.md).

### CG-24: Test source and reaction recovery across more real-world cases

**Priority: P1 for recovery correctness; wider connector coverage is partly P2.
Complexity: High. Disruption risk: Low for the test work.**

**What remains uncertain:** Tests already exercise real PostgreSQL restarts and
HTTP/gRPC effects. Remaining combinations include changes arriving during initial
loading, independently slow or failing snapshots from several sources, unavailable
saved source positions, and additional connector products.

**Why it matters and what completion means:** A restart must not miss changes
between the initial snapshot and the live stream. At the output end, cancellation
combined with a failed progress write must not skip an unfinished action. Test
consumers that act only on fresh changes together with consumers that maintain a
current snapshot of the result.

**Why these ratings:** This needs controlled failures involving real external
systems, not just mocked success paths. That makes the work substantial but
largely isolated to qualification. Repeated external actions may be unavoidable
under some contracts; tests must distinguish permitted retries from lost work.

**Where/evidence:** Ledger **S01, S02, S04, S05, R01, R03**.
Missing native capabilities are separately tracked in CG-02/CG-03.

### CG-25: Test plugin compatibility and cleanup under more adverse conditions

**Priority: P1 for boundary correctness; long-running accounting is P2.
Complexity: High. Disruption risk: Low for the test work.**

**What remains uncertain:** Existing tests cover malformed inputs and important
cancellation cases, but the full set of bad metadata/buffers, incompatible
versions, and capability combinations is not established. More cases are needed
where plugin transaction calls are revoked while work is active, handles are
reused repeatedly, or callbacks arrive after cancellation.

**What completion means:** Show that unsupported combinations fail clearly,
cancelled work cannot use revoked access, and per-instance objects are eventually
released. Sustain the activity long enough to detect gradual accumulation.

**Why these ratings:** This is important because two separately built libraries
must agree about data and object lifetimes. Reproducing late callbacks and exact
release ordering makes the tests complex. The test work is low-disruption; ABI
changes found necessary belong to the relevant implementation item.

**Where/evidence:** Ledger **A01, A04, A05**. Keeping a loaded library resident for
the process lifetime is intentional and must not itself be reported as a leak.

### CG-26: Qualify the Server APIs and test infrastructure

**Priority: P1 for Server acceptance/readiness guarantees; prolonged observer tests are P2.
Complexity: High. Disruption risk: Low for the test work.**

**What remains uncertain:** More evidence is needed that REST responses correctly
distinguish accepted requests from components that are actually ready, including
delayed or failed activation. Custom and foreign component API boundaries also
need fuller coverage.

**What completion means:** Exercise those behaviors through Server, not only
through embedded Core calls. Add versioned test-infrastructure scenarios for
delivery rules and query hosting, recording which runtime and plugin versions
actually ran. Test long-lived result streams reconnecting after gaps and require
an explicit catch-up or resynchronization path rather than a stale view.

**Why these ratings:** The Server is the user-facing contract, so Core unit
coverage alone cannot establish it. The work spans Server, Core, and test-infra,
which increases complexity while leaving production behavior mostly untouched.
Close a qualification gap only with reproducible checked-in evidence.

**Where/evidence:** Ledger **H01, H03-H05**. The missing managed configuration
feature itself is CG-04, not just a missing test.

### CG-27: Establish a current performance baseline

**Priority: P2; start measurement early. Complexity: Medium. Disruption risk: Low.**

**What is missing:** The available comparisons predate later implementation
changes. They do not establish a current general speedup or slowdown.
Results also depend on the number of queries, source behavior, persistence,
transport choice, and whether ordinary adapters or native components are used.

**What completion means:** Record repeatable measurements on the current code:
processing rate, end-to-end delay, CPU and memory use, responsiveness to
cancellation, and behavior under sustained load. Compare equivalent input,
correct output, storage guarantees, and runtime/plugin versions.

**Why these ratings:** This is necessary for informed optimization and especially
for deciding CG-13, but is not evidence of a correctness failure. Existing
comparison infrastructure provides a starting point. Measurement is low-risk;
any proposed optimization should receive a separate complexity and disruption
rating after the bottleneck is demonstrated.

**Where/evidence:** [Test-infra comparison methodology and historical results][comparison].
Do not reuse old percentages as claims about today's code.

### CG-28: Automatically check the tutorials' expected behavior

**Priority: P2. Complexity: Medium. Disruption risk: Low.**

**What is missing:** The tutorial evaluation tooling prepares isolated environments
and checks the intended ComputationGraph runtime is running. It does not
automatically prove that every tutorial's actions and expected observations work.

**Why it matters and what completion means:** A healthy process can still produce
the wrong result, or no result, in a tutorial. Automate the tutorial steps and
assert the meaningful observations: the expected data, query changes, and output
effects after each action. Include useful failure messages, bounded waiting, and
a reproducible reset.

**Why these ratings:** This improves confidence in the experience users actually
follow, without changing the engine. Existing isolated setup reduces the effort,
but each tutorial has different external dependencies and expected behavior.
Treat setup/readiness success and tutorial acceptance as separate reported
outcomes.

**Where/evidence:** [learning-drasi-server evaluation workflow][tutorials].
This repository supports evaluation; it is not an implementation dependency of
the engine or Server.

## Suggested work sequence

1. **Fix misleading behavior first:** CG-05 through CG-07. Plan versioned plugin
   changes together where appropriate, but do not delay a localized status fix
   until every plugin feature is complete. Start CG-27's measurements alongside
   this work so there is a baseline before larger changes.
2. **Define the recovery promise and fill its missing pieces:** CG-01 through
   CG-03, with only the necessary parts of CG-11. Decide which uses intentionally
   remain memory-only and make that distinction visible.
3. **Make managed Server operation dependable:** CG-04 using existing
   reconstructible components first. Expand factory and provider coverage through
   CG-08/CG-15 as needed, then broaden live changes and UI support with CG-09/CG-14.
   Never introduce a second root graph or a competing configuration registry.
4. **Collect qualification evidence as each capability is delivered:** select the
   relevant CG-20 through CG-26 cases, not just normal-operation tests. The current
   runner produces a qualification report; a passing ordinary test run is not the
   same as satisfying the strict replacement qualification gate.
5. **Add use-case-driven composition and packaging:** CG-10, CG-12, CG-16,
   CG-17, and CG-28 according to the next real solution. Decide CG-18 separately
   if whole-database-transaction context is required. Schedule CG-19 before
   long-term configuration retention or key-rotation requirements become urgent.

This sequence is a recommendation, not a strict dependency graph. Some items can
proceed independently. CG-13 should remain measurement-led unless a workload
demonstrates a scheduling limitation.

## Things not to quietly turn into bug fixes

The earlier assessment also identified deliberate boundaries. These are **not
additional committed backlog items**. Adding them would require an explicit
product and architecture decision.

| Boundary | Plain-English meaning | Why it is a separate decision |
|---|---|---|
| Graph cycles | A component cannot freely feed a loop that eventually returns to itself. | Supporting feedback loops needs rules for progress, termination, buffering, and recovery. |
| Distributed execution | The current graph is not an execution system spread across several machines. | Distribution adds network failures, coordination, ownership, and deployment concerns. |
| Global ordering over unseen events | The runtime cannot order an event against a future event that has not arrived yet. | Waiting for stronger ordering needs explicit lateness/watermark and latency rules. |
| All-or-nothing deployment rollback | A failed batch change need not restore every component to its exact prior live state. | Restoring processes, resources, and external effects is a larger guarantee than applying graph changes. |
| Exactly-once arbitrary external effects | A request may reach an external service even if its reply is lost; retrying can repeat the action. | This needs cooperation such as deduplication from the destination, not just a local transaction. |
| Unloading plugin libraries while the process runs | Loaded native libraries remain resident for the process lifetime. | Unloading requires proving that no object, callback, or operation still depends on the library. |
| Automatic migration of old stored state | Old runtime state is not automatically converted into the new model. | Migration requires explicit formats, compatibility rules, and recovery procedures. |

## Keeping this document useful

Keep the CG numbers stable. When an item changes, record whether the capability
was implemented, its affected usage is now explicitly excluded, or qualification
evidence has been added. Do not close a testing item merely because code exists,
or close a feature item merely because its limitation is documented.

For high-disruption work, review the
[runtime ownership diagrams](runtime-architecture.md) and generated dependency
artifacts before and after the change. Pay particular attention to new graph
owners, background tasks, saved-state formats, and plugin contracts. Update the
qualification ledger with the evidence relevant to the chosen guarantee.

### Source baseline

These were the checked-out runtime revisions for the original assessment and
remain the revisions used for this document. The architecture documentation
added afterward does not change that runtime baseline.

| Repository | Revision |
|---|---|
| drasi-core | [`5f48406`](https://github.com/drasi-project/drasi-core/commit/5f48406bfce641d83d7f881877a4a8cac663eeac) |
| drasi-server | [`a7b564a`](https://github.com/drasi-project/drasi-server/commit/a7b564a8fbbfaf141cd078994275e09f13a798d9) |
| test-infra | [`57b5c67`](https://github.com/drasi-project/test-infra/commit/57b5c6765a120e8b8159252dce6d164c7654ffe2) |
| learning-drasi-server | [`ca5e7f9`](https://github.com/drasi-project/learning-drasi-server/commit/ca5e7f936c2607d9a99841eef5d99323863b58fc) |

Core links below open the current local files so the document remains useful
while working in the repository. Cross-repository links are pinned to the
reviewed revisions. Recheck the evidence before promoting a limitation into a
release-blocking claim or starting a large redesign.

[ledger]: ../tests/runtime_parity/requirements.tsv
[contract-checks]: ../tests/runtime_parity/computation-contracts.tsv
[callbacks]: ../../components/host-sdk/src/callbacks.rs#L381
[state-bridge]: ../../components/host-sdk/src/state_store_bridge.rs#L265
[state-proxy]: ../../components/plugin-sdk/src/ffi/state_store_proxy.rs#L137
[bootstrap-host]: ../../components/host-sdk/src/proxies/bootstrap_provider.rs#L55
[bootstrap-plugin]: ../../components/plugin-sdk/src/ffi/bootstrap_proxy.rs#L55
[lib-guidance]: ../AGENTS.md
[consumer-recovery]: ../src/computation/v1/consumer_recovery.rs
[management]: managed-configuration.md
[design-limits]: computation-graph-design.md#current-limits
[reconcile]: ../src/computation/v1/graph/reconcile.rs
[query-runtime]: ../src/computation/runtime/query.rs
[controller]: ../src/computation/v1/graph/controller.rs
[native-sdk]: ../../components/computation-plugin-sdk/README.md#explicit-abi-10-limitations
[network]: ../../components/computation-plugins/network/README.md
[server-routes]: https://github.com/drasi-project/drasi-server/blob/a7b564a8fbbfaf141cd078994275e09f13a798d9/src/api/v1/routes.rs#L81
[server-computation]: https://github.com/drasi-project/drasi-server/blob/a7b564a8fbbfaf141cd078994275e09f13a798d9/src/computation.rs#L65
[native-ui]: https://github.com/drasi-project/drasi-server/blob/a7b564a8fbbfaf141cd078994275e09f13a798d9/ui/src/components/inspector/ComputationDetails.tsx#L27
[server-readme]: https://github.com/drasi-project/drasi-server/blob/a7b564a8fbbfaf141cd078994275e09f13a798d9/README.md#native-computation-components
[gpu-server]: https://github.com/drasi-project/drasi-server/blob/a7b564a8fbbfaf141cd078994275e09f13a798d9/examples/gpu-cluster-lab/server/README.md
[gpu-compatibility]: https://github.com/drasi-project/drasi-server/blob/a7b564a8fbbfaf141cd078994275e09f13a798d9/examples/gpu-cluster-lab/embedded/compatibility.json
[comparison]: https://github.com/drasi-project/test-infra/blob/57b5c6765a120e8b8159252dce6d164c7654ffe2/e2e-test-framework/examples/building_comfort/local/native_comparison/README.md
[tutorials]: https://github.com/drasi-project/learning-drasi-server/blob/ca5e7f936c2607d9a99841eef5d99323863b58fc/README.md#isolated-runtime-evaluation
