<a id="computationgraph-consolidated-outstanding-work-inventory"></a>

# ComputationGraph: what is done and what remains

**Reviewed: 13 October 2026. This is the current work inventory, not a completion announcement.**

**Targeted evidence update: 14 October 2026.** The reproduced durable network
source-stop investigation is recorded under [CW-05](#cw-05-durable-network-source-stop).
This does not constitute a new review of the entire inventory.

**The approved execution-and-isolation work package is not fully complete.**
Independent component execution, tracked background work, several failure and
cleanup protections, child-process hosting and inter-instance links are
implemented and have recorded tests. However, the complete startup/shutdown
deadline requirement is unfinished. The requirement to avoid excessive
whole-graph work for small status changes also remains unresolved. These were
part of the approved plan, not newly discovered optional features.

Earlier statements that the entire E1/E2/B1/H1/H2/N1/Q1 package was complete were
too strong. That includes the completion wording in the
[execution and isolation plan][execution-plan]. Its descriptions and recorded
test results remain useful evidence, but its blanket completion label must not
override the item-by-item assessment here.

The 56 **CW** numbers are stable tracking numbers, not 56 missing features.
Some entries are finished fixes, some combine working code with remaining work,
some need more testing, and some are proposals or work explicitly no longer
being pursued.

## Contents

- [How to read the assessment](#reading-the-status-correctly)
- [Code, evidence and the approved scope](#code-and-evidence-used)
- [Running components and stopping safely](#running-components-handling-failures-and-stopping-safely)
- [Query correctness and background work](#query-correctness-and-component-owned-background-work)
- [Saved data, retries and input limits](#saved-data-retries-and-input-limits)
- [Errors, logs and measurements](#errors-inspection-logs-and-measurements)
- [Performance and storage growth](#performance-storage-growth-and-maintainability)
- [Evidence still needed](#evidence-still-needed)
- [Hosting, connectors, Server and examples](#hosting-connectors-server-and-examples)
- [Optional features](#optional-features-and-decisions)
- [Earlier tracking numbers](#where-earlier-tracking-numbers-went)

## Reading the status correctly

Every item separates implementation from testing:

- **Done for the stated task:** the specific change and its stated tests exist.
  This is not a claim about every possible deployment or failure.
- **Implemented, tested on named paths:** the mechanism exists and specified
  scenarios have passing evidence. Other scenarios may still need checking.
- **Partly implemented:** the entry names actual missing behavior, not just a
  wish for more tests.
- **Implementation present; evidence incomplete:** do not rebuild the feature.
  Establish the missing test evidence, fixing code only if those tests find a
  problem.
- **Finding not reverified:** an earlier report or code concern exists, but this
  review did not establish the current failing scenario or a completed fix.
  That is different from both a confirmed current bug and work not started.
- **Not implemented / not selected:** a proposed capability does not exist in
  the requested form. An unselected proposal is not unfinished approved work.
- **Retired, not fixed:** the older integration work is no longer being pursued.
  This does not mean its defects were repaired.

The **Testing** paragraphs describe inspected tests and previously recorded
runs. This documentation review did not rerun the full runtime, database,
plugin, Server or cross-platform suites. A test that exists, a test that passed
in a recorded run, and an untested scenario are not interchangeable.

**Complexity estimates concern the remaining work.** Low means a localized
change or small set of checks. Medium means several related paths or a modest
test setup. High means changes across ownership, recovery, stored formats or
several components, with substantial failure testing. Very high means a
multi-product or multi-platform effort needing its own breakdown. These are
engineering estimates, not promises of elapsed time. Where the defect itself
is unverified, implementation effort is necessarily conditional.

**P1** means required before relying on the affected behavior or completing the
affected native migration. **P2** means an important improvement. **P3** means a
capability to select when a use case needs it. Priority does not measure how
much work is already done.

## Code and evidence used

Implementation statements refer to Core
[`707d27c5`](https://github.com/drasi-project/drasi-core/commit/707d27c584dcbb27218936ed6fe06d8055b738f8)
and Server
[`f15706d`](https://github.com/drasi-project/drasi-server/commit/f15706d0d65c5c829c0a653c0d977f4cad617d6a).
The unchanged reference checkouts are learning-drasi-server `7277082` and
test-infra `55ff7ee`. All four use the existing
`agentofreality-parallel-computation-graph` branches. This describes development
code, not the guarantees of a published release.

The older review used Core `4587bd5e` and Server `f97bb59`. Old findings are not
automatically current defects. Likewise, a historical successful run is not a
new run against today's binaries.

The [test requirement ledger][requirements] has 48 scenario rows: 20 marked
covered, 23 partial and five unqualified. Those are **test-coverage labels, not
feature-completion counts**. For example, an unqualified Server row can mean
that the required complete workflow has no evidence attached to that row, even
though Server code and other Server tests exist. The
[protected test list][contracts] names many narrower checks.

The [comparison](componentgraph-vs-computationgraph.md) explains what changed
from the older runtime. The [maintainer's guide][maintainer-guide] explains the
code. The [older backlog][old-backlog] and [reliability plan][reliability-plan]
are historical sources, not additional active work lists.

<a id="which-graph-does-an-item-concern"></a>

### Runtime work versus migration work

ComputationGraph is already the only execution engine. Older source, query and
reaction interfaces still run through it. The remaining `ComponentGraph` type
is a read-only view, not a second executor.

Here **native** means a component using the newer processing/plugin interface,
not merely code compiled for the local machine. **Durable** means saved in
persistent storage for recovery after restart. A **receipt** is a saved record
of a particular request and its outcome; its retry protection lasts only as
long as the applicable retention policy allows.

The separately requested **native-only migration** replaces older component
and plugin interfaces with native components. It must preserve existing
deployment features. Do not confuse that larger migration with finishing the
selected execution-and-isolation round.

Do not expand work into repairing retiring interfaces, adding compatibility
bridges, general developer-helper projects or optional new features. Shared
query evaluation and storage still matter wherever native queries use them.
Retiring a connector does not authorize losing its required behavior.

<a id="scope-and-evidence"></a>
<a id="native-only-target-and-work-exclusions"></a>
<a id="capability-preservation-gaps-and-owners"></a>
<a id="required-plugin-failure-contract"></a>
<a id="status-and-priority"></a>

### What the previous round did, and what it did not finish

The previous plan explicitly included CW-03, CW-05 through CW-08, CW-17,
targeted status/reporting and publication work, and CW-50/CW-56. It selected
parts of those packages, not every future provider or deployment scenario.

The implemented portions include independently scheduled native components;
tracked tasks that remain owned when a caller stops waiting; catching supported
Rust panics and refusing to reuse the damaged object; unrelated management
commands progressing during selected slow operations; explicit cancellation of
the complete registered group sharing a resource; and normal child/grandchild
DrasiLib processes with separate data links.

Two important distinctions follow:

- **CW-06 is unfinished approved implementation.** Existing cleanup timeouts
  do not provide the promised complete set of phase/per-owner deadlines and an
  overall shutdown deadline.
- **CW-31 remains an approved concern.** Local status changes still rebuild
  whole-graph mappings. The cost has not been measured over growing graphs, and
  the plan's proportional-publication requirement is not demonstrated.

CW-03/CW-05/CW-07/CW-17 must not be described as wholly missing or untested:
their named mechanisms and tests exist. Their broader checks need to identify
specific missing evidence or a reproduced bug. CW-08's unrelated management
store open/close problems were explicitly not all included in the previous
round. Windows, a security sandbox and arbitrary remote-machine provisioning
were not promised by the selected Unix child-host implementation.

<a id="recommended-sequence"></a>
<a id="recommended-order-and-completion-rules"></a>

### How to use this inventory

Finish or explicitly resolve remaining requirements from the already approved
package before calling it complete. Preserve the working mechanisms rather
than starting them again. For an investigation, reproduce the problem before
planning a fix. For a testing-only gap, add the missing scenario rather than
claiming the whole subsystem is unimplemented.

Native connector and Server migration have their own missing features in
CW-43 through CW-47. Optional features remain optional. A hard limit, such as
not being able to interrupt arbitrary native code inside a process, is not an
implementable promise merely because it appears in a work list.

<a id="already-complete-do-not-reopen-as-missing-features"></a>
<a id="qualification-snapshot"></a>

### Foundations that already exist

Atomic query state/output commits, saved outgoing results, replay, saved input
positions, whole-source-transaction input, durable middleware, per-consumer
completion, managed configuration and recovery-path checks already exist in
their supported forms. The relevant items below describe their exact limits.
Do not create second implementations of them.

Recorded evidence includes focused transaction and crash tests, actual native
plugin libraries, real RocksDB reconstruction, local CPU-scaling measurements,
and macOS/Linux child-process and prepared encrypted-network cases. It does
not prove every connector, failure combination or production workload.

<a id="correctness-address-first"></a>
<a id="close-the-remaining-runtime-hardening-gaps"></a>

## Running components, handling failures and stopping safely

<a id="computationgraph-runtime-integrity"></a>
<a id="cw-50"></a>
<a id="cw-50-independent-native-node-cpu-execution"></a>

### CW-50: Let independent native components use different CPU workers

**Priority: P1. Implementation: done for the selected independent-execution task.**

**What works:** native processing runs in separately scheduled tasks, not inside
one central processing loop. The graph still controls membership and status.
Each component's own mutable work remains ordered. One stateful query is not
automatically split across several cores.

**Testing:** the recorded eight-query CPU workload checked 480,000 results over
three repetitions, including single-thread correctness. Two/four workers
achieved median throughput ratios of 1.926/3.478 against one worker, exceeding
the approved 1.25/1.5 targets. Task, panic, cancellation and independent-command
tests also exist.

**Remaining work:** sustained production measurements, graph-size costs and
native migration are CW-40, CW-31 and CW-43 through CW-47. They do not mean
independent execution has not been implemented.

**Likely complexity:** implementation **None** for the completed execution
change; testing **Low** to repeat the selected reference, **High** for the
broader measurements owned by CW-40.

**Evidence:** [task implementation][execution], [execution tests][task-tests],
[recorded workload and limits][execution-plan]. Earlier tracking: CG-13.

<a id="cw-01"></a>
<a id="cw-01-make-desired-topology-commit-atomic"></a>

### CW-01: Do not leave half a graph change applied after rejecting it

**Priority: P1. Implementation: done for the stated fix.**

**What works:** the graph checks counters, ownership and replacement connections
before changing its active definition. A successful change publishes one
consistent revision. A rejected change cannot leave some of those internal
changes applied and others absent.

**Testing:** focused regressions exercise exhausted counters, an occupied
component and a stale connection. They check that the previous objects,
counters and published state remain unchanged. Successful replacement also
checks that old control handles are rejected.

**Remaining work:** none for this fix. Earlier external stop/cleanup effects
cannot be rolled back; a later startup failure must still report that the
definition was committed. This is not an all-or-nothing deployment transaction.

**Likely complexity:** implementation **None**; testing **Low** to repeat the
focused regressions.

**Evidence:** [commit code][reconcile], [commit regressions][commit-tests].
Earlier tracking: GM-07, GM-09's Phase 0 follow-up.

<a id="cw-02"></a>
<a id="cw-02-resume-every-eligible-paused-producer"></a>

### CW-02: Try to restart every eligible component after a paused update fails

**Priority: P1. Implementation: done for the stated fix.**

**What works:** restarting one paused component can fail without preventing
attempts to restart the others. The report keeps each failure and the original
reason the update failed. A component that could not resume remains visibly
unavailable.

**Testing:** regressions deliberately prevent more than one restart and verify
that a later healthy component still starts. Stop-timeout/retry cases check
that accepted work remains available for a subsequent attempt.

**Remaining work:** none for this fix. Wider combinations of cancelled updates
and replacement failures belong to CW-07/CW-39.

**Likely complexity:** implementation **None**; testing **Low** for the focused
cases, with broader combinations estimated under their owning items.

**Evidence:** [resumption code][reconcile], [commit/resumption tests][commit-tests].
Earlier tracking: GM-10's Phase 0 follow-up, remaining GM-01.

<a id="cw-03"></a>
<a id="cw-03-contain-component-panics-and-publish-driver-failure"></a>

### CW-03: Keep a catchable component crash from taking down unrelated work

**Priority: P1. Implementation: present on the protected execution paths; complete callback coverage is not established.**

**What works:** supported component and resource tasks catch Rust panics, retain
the failed component and its cause, and prevent reuse of that damaged runtime
object. A fresh replacement can use the same logical component name after safe
cleanup. Driver failures are published rather than silently losing the driver.

**Testing:** recorded cases cover processing, control, startup/control races,
preparation, connection construction, reconfiguration and destruction of
replaced objects. They check continued healthy traffic and repair. Actual
native-library panic cases also exist.

**Remaining work:** check coverage of the remaining custom factory, provider,
schema and destruction callbacks. This review has not established a new failing
callback in every one of those categories. Do not describe them as uniformly
unimplemented. Native aborts and memory faults require the separate-process
option in CW-56; an in-process panic catcher cannot contain them.

**Likely complexity:** implementation **Medium**, conditional on a concrete
unprotected path being found; testing **High**, because failures must be
injected at several points without losing saved work or stopping healthy peers.

**Evidence:** [protected tasks][execution], [driver reporting][instance],
[execution tests][task-tests], [protected cases][contracts], [native Host tests][host-tests].
Earlier tracking: GC-03/06/15, GM-11, QX-11, RT-09, part of LI-11.

<a id="cw-04"></a>
<a id="cw-04-use-one-failure-policy-and-preserve-usable-control-channels"></a>

### CW-04: Report component errors consistently and keep control usable

**Priority: P1. Implementation: done for returned-error handling.**

**What works:** ordinary returned errors and invalid output leave the failed
node inspectable rather than ending the entire graph. Original error types
survive wrapping. Control-handler errors do not permanently close control.
Retries have explicit limits and cannot turn uncertain storage results into
safe success.

**Testing:** recorded tests on both supported Tokio execution modes check
failed nodes, healthy branches, invalid output, saturated control queues,
subsequent control messages, explicit repair and exhausted retry limits.
Actual native consumer failures retain unfinished work.

**Remaining work:** none for this returned-error fix. Catching panics is CW-03;
consistent presentation in Server/UI and logs is CW-22.

**Likely complexity:** implementation **None**; testing **Low to Medium** to
repeat the selected graph and actual-plugin regressions.

**Evidence:** [failure policy][lifecycle], [failure tests][failure-tests],
[control][control], [delivery][delivery]. Earlier tracking: GC-01/04/05, PC-05,
part of DP-08.

<a id="cw-05"></a>
<a id="cw-05-pause-and-stop-at-safe-processing-boundaries"></a>

### CW-05: Stop without abandoning accepted work; make forced interruption explicit

**Priority: P1. Implementation: graceful stop and registered-group force are implemented.**

**What works:** normal stop waits for already admitted processing, outgoing
work and required acknowledgements. A deadline does not silently authorize
aborting that work. Explicit force checks the registered components sharing
the affected resources, interrupts only an eligible complete group and reports
interruption separately from cleanup.

**Testing:** recorded cases cover blocked handling, exact results after restart,
deadline/retry, persistent continuations, active input writes, shared providers,
forced reconfiguration and an idle companion that previously prevented force
from finishing.

**Remaining work:** broader provider and hidden-sharing scenarios need checking.
An unreported private resource cannot be assumed safe. No new general failure
of the implemented force mechanism is established here. Non-yielding code
cannot be forcibly stopped in-process; killing a child stops the whole child.

**Likely complexity:** implementation **Medium** if another retained provider
needs explicit ownership reporting; testing **High** for interruption around
real commits and reconstruction.

**Evidence:** [stop tests][graceful-tests], [force/ownership tests][task-tests],
[controller][controller]. Earlier tracking: GC-02/09, DP-01. The older ordinary
subscription-setup investigation PX-04 is not a new native repair task.

<a id="cw-05-durable-network-source-stop"></a>

#### Reproduced investigation: stopping a durable network source during commit

**Priority: P1. Status: reproduced failing test; root cause and stop-contract
interpretation unresolved. Recorded: 14 October 2026.**

**Scenario:** the [native durable-admission test][native-admission-tests] gates a
real RocksDB commit immediately before or after persistence. It exercises
HTTP/gRPC, request timeout without stopping, and source stop while the commit
call remains suspended. The admission request has a 100 ms timeout and reports
`AcceptanceUnknown`: the client must resolve or retry the same producer session
and sequence, not assume the event was rejected.

**Observed failure:** from the test's ordering, the first failing stop case is
HTTP after persistence, before the asynchronous commit call returns. Ordinary
`stop_components` does not produce the test's expected completed-stop report;
it returns `ReconciliationTimeout` at the graph's default five-second cleanup
deadline:

```text
reconciliation did not reach its processing or cleanup boundary before the deadline
```

That failure occurs before this case's reconstruction and duplicate-acceptance
assertions, and before the later gRPC cases. It demonstrates a stop/recovery
qualification gap, not observed event loss or duplication, and does not
establish that both protocols have the same failure.

**Baseline evidence:** the same isolated test fails with the unchanged
pre-split network plugin library and the same host/runtime. This is not a
regression attributable to the component-crate reorganization; reproduction
against current upstream `main` has not been established.

**Reproduce:** from `drasi-core`, build the actual dynamic plugins before running
the focused test; Rust path dependencies alone do not rebuild them.

```sh
make build-native-network-plugins
cargo test -p drasi-host-sdk --test native_network_loopback \
  durable_tests::network_timeouts_and_source_stop_resolve_real_commits_without_duplicate_acceptance \
  -- --exact --nocapture --test-threads=1
```

**Remaining investigation and closure criteria:** trace ownership of the
in-flight admission through quiescence, cancellation, fencing and cleanup.
Determine whether ordinary stop is correctly waiting for accepted work and
the test instead requires explicit force or release of its commit gate, or
whether runtime lifecycle handling violates the intended contract. Do not
assume the broader [CW-06](#cw-06) deadline work is the proven cause.

Resolve the stop expectation explicitly and qualify all eight combinations
across both protocols, both commit boundaries and both interruption modes.
After supported stop/cleanup and reconstruction, retrying the same session
and sequence must leave exactly one accepted event and one downstream
delivery. A completed-stop report must not hide retained work or lost
ownership. Do not close this investigation by ignoring the test or merely
raising its timeout; any changed expectation must follow the documented
stop contract and retain exact recovery checks.

<a id="cw-06"></a>
<a id="cw-06-give-lifecycle-phases-independent-enforceable-deadlines"></a>

### CW-06: Give startup and shutdown clear time limits without losing ownership

**Priority: P1. Implementation: partly implemented; this is unfinished approved work.**

**What works:** construction, reconfiguration and cleanup have several existing
timeouts. Independent component cleanup gets execution time, and cancelling a
wait does not lose the tracked cleanup task.

**Testing:** recorded tests check slow independent cleanup, retained work after
cancelled waits, and graceful-stop timeout/retry. They do not establish the
complete deadline policy promised by E2. The separately recorded
[durable network source-stop failure](#cw-05-durable-network-source-stop)
remains an unresolved investigation, not proof of a particular deadline defect.

**Remaining work:** provide separate time limits for each startup/cleanup step,
each component or resource owner, and the whole shutdown. A component's startup
function is still awaited without its own startup deadline. Several cleanup
categories get separate waits using the shared `cleanup_timeout`; resource
disposal also waits through owners in sequence. Those waits are not one overall
shutdown limit. On expiry, report what finished, what remains and who still
owns it. Do not falsely report that arbitrary synchronous code was interrupted.

**Likely complexity:** implementation **High**, because the policy affects
startup, updates, cleanup, cancellation and public outcomes; testing **High**
for slow/failing owners in different positions and retries after partial work.

**Evidence:** [start/cleanup implementation][controller], [timeout setting][graph],
[cleanup tests][task-tests], [approved E2 requirement][execution-plan].
Earlier tracking: GC-07/09/11, GM-08, QX-06.

<a id="cw-07"></a>
<a id="cw-07-own-and-compensate-multi-step-updates"></a>

### CW-07: Keep accepted updates owned and let unrelated commands finish

**Priority: P1. Implementation: selected independent-operation paths implemented; broader operation handling is not fully verified.**

**What works:** unrelated start/stop commands can finish while another component
is waiting. Overlapping operations retain ordering. Reconfiguration work survives
the caller stopping its wait, and the graph checks actual shared resources
before allowing conflicting work.

**Testing:** recorded tests hold startup, processing, drain or reconfiguration
at controlled points and complete unrelated commands. They exercise cancelled
callers, explicit force, replacement and delayed resource observations on both
execution modes.

**Remaining work:** finish checking combinations of accepted managed updates,
conflicting stop/update/remove commands, failed replacement and additions during
startup. Identify a failing case before claiming another executor or operation
tracker is missing. Do not repair the retiring ordinary update implementation
as a separate project.

**Likely complexity:** implementation **Medium** if combined cases reveal a
missing completion/recovery path; testing **High**, because applied definitions,
unfinished work and conflicts must be checked independently.

**Evidence:** [reconciliation][reconcile], [reconciliation tests][reconcile-tests],
[independent command tests][task-tests]. Earlier tracking: RT-01/02; LI-05
remains an investigation rather than a proven lost-start incident.

<a id="cw-08"></a>
<a id="cw-08-make-cleanup-management-close-and-rejected-resources-reliable"></a>

### CW-08: Make failed or cancelled cleanup retryable and keep every error

**Priority: P1. Implementation: partly implemented.**

**What works:** selected graph cleanup, rejected-resource disposal and removal
of replaced objects use tracked work. Independent owners are attempted, errors
remain available, and an unfinished task is not treated as released storage.
Instance stop and shutdown have retry tests.

**Testing:** recorded cases cover failed builder cleanup, rejected registration,
cancelled shutdown, retained resource operations and force cleanup. These do
not cover every management-store open/close failure.

**Remaining work:** management close still removes its driver handle before
awaiting it; cancellation after a close request can make a subsequent attempt
fail to observe/join the same operation. Failed open can replace the original
failure with a close failure. Drop-triggered cleanup has an open-ended retry
loop rather than the full bounded/reportable policy. Preserve both primary and
cleanup causes. Coordinate remaining deadline work with CW-06.

These management-store cases were not all included in the previous execution
round; its runner/resource cleanup must not be reopened as wholly absent.

**Likely complexity:** implementation **Medium**; testing **High** for cancelled
close, failed restore, store ownership and repeated cleanup attempts.

**Evidence:** [management open/close][management-runtime], [instance tests][instance-tests],
[resource tasks][resource-tasks]. Earlier tracking: GM-02, RT-07/14, LI-03/04/10.

<a id="cw-09"></a>
<a id="cw-09-make-topology-validation-and-ownership-explicit"></a>

### CW-09: Apply the same connection and ownership rules when building or changing a graph

**Priority: P2. Implementation: partly implemented; some policy choices remain.**

**What works:** graph construction validates components, ports, connections and
declared resources. Explicitly incomplete graphs are supported.

**Testing:** construction and live-change tests exist. The complete comparison
of addition, replacement, export and reconstruction rules is not established.

**Remaining work:** resolve inconsistent handling of subscriptions whose source
does not exist. Live additions can leave `allow_incomplete` enabled in exported
definitions. Cleanup still recognizes some attached resources by the `attached/`
name prefix, which needs a safe ownership rule rather than trusting a name.
Readiness explanations should identify the actual missing prerequisite.

**Likely complexity:** implementation **Medium**; testing **Medium** for
add-before-source, removal, user-chosen names and export/rebuild combinations.

**Evidence:** [addition][addition], [live changes][reconcile], [definitions][topology].
Earlier tracking: GM-03/15, original GM-09, part of GC-14.

<a id="cw-10"></a>
<a id="cw-10-align-status-read-handles-and-public-operation-outcomes"></a>

### CW-10: Show the same truthful status and query results through every supported API

**Priority: P1. Implementation: partly implemented.**

**What works:** failed native nodes remain visible; query readers check which
runtime version they belong to; snapshots, saved outgoing results and live
subscriptions have native implementations. Driver failures and member-level
stop failures are retained.

**Testing:** native API, read-handle, failure, restart and instance-stop tests
exist. That is not proof of every Server workflow or complete live-subscription
wiring after restoration.

**Remaining work:** complete the supported native/Server outcomes for partial
startup/shutdown and missing/duplicate components. Standard query assembly must
actually connect the result catalogue and output needed for live subscriptions,
including after reconstruction; CW-43 owns that wiring. Check stale readers,
snapshot-to-live handover and missing history. Do not create a second evaluator
or repair older-only API parity as a separate goal.

**Likely complexity:** implementation **High** across assembly, readers and
Server responses; testing **High** for replacement, restart and subscription races.

**Evidence:** [native catalogue][catalog], [inspection][runtime-inspection],
[instance][instance], [native API tests][api-tests]. Earlier tracking: RT-03
through RT-06, LI-02/09/11/13; ordinary-only portions are retired.

## Query correctness and component-owned background work

<a id="shared-input-evaluator-and-plugin-correctness"></a>
<a id="cw-11"></a>
<a id="cw-11-reject-stale-mutations-and-define-duplicate-element-behavior"></a>

### CW-11: Prevent old or duplicate changes from corrupting current query results

**Priority: P1. Implementation: done for the stated mutation rules.**

**What works:** older changes cannot overwrite newer source-dated state.
Repeated inserts replace the existing row rather than counting it twice.
Deletes save enough history to prevent an old insert resurrecting a deleted
row after restart. Conflicting node/relationship identities fail before changing
the existing object. Equal-time input follows admitted order.

**Testing:** focused evaluator tests and an independent persistent model check
duplicates, all insert/update/delete orders, source-specific identities,
rollback and reconstruction. Real RocksDB cases check saved positions and rows.

**Remaining work:** none for these rules. Automatic deletion-history expiry
requires a separate source-history policy; it is not silently enabled.
Older saved query configurations require explicit supported reset/rebuild or
a separately designed migration, not reinterpretation of existing rows.

**Likely complexity:** implementation **None**; testing **Medium** to repeat the
persistent model and backend checks.

**Evidence:** [evaluator][evaluator], [mutation tests][mutation-tests],
[persistent model][persistent-mutation-tests]. Earlier tracking:
[#244][i244], [#507][i507], [#805][i805].

<a id="cw-12"></a>
<a id="cw-12-preserve-real-aggregate-rows-and-remove-genuinely-empty-groups"></a>

### CW-12: Distinguish an empty result group from a real group whose answer is zero

**Priority: P1. Implementation: aggregation exists; the listed edge-case fixes are not verified complete.**

**What works:** ordinary grouping and aggregation run in the shared evaluator.
Previous fixes to aggregate identity are already present and are not this task.

**Testing:** aggregate and persistence tests exist. Earlier reports cover
missing first results, incorrect removal of groups and averages containing null
values. This review checked the retained implementation but did not rerun reduced
reproductions of every reported case or establish a completed fix for them.

**Remaining work:** reproduce each case against the current evaluator. A real
group must not disappear just because its count/value is zero; a genuinely
empty group must disappear. Check initial Add, later Update/Delete and restored
averages. Check that old group bookkeeping is removed during repeated creation
and deletion. Any stored-representation change needs an upgrade decision.

**Likely complexity:** implementation **High** if contributor/group state must
change; testing **High** across nested aggregates, nulls, restart and churn.
Reproduction comes first.

**Evidence:** [group processing][query-parts], [average][average].
Earlier reports: [#384][i384], [#498][i498], [#806][i806], [#411][i411].
PRs [#409][p409]/[#907][p907] are historical leads, not proof of completion.

<a id="cw-13"></a>
<a id="cw-13-correct-matching-scope-and-list-comprehension-behavior"></a>

### CW-13: Correct the reported query matching and variable-scope edge cases

**Priority: P1. Implementation: query features exist; the reported fixes are not verified complete.**

**What works:** matching, optional matches and list expressions already work
for supported cases. This is not an unimplemented query engine.

**Testing:** existing tests cover many normal queries. The earlier assessment
reported optional-filter, outer-variable, list-membership and relationship-end
errors. This review did not freshly reproduce or close every reported case.

**Remaining work:** use reduced examples to establish the current failures.
For example, filtering an optional member must not remove its unmatched team;
an A-to-B relationship must not prove an A-to-C connection. Then check multiple
candidates, changed/deleted relationships and both supported solving modes.
Do not declare all optional matching broken, or fixed, from one example.

**Likely complexity:** implementation **High**, because query scope and matching
interact; testing **High** for exact rows across updates and both solver modes.

**Evidence:** [evaluator][evaluator], [query parts][query-parts], [matching][solver].
Earlier reports: [#795][i795], [#796][i796], [#797][i797], [#798][i798],
[#807][i807], [#808][i808], [#812][i812]. PRs
[#904][p904]/[#912][p912]/[#915][p915] are reuse leads.

<a id="cw-14"></a>
<a id="cw-14-enforce-source-history-and-subscription-ownership-contracts"></a>

### CW-14: Verify that a native source can really resume from the requested position

**Priority: P1. Implementation: recovery mechanisms exist; native-source coverage is incomplete.**

**What works:** native progress, durable input and coordinated initial snapshots
have implementations. Missing retained history must cause a visible failure or
an explicitly selected recovery action.

**Testing:** native framework and selected source recovery tests exist.
Historical ordinary-source dispatcher/filter reports do not establish the same
bug in native sources.

**Remaining work:** test real native sources when the requested position is too
old, history is removed, snapshots fail, or live input arrives during initial
loading. Check cancellation and independent subscriptions. Identify actual
native failures before applying an old SourceBase patch.

**Likely complexity:** implementation **Medium**, conditional on the source
failure found; testing **High** because source retention and snapshot/live
handover need real provider controls.

**Evidence:** [native progress/recovery][query-recovery], [bootstrap services][native-sdk],
[source recovery tests][source-recovery-tests]. Earlier reports:
[#721][i721], [#722][i722], [#723][i723], [#919][i919], [#891][i891].

<a id="cw-15"></a>
<a id="cw-15-make-plugin-snapshot-and-source-discontinuities-visible"></a>

### CW-15: Older plugin snapshot errors that were not fully surfaced

**Priority: no active ordinary-integration work. Implementation: retired, not fixed.**

**What works:** the previously fixed live-change decode error is reported
instead of silently skipped. The remaining historical findings concern the
older snapshot callback and source adapter.

**Testing:** those findings are retained as history, not evidence that native
streams have the identical defect.

**Remaining work:** no separate repair of the retiring integration is selected.
Native streams and initial loading must still report loss/failure explicitly;
their evidence belongs to CW-03/CW-04/CW-39/CW-41.

**Likely complexity:** implementation/testing **Not applicable** to this retired
task. Any newly reproduced native problem must be estimated under its actual owner.

**Evidence:** [older change receiver][change-receiver], [older adapter][legacy-source].
Earlier tracking: PX-13's Phase 0 follow-up, PX-03, remaining PX-01.

<a id="cw-16"></a>
<a id="cw-16-complete-retained-plugin-ownership-and-lifecycle-contracts"></a>

### CW-16: Remaining ownership problems in the older plugin interface

**Priority: no active ordinary-SDK expansion. Implementation: retired, not fixed.**

**What works:** the older SDK remains present while migration proceeds. Some
ownership and durability fixes already exist, but this does not close every
reported callback/context problem.

**Testing:** historical reports concern snapshot callbacks, reaction context,
subscription responses and cleanup. They are not blanket native-ABI findings.

**Remaining work:** do not expand the older SDK to complete this entry.
Required native ownership and failure behavior stays in CW-03/CW-17/CW-41/CW-44,
and must work without optional author-helper APIs.

**Likely complexity:** implementation/testing **Not applicable** to the retired
scope; native replacement is assessed separately.

**Evidence:** [older Host proxies][old-proxies], [older SDK ownership][old-sdk].
Earlier reports: [#646][i646], [#648][i648], [#649][i649], [#774][i774],
[#815][i815], [#896][i896]. PRs [#903][p903]/[#906][p906] are historical leads.

<a id="cw-17"></a>
<a id="cw-17-register-every-workers-owner-before-spawning"></a>

### CW-17: Keep background work owned until it has actually finished

**Priority: P1. Implementation: engine-owned work implemented; private component/provider work not fully checked.**

**What works:** the engine records its processing, lifecycle, control and
supported resource tasks before scheduling them. Cancelling a caller's wait
does not discard the actual task handle. Old task completions cannot be
applied to a newer replacement.

**Testing:** recorded tests cover cancelled cleanup, pending resource results,
late observations, force, callback faults and independent processing/control.
This is not work that has not started.

**Remaining work:** inspect retained native components/providers that start
their own tasks or threads. The graph cannot automatically track arbitrary
private native work. Establish a real lifetime boundary or use process
isolation; do not claim optional helper use is a guarantee. This review does
not identify every such component as defective.

**Likely complexity:** implementation **Medium** for a concrete unowned worker,
potentially **High** for blocking work; testing **High** for setup/stop races,
cancelled waits and resource reuse.

**Evidence:** [graph tasks][execution], [resource operations][resource-tasks],
[worker ownership][workers], [execution tests][task-tests]. Earlier tracking: LI-06.

## Saved data, retries and input limits

<a id="persistence-wire-formats-and-delivery-integrity"></a>
<a id="cw-18"></a>
<a id="cw-18-own-and-version-payload-retry-and-configuration-identity"></a>

### CW-18: Make stored formats and retry identities safe to change between versions

**Priority: P1. Implementation: partly implemented.**

**What works:** native ABI/wire versions, bridge versions and query configuration
versions are checked. Unsupported versions and conflicting retry identities
can fail explicitly.

**Testing:** version rejection, retry-content and selected bridge reconstruction
tests exist. They are not a complete old-version compatibility suite.

**Remaining work:** some graph payloads still serialize evolving Core types
under an unchanged schema description. Define which bytes and fields determine
stored data, retry identity and configuration identity; use fixed reference
bytes/digests. Read old formats explicitly or reject them with a documented
rebuild path. Diagnostics must not accidentally change business identity.
An ordinary configuration-hash change is a cutover concern, not an older-adapter
improvement project.

**Likely complexity:** implementation **High** for format ownership and upgrade
rules; testing **High** across old/new bytes, replay and partial upgrades.

**Evidence:** [graph encoding][graph-codec], [query encoding][query-codec],
[output identity][output-identity], [query configuration][query-configuration],
[bridge code][bridge-codec]. Earlier tracking: PX-02/07/09, QX-03.

<a id="cw-19"></a>
<a id="cw-19-bound-decoded-structure-and-recursive-destruction"></a>

### CW-19: Reject excessively deep data before it can crash the process

**Priority: P1 where input can abort the process. Implementation: bridge protection present; general protection incomplete.**

**What works:** the inter-instance bridge checks encoded size, collection counts,
nesting and ancestry before normal decoding of its three supported data formats.
Peer data cannot select arbitrary executable validators.

**Testing:** bridge tests include very deep and malformed input, oversized
collections and standard data/transaction round trips.

**Remaining work:** arbitrary custom formats, other persisted/shared value
decoders and deeply nested objects built inside the process are not all covered.
Releasing a deep recursive object can also overflow the stack even if decoding
was bounded. Add appropriate limits and safe release behavior for supported
paths; test failed decoding, cancellation and destruction.

**Likely complexity:** implementation **High**, particularly safe destruction;
testing **High**, including generated hostile structures in isolated test processes.

**Evidence:** [bridge checks][bridge-codec], [bridge tests][bridge-codec-tests],
[graph encoding][graph-codec], [value encoding][value-codec].
Earlier tracking: PX-05/06.

<a id="cw-20"></a>
<a id="cw-20-qualify-interrupted-deprovision-across-providers"></a>

### CW-20: Check that a crash during query reset cannot leave partly cleared state usable

**Priority: P1. Implementation: reset markers and generation changes exist; provider coverage is incomplete.**

**What works:** native reset/deprovision records that clearing has begun and
advances output identity. Incomplete clearing is not supposed to look like a
normal saved query on restart. The old claim that no marker exists is obsolete.

**Testing:** real child-process deaths at six selected reset/deprovision points
were recorded on macOS and Linux using RocksDB. Tests check actual saved rows,
markers, exclusive ownership, strict refusal and an explicit successful retry.

**Remaining work:** extend evidence to the other supported providers and
weaker optional storage methods. Separate clearing commits must never permit
partly cleared state to resume as healthy. Do not implement the marker again.

**Likely complexity:** implementation **Medium** if a provider cannot honor the
existing protocol; testing **High** for real crash/reopen points per provider.

**Evidence:** [reset code][query-recovery], [reset/reopen tests][query-recovery-tests].
Earlier tracking: narrowed QX-04; QX-01's output-identity reuse is fixed.

<a id="cw-21"></a>
<a id="cw-21-resolve-remaining-admission-receipt-and-acknowledgement-edge-cases"></a>

### CW-21: Resolve the remaining questions about what was accepted or acknowledged

**Priority: P1. Implementation: core receipt/retry mechanisms exist; these findings need focused verification.**

**What works:** saved input receipts, output replay, consumer completion and
merger recovery have implementations and tests. This item is not one confirmed
data-loss incident.

**Testing:** broad tests exist, but the following particular concerns have not
all been resolved by a named reproduction or proof that they cannot occur.

**Remaining work:** check clock changes between merger decisions; retrying input
with no original timestamp; whether middleware confirmation checks enough
content; replay older than a merger's latest saved receipt; and whether an
already failed storage owner incorrectly reports a never-attempted write as
uncertain. For each, distinguish rejected work, saved work and an unknown result.
Do not silently reinterpret an old or conflicting retry as new input.

**Likely complexity:** implementation **Medium** after reproduction, but an
expanded saved-receipt format could be **High**; testing **High** for exact
positions, content and crash/retry behavior.

**Evidence:** [merger][time-merge], [consumer recovery][consumer-recovery],
[middleware recovery][middleware-recovery], [saved queues][qos].
Earlier tracking: DP-06/07/09/11, PC-09.

## Errors, inspection, logs and measurements

<a id="error-reporting-observation-and-observability"></a>
<a id="reconcile-error-reporting-and-consumption"></a>
<a id="cw-22"></a>
<a id="cw-22-reconcile-error-reporting-and-consumption"></a>

### CW-22: Make errors explain what stopped, what is saved and what can be retried

**Priority: P1 for the reporting requirement. Implementation: partly implemented.**

**What works:** typed causes, retained failed nodes, task/process failures and
several cleanup reports exist. They are not all missing because a broader
reporting plan remains open.

**Testing:** failure-policy, driver, native-plugin and cleanup tests check
selected preserved causes. They do not establish consistent presentation through
every Server response, UI view, log, metric and external alert consumer.

**Remaining work:** present the same incident consistently, including component
identity, whether processing stopped, saved unfinished work and failed cleanup.
Distinguish a rejected request from a failed component. Keep safe useful causes,
not only a phase or HTTP 500. Demonstrate diagnosis and recovery from Rust and an
external consumer. Report a skipped range only after the saved position changes.

**Likely complexity:** implementation **High** across Core, plugins and Server;
testing **High** for the same failure viewed through each supported interface.

**Evidence:** [errors][errors], [controller][controller], [delivery][delivery],
[Server routes][server-routes]. Earlier tracking: original GM-10, PC-04/06/07,
PX-10, RT-11, LI-02 and the error-consumption proposal.

<a id="cw-23"></a>
<a id="cw-23-make-observation-loss-instance-identity-and-host-controls-explicit"></a>

### CW-23: Tell observers when they missed updates and how to get current state again

**Priority: P1. Implementation: current inspection and selected host reporting exist; the complete observation behavior is unfinished.**

**What works:** graph snapshots are coherent, changes carry versions, and the
child host retains bounded last-known state with identity/failure information.

**Testing:** inspection, generation and host tests exist. A complete long-lived
snapshot-plus-live-feed/reconnect sequence across Server and logs is not proven.

**Remaining work:** define a snapshot/live-update handover that either loses
nothing or explicitly reports a gap and requires resynchronization. Check slow
observers, reconnects and replacements. Logs need unambiguous instance identity
and instance-owned state must be released. Recovery completion, reset and any
explicitly skipped input range also need usable notifications. Decide separately
whether durable incident history belongs in Drasi or an external system.

**Likely complexity:** implementation **Medium to High** across inspection and
Server; testing **High** for gaps and stale observers under load.

**Evidence:** [inspection publication][inspection], [component logs][component-logs],
[host behavior][host-readme]. Earlier tracking: LI-07/08, GM-13,
[#632][i632], [#355][i355]. Older introspection-adapter repairs are retired.

<a id="cw-24"></a>
<a id="cw-24-establish-application-owned-observability-and-logging"></a>

### CW-24: Let the embedding application control logging and monitoring

**Priority: P2. Implementation: existing logging/profiling works; the full proposal is not implemented.**

**What works:** component logs, metrics and optional profiling exist today.
That is different from the proposed common instrumentation/configuration model.

**Testing:** existing facilities have tests. The proposed complete off/basic/
debug/persistence profile behavior has no complete acceptance evidence here.

**Remaining work:** implement the selected parts of the
[monitoring proposal][observability-design] using established tracing/metrics
libraries. The application chooses collection, filtering, export and flushing.
Drasi must not silently replace its logging setup. Turning off new measurements
must not disable existing application/component logs. Resolve the unused
profiling interfaces and dependency-upgrade questions explicitly rather than
assuming a different exporter has resolved them. Preserve current log history/
subscriptions, inspection measurements and optional profiling.

**Likely complexity:** implementation **High** across initialization and existing
interfaces; testing **Medium to High** for embedding combinations, filtering,
shutdown and overhead.

**Evidence:** [tracing integration][tracing-layer], [metrics][metrics],
[component logs][component-logs]. Earlier tracking: observability A,
R1/R3/R4/R5/R8/R9, [#633][i633].

<a id="cw-25"></a>
<a id="cw-25-carry-trace-context-and-valid-timing-through-the-complete-path"></a>

### CW-25: Follow one request through queues, plugins and network calls

**Priority: P2. Implementation: some identity/context support exists; full tracing is not implemented.**

**What works:** envelopes carry context and the selected task/host/link work
includes useful identities. This is not complete end-to-end distributed tracing.

**Testing:** selected context-preservation and bridge tests exist. Continuous
trace relationships across all input protocols, plugin calls, retries and actual
destination completion are not established.

**Remaining work:** preserve validated trace context from ingress through
processing and real outbound work, including supported control requests.
A retry gets a new attempt without changing saved business identity. Define
how joined/batched causes are represented. Saved context needs a versioned data
format, not live tracing objects; plugin interface extensions must preserve
compatibility. Measure durations with a consistent monotonic clock and do not
invent a duration across restart or mistake HTTP 202 for completed work.

**Likely complexity:** implementation **High** across protocols and the native
plugin interface; testing **High** for retries, batching, cancellation and restart.

**Evidence:** [envelopes][envelope], [native SDK][native-sdk], [network plugins][network].
Earlier tracking: observability B/D/E, R2/R7/R10. The previous execution round
included only the minimal correlation/timing portion, not this whole proposal.

<a id="cw-26"></a>
<a id="cw-26-implement-the-shared-signal-catalogue-without-double-counting"></a>

### CW-26: Count work, errors, queue pressure and processing time consistently

**Priority: P2. Implementation: existing measurements present; the proposed shared set is incomplete.**

**What works:** runtime, queue, query and component measurements already exist.
Some loss/failure diagnostics are also tested.

**Testing:** current measurements have scoped tests. The complete proposed
instrument set, including native-plugin consistency and no double counting,
does not have complete evidence.

**Remaining work:** consistently measure component availability; source events;
query inputs/results; destination handling; queue depth, peak, blocked sends and
drops; saved-position lag; errors; query-engine duration; and actual end-to-end
completion time. Preserve the proposed names and units when implementing them.
Keep labels bounded and exclude payloads, secrets and error text. Plugins should
reuse host instrumentation rather than each owning exporters.

The existing proposal's initial names remain:

- Current values (gauges): `drasi.component.up`, `drasi.queue.depth`,
  `drasi.queue.depth_max`, `drasi.reaction.checkpoint_lag_events`.
- Running totals (counters): `drasi.source.events_dispatched`,
  `drasi.query.events_processed`, `drasi.query.results_emitted`,
  `drasi.reaction.results_processed`, `drasi.queue.blocked_enqueues`,
  `drasi.queue.drops`, `drasi.errors`.
- Duration distributions (histograms): `drasi.query.engine.duration`,
  `drasi.pipeline.end_to_end.duration`.

Record all enabled events before potentially lossy export. Distinguish instances
and actual queues without unbounded labels. Durations use seconds; query-engine
time excludes committing storage, and initial loading is distinct from live
processing. Extra future measurements must be selected explicitly rather than
silently added to this initial commitment.

**Likely complexity:** implementation **High** across processing and plugin
interfaces; testing **High** for exact counts, failure/retry behavior, timing
boundaries and disabled/enabled overhead.

**Evidence:** [metrics][metrics], [queue measurements][pipe-metrics],
[monitoring proposal][observability-design]. Earlier tracking: observability C/D/H,
R6/R7, PX-01's remaining counter.

<a id="cw-27"></a>
<a id="cw-27-provide-isolated-otlp-and-local-telemetry-consumption"></a>

### CW-27: Export monitoring data without letting monitoring stall business work

**Priority: P2. Implementation: proposed adapter not implemented.**

**What works:** the OTel source can ingest application telemetry. That is not
the proposed exporter for Drasi's own logs, metrics and completed traces.

**Testing:** tutorials and existing telemetry features are not acceptance
evidence for this new combined export/local-consumption facility.

**Remaining work:** supply an optional application-owned adapter that aggregates
once and independently sends to OTLP, local Drasi consumers, or both. Each
destination needs bounded queues and visible loss. Slow monitoring must not
block business processing. Prevent monitoring from recursively monitoring its
own output; define flush and shutdown ownership.

**Likely complexity:** implementation **High**; testing **High** for slow/broken
destinations, combined modes, recursion, instance isolation and shutdown.

**Evidence:** [proposal][observability-design], [OTel source][otel-source].
Earlier tracking: observability F/G and proposal acceptance requirements.

## Performance, storage growth and maintainability

<a id="efficiency-resource-bounds-and-rust-structure"></a>
<a id="performance-and-resource-management-improvements"></a>
<a id="cw-28"></a>
<a id="cw-28-improve-durable-source-storage-and-persistent-hygiene"></a>

### CW-28: Reduce storage overhead and check long-term disk growth

**Priority: P2. Implementation: storage works; these improvements are not complete.**

**What works:** durable source logs and index providers exist. Their presence
does not imply efficient grouped writes or bounded growth for every workload.

**Testing:** storage/recovery tests and selected persistent benchmarks exist.
Historical slowdown reports are not current measurements of this branch.

**Remaining work:** measure and, where needed, group durable writes within
explicit size/time limits. Check multi-query disk growth, obsolete relationship
references and old aggregate bookkeeping. Support required per-query archive
choices without changing other users. Reproduce the old Garnet count concern
against its current transaction implementation before proposing that fix.

**Likely complexity:** implementation **High** across providers and write
semantics; testing **High** for throughput, tail latency, disk growth and crashes.

**Evidence:** [source log][redb-wal], [RocksDB][rocksdb], [provider selection][index-factory].
Earlier reports: [#668][i668], [#782][i782], [#309][i309], [#635][i635],
[#411][i411], [#276][i276].

<a id="cw-29"></a>
<a id="cw-29-extend-byte-budgets-and-choose-transformer-admission-behavior"></a>

### CW-29: Finish the selected memory/queue limits and define what a full buffer does

**Priority: P2. Implementation: partly implemented; some behavior needs a decision.**

**What works:** selected ordinary, retained and saved multicast queues have
count/byte limits; persistent queues support bounded read pages. Prior paged
trimming errors were fixed. These are not total-process memory limits.

**Testing:** byte budgets, oversized records, reopen, required subscribers and
paged caches have tests. The shared-transaction queue's byte reservation and
outgoing-result/live-row byte limits are not complete.

**Remaining work:** add agreed limits where missing while preserving reservation
before the processing commit. Define oversized-input behavior. A full merger
currently fails explicitly; changing it to wait needs a selected policy, not a
claim that waiting already exists. Measure slow-subscriber behavior before
changing result publication.

**Likely complexity:** implementation **High** for shared-transaction reservation
and output limits; testing **High** for capacity, crashes and retained obligations.

**Evidence:** [shared queue][shared-qos], [saved output][query-state],
[merger][time-merge], [byte/page tests][budget-tests]. Earlier tracking:
DP-02, QX-09/12, Q08; PC-01's old trimming bug is fixed.

<a id="cw-30"></a>
<a id="cw-30-reduce-whole-state-persistence-and-replay-round-trips"></a>

### CW-30: Avoid rewriting or rereading large amounts of state for one small change

**Priority: P2. Implementation: existing recovery works; optimization is incomplete.**

**What works:** merger state, receipts, replay and consumer progress are saved.
Paging already exists for selected persistent queues.

**Testing:** recovery and selected cost tests exist. A measured, complete
optimization of merger snapshots, replay reads and receipt updates is not present.

**Remaining work:** measure full-state rewrites and per-item storage calls,
then use bounded reads or smaller updates where worthwhile. Preserve exact
retry identity and saved progress. Never combine acknowledgements beyond what
actually finished, or move related writes out of the same transaction.

**Likely complexity:** implementation **High** if saved formats change;
testing **High** for compatibility, failed writes, replay and measured benefit.

**Evidence:** [merger persistence][merge-recovery], [delivery][delivery],
[source replay][wal-source], [query delivery][query-delivery], [queues][qos].
Earlier tracking: PC-03, DP-03/04/07, QX-10.

<a id="cw-31"></a>
<a id="cw-31-publish-changes-and-wake-work-proportionally"></a>

### CW-31: Avoid rebuilding whole-graph status data for one small change

**Priority: P1 for the approved graph-size requirement; P2 for unrelated idle overhead. Implementation: unresolved.**

**What works:** each published view is consistent. Independent processing no
longer runs inside the controller.

**Testing:** small-graph execution and CPU-scaling tests exist. They do not
measure increasing graph size or startup/status-publication cost.

**Remaining work:** a local controller update still rebuilds graph-wide
configuration, query-reader and resource mappings. That code behavior is
confirmed; an unacceptable measured slowdown is not yet established. Measure
larger graphs, set acceptable costs, and reduce or appropriately batch the work.
This was named in E2 and cannot be closed by the eight-query CPU reference.
Separate idle polling and broad-wakeup improvements should follow measurements.
This is not a claim that every data record rebuilds the graph.

**Likely complexity:** implementation **High** while preserving consistent
snapshots; testing **High** across graph size, observers, allocation and latency.

**Evidence:** [controller update][controller], [registry publication][registry],
[scheduling][query-scheduling]. Earlier tracking:
GC-08/14, GM-12, RT-08/13, DP-04, QX-08.

<a id="cw-32"></a>
<a id="cw-32-remove-measured-hot-path-duplication"></a>

### CW-32: Remove repeated work only where measurements show it matters

**Priority: P2. Implementation: some optimizations complete; remaining candidates need measurement.**

**What works:** direct memory providers and reduced disabled-feature overhead
are already implemented. Selected fast-path and persistent workloads were
measured; those improvements must not be reopened as missing.

**Testing:** exact-output benchmarks and recovery tests exist. Repeated encoding,
hashing, receipt scans and remaining native interface costs have not all been
quantified and removed.

**Remaining work:** profile retained native paths, then cache genuinely
unchanging metadata or reuse encoded bytes safely. Preserve validation of
untrusted input and fixed float/identity representations. Retiring ordinary-only
interface round trips are not an optimization project.

**Likely complexity:** implementation **Medium** for a measured localized cost;
testing **High** where retry identity or saved output can change.

**Evidence:** [encoding][graph-codec], [identity][output-identity], [queues][qos],
[workload measurements][workload-runner]. Earlier tracking: PC-08, DP-08,
PX-08/11, original PX-13.

<a id="cw-33"></a>
<a id="cw-33-bound-retired-topology-metadata"></a>

### CW-33: Keep removed-component bookkeeping from growing forever

**Priority: P2. Implementation: task retirement exists; long-lived graph storage remains unresolved.**

**What works:** completed owned tasks can be joined and removed, and replaced
runtime objects have tracked destruction. Selected bounded churn tests exist.

**Testing:** task/file/socket cleanup and repeated replacement were checked.
Those checks do not establish bounded configuration/slot memory in one graph
that continually adds and removes different component names.

**Remaining work:** removed components still leave slots in graph collections.
Measure unique-name and reused-name churn, then reclaim safe metadata without
forgetting unfinished cleanup, saved-output obligations or stale-handle checks.
Distinguish necessary retained records from unnecessary growth.

**Likely complexity:** implementation **Medium to High** because indexes and
replacement checks must remain valid; testing **High** for long-lived churn and
retained failure/recovery state.

**Evidence:** [removal code][reconcile], [graph storage][graph], [resource soak][resource-soak].
Earlier tracking: GM-04, GC-12. Selected task bookkeeping and general graph
compaction are different obligations.

<a id="cw-34"></a>
<a id="cw-34-consolidate-duplicated-algorithms-and-explicit-state-machines"></a>

### CW-34: Simplify duplicated recovery logic without changing its behavior

**Priority: P2. Implementation: some shared mechanisms extracted; broader cleanup incomplete.**

**What works:** task ownership, resource jobs and several transaction/delivery
mechanisms are shared already.

**Testing:** their existing regression tests establish selected behavior, not
the safety of an unperformed large refactor.

**Remaining work:** address duplication tied to a real bug or measured cost:
retained-history calculations, saved-progress updates, reset logic and large
lifecycle functions. Make allowed states and transitions clear. Do not build a
generic framework just to shorten files, merge retiring adapters with native
code, or create a developer-helper project.

**Likely complexity:** implementation **Medium** for one bounded extraction,
**High** for a recovery state change; testing **High** to preserve cancellation,
commit and cleanup behavior.

**Evidence:** [controller][controller], [reconciliation][reconcile],
[queues][qos], [delivery][delivery]. Earlier tracking: GC-10, GM-05/06,
PC-02, DP-05, QX-07, RT-14, PX-14, [#582][i582], [#584][i584].

<a id="cw-35"></a>
<a id="cw-35-tighten-rust-api-and-ownership-invariants"></a>

### CW-35: Track concrete ownership and overflow defects, not general code-style work

**Priority: P2. Implementation: safeguards exist; remaining native defects need an explicit case.**

**What works:** checked counters, actual storage ownership checks, replacement
guards and truthful failure state already exist in several changed paths.

**Testing:** counter exhaustion, stale owners and rejected retirement have
focused tests. This heading is not evidence that all similar code is defective.

**Remaining work:** identify and reproduce any retained native path that
violates these rules. Keep actual lock order, arithmetic and safe retirement
fixes in scope. General API annotations, cosmetic cleanup and author-convenience
wrappers are retired objectives. Close or narrow unsupported review suspicions
rather than leaving an unlimited “improve safety” task.

**Likely complexity:** investigation **Low to Medium**; implementation
**Medium** for a specific defect, otherwise not estimable; testing **Medium to
High** for malformed input, concurrency and resource reuse.

**Evidence:** [lifecycle][lifecycle], [retirement][retirement], [shared storage][shared-qos],
[inspection][runtime-inspection]. Earlier tracking: GC-13, GM-11/14/16,
PC-09/10, PX-14, RT-10/12/15, LI-13.

## Evidence still needed

<a id="assurance-and-qualification"></a>
<a id="recovery-and-operational-qualification"></a>
<a id="cw-36"></a>
<a id="cw-36-make-tests-prove-behavior-not-just-execute-named-cases"></a>

### CW-36: Make the acceptance tests prove the promised result

**Priority: P1. Implementation: substantial test infrastructure exists; overall acceptance evidence is incomplete.**

**What works:** protected test names, independent result models, persistent
recovery checks and exact-output workloads exist. These are not all missing.

**Testing:** selected models and test-runner checks have recorded results.
Passing test names or high line coverage alone do not prove the assertion is
strong enough.

**Remaining work:** connect every required behavior to an actual native setup,
an exact expected outcome and a recorded run. Deliberately break selected
behavior to check that its test fails. Extend generated data, malformed-input
and narrowly bounded concurrency checks where they protect a stated requirement.
An unrelated command must actually finish; a stop hook being called is not
proof its resource was released.

**Likely complexity:** runtime implementation **None unless a test finds a
bug**; test infrastructure **Medium**, testing/review **High** across the
required behavior set.

**Evidence:** [protected cases][contracts], [test ledger][requirements],
[runner tooling][parity-tooling]. Earlier tracking: TS-01/03/04/10, PC-10.

<a id="cw-37"></a>
<a id="cw-37-make-timing-completion-and-test-environments-deterministic"></a>

### CW-37: Stop using timing guesses as proof that asynchronous work finished

**Priority: P2. Implementation: controlled tests exist; broader deterministic testing is incomplete.**

**What works:** many newer tests use explicit gates and saved processing
positions. Selected scheduling tests control clock changes.

**Testing:** those cases have scoped evidence. The separately inspected query
test-clock branch is not included in the current Core commit. It must not be
described as an available integrated feature.

**Remaining work:** use actual query completion, due-work completion and
destination completion as separate checks. Add only the native controls needed
for required acceptance. Isolate global test state and retain single-thread
coverage. Reproduce relevant shared evaluator range problems rather than
merely freezing a panic as expected behavior. Do not create a general public
testing-helper project.

**Likely complexity:** implementation **Medium** for internal test controls;
testing **High** for reliable clock, completion and concurrency scenarios.

**Evidence:** [tests][lib-tests], [scheduling][query-scheduling],
[separate clock proposal][timing-control]. Earlier tracking: TS-02/05/06/08/09,
[#245][i245], [#461][i461], [#482][i482], [#880][i880], [#483][i483].
PRs [#246][p246]/[#887][p887] are historical leads.

<a id="cw-38"></a>
<a id="finish-qualifying-the-source-time-merger"></a>
<a id="cw-38-protect-merger-contracts-and-prove-real-process-crash-recovery"></a>

### CW-38: Verify the saved source-time merger survives actual process death

**Priority: P1. Implementation: present; selected real-crash tests completed.**

**What works:** the merger can save buffered input, pending output and retry
receipts. Protected tests name the important native merger/middleware behavior.

**Testing:** a normal DrasiLib child was killed at eight selected persistence/
handoff points. macOS/Linux RocksDB reopen tests checked exact buffered/output
recovery and duplicate prevention. Do not describe real process-crash testing
as not started.

**Remaining work:** broader held-late-input, provider and prolonged-operation
combinations are not covered by that selected fixture. Name those extra cases
before claiming broader recovery.

**Likely complexity:** implementation **None** for the completed selected
fixture, **Medium** if another provider exposes a gap; testing **High** for the
additional crash matrix.

**Evidence:** [merger tests][merge-tests], [persistent/crash tests][merge-tests-recovery],
[protected cases][contracts]. Earlier tracking: TS-07 and merger follow-ups.
Storage-window performance is CW-30/CW-40.

<a id="cw-39"></a>
<a id="cw-39-extend-combined-recovery-and-lifecycle-scenarios"></a>

### CW-39: Test failures in combination, not only one at a time

**Priority: P1. Implementation: underlying recovery mechanisms present; combined evidence incomplete.**

**What works:** transactions, saved delivery, managed configuration, reset
markers and selected real crash/restart scenarios exist.

**Testing:** the ledger records substantial coverage, including its completed
M01-M07 Core/redb configuration scenarios. Other rows remain partial. A passing
single-fault test does not establish the following combinations.

**Remaining work:** combine failed/delayed initial snapshots with live input;
lost history with failed reads/writes; clock changes with persistent query
restart; cancelled replacement with fan-out; disconnected consumers with
damaged history; and lost destination replies with failed completion writes.
Extend actual provider/connector coverage. Specifically bind evidence for the
old configuration-hash read-error/reset fix instead of inferring its exact
coverage from a broad successful suite.

**Likely complexity:** runtime implementation **None assumed**; test setup
**Medium to High**, testing **High** for exact rows, positions, effects and
released resources. Any discovered bug gets its own implementation assessment.

**Evidence:** [requirement details][requirements], [persistent query tests][transaction-query-tests],
[configuration tests][managed-tests]. Earlier tracking: CG-01/20-24,
[#393][i393], [#394][i394], [#743][i743], [#818][i818].
PR [#745][p745] is a scenario lead.

<a id="cw-40"></a>
<a id="cw-40-measure-sustained-behavior-and-current-performance"></a>

### CW-40: Measure sustained performance, resource growth and large graphs

**Priority: P1 for required execution/migration evidence; P2 for broader production characterization. Implementation: measurement tools and selected results present.**

**What works:** workload runners, exact result checks, bounded replacement
soaks and CPU measurements exist. CW-50's selected CPU-scaling target passed.

**Testing:** recorded local soaks checked file/socket/task release and flat
live Rust memory for their bounded workload. They do not prove many-day
persistent operation, total process memory or production latency limits.

**Remaining work:** measure growing graph sizes, busy/slow branches, shared
storage, large payloads, outages and long-running native pipelines. Record
latency, CPU, allocations, memory, disk and actual binary/provider/plugin
versions. Set workload-specific acceptance limits before claiming success.
Measure CW-31 separately; extra worker threads alone prove neither scaling
nor control responsiveness.

**Likely complexity:** implementation **Low to Medium** for measurement
fixtures; testing **High**, with substantial run time and environment control.

**Evidence:** [workload runner][workload-runner], [resource soak][resource-soak],
[recorded execution reference][execution-plan]. Earlier tracking:
CG-21/22/27, G06/Q08/T07, [#681][i681].

<a id="cw-41"></a>
<a id="cw-41-qualify-independently-built-plugin-lifetimes-and-compatibility"></a>

### CW-41: Check real plugin binaries across failure, replacement and version changes

**Priority: P1. Implementation: native SDK/Host present; broader compatibility/lifetime evidence incomplete.**

**What works:** separately built native libraries exercise processing,
transactions, input receipts, initial loading, consumer completion and panic
handling. Invalid capabilities and revoked storage can be rejected.

**Testing:** actual-binary cases and selected restart/cancellation tests passed
in recorded runs. They are more than compile-only checks, but not every
cross-version or late-callback schedule.

**Remaining work:** expand malformed metadata/buffer, version mismatch,
cancel/restart, reused handle and delayed callback tests. Measure per-instance
allocation cleanup under repeated multi-instance use. Libraries intentionally
loaded for process lifetime are not by themselves a leak; hot unloading is not
the selected requirement.

**Likely complexity:** implementation **Medium** if tests reveal a lifetime
gap, **High** if compatibility changes; testing **High** with independently
built binaries and allocation/lifetime accounting.

**Evidence:** [Host tests][host-tests], [native SDK][native-sdk], [test ledger][requirements].
Earlier tracking: CG-05/06/07/25, A01-A05. Ordinary SDK expansion is excluded.

## Hosting, connectors, Server and examples

<a id="server-connectors-and-solution-delivery"></a>
<a id="integration-and-productization"></a>
<a id="cw-56"></a>
<a id="cw-56-isolated-drasilib-instance-hosting-and-native-inter-instance-links"></a>

### CW-56: Run normal DrasiLib instances in child processes and connect them safely

**Priority: P1. Implementation: selected macOS/Linux host and link scope implemented.**

**What works:** parent, child and grandchild use the same DrasiLib engine.
A separate supervisor owns local child processes; native sources/sinks carry
data between instances. Definitions and last-known failure state survive a
child exit in the parent. A child-only plugin need not be loaded in the parent.

**Testing:** recorded real-process tests cover hierarchy, abrupt parent death,
child repair, stale control, cancelled host cleanup, data drain and process
reaping on macOS/Linux. Bridge tests cover retries, full queues and saved state.
A prepared macOS/Linux-VM mutual-TLS run checks lost/delayed replies, rejected
versions/authentication, exact outputs and persistent reopen.

**Remaining work:** broader physical-machine/network and production/provider
evidence is not established. Windows and hard operating-system resource
containment are not implemented/qualified by this work and were not promises
of the selected Unix scope. Remote network loss is not proof a process died.
Transactions remain local to one instance.

**Likely complexity:** implementation **None** for the selected completed
host/link mechanism; a separately selected Windows/resource-isolation extension
would be **High to Very high**. Broader production testing is **High**.

**Evidence:** [host documentation/tests][host-readme], [bridge documentation][bridge-readme],
[protected process cases][contracts], [selected scope][execution-plan].
This is not a remote per-component executor or automatic remote provisioning.

<a id="cw-42"></a>
<a id="cw-42-expose-ordinary-factory-based-creation"></a>

### CW-42: Add another factory API for the older component interface

**Priority: none under the current direction. Implementation: retired, not implemented as a new API.**

**What works:** native declarations and factories already support graph-owned
construction and retained failures.

**Testing:** native construction tests exist. They do not make a new ordinary
factory API necessary.

**Remaining work:** none in this retired scope. Use native construction for
the migration rather than adding another older-interface convenience API.

**Likely complexity:** implementation/testing **Not applicable**.

**Evidence:** [native addition][addition], [native definitions][topology].
Earlier tracking: CG-08.

<a id="cw-43"></a>
<a id="cw-43-complete-ordinary-to-native-wiring-and-reconstruction-recipes"></a>

### CW-43: Make complete native pipelines rebuildable, including their query-result connections

**Priority: P1 for native migration. Implementation: partly implemented.**

**What works:** native factories, resource descriptions, saved definitions and
selected Server reconstruction already exist. Export can distinguish a
rebuildable definition from an application-supplied object it cannot recreate.

**Testing:** selected reconstruction and native query-read tests exist. They
do not establish every native deployment recipe or live-result outlet.

**Remaining work:** complete the supported native recipes, dependencies and
versions, including required merger/provider combinations. Standard query
assembly must preserve the actual catalogue and connected output needed by
snapshot, saved-result and live-subscription workflows through export, restart
and replacement. A deliberate processing-only graph may omit that surface;
a migrated user workflow must not lose it silently.

**Likely complexity:** implementation **High** across assembly, recipes and
Server; testing **High** for recreation and exact result/subscription behavior.

**Evidence:** [assembly][pipeline], [catalogue][catalog], [management guide][managed-guide],
[Server configuration][server-computation]. Earlier tracking: CG-10/15.
The proposed ordinary-to-native bridge is retired, not a remaining deliverable.

<a id="cw-44"></a>
<a id="cw-44-select-native-plugin-and-connector-breadth-explicitly"></a>

### CW-44: Preserve required connector features when replacing older plugins

**Priority: P1 before each affected native cutover. Implementation: native framework and some connectors present; migration incomplete.**

**What works:** native HTTP/gRPC sources/sinks, SSE and reference components
exist. Native admission, progress, initial-loading and completion services also
exist. Built-in native queries do not require a new dynamically loaded evaluator.

**Testing:** selected real native network/service cases passed. That is not
feature parity with all existing production connectors.

**Remaining work:** migrate the required component/provider set and packaging.
Concrete gaps include payload/header/routing templates, fixed/adaptive batching,
and supported snapshot/recovery replacement messages. Native gRPC currently
supports only `batchSize=1`; applicable native network options reject unsupported
features. That is missing implementation, not just missing tests. Preserve
required transport/security and destination-completion behavior or obtain an
explicitly accepted change before cutover.

**Likely complexity:** implementation **Very high** for the full connector set,
to be split by required connector/feature; testing **Very high** across real
products, protocols, failures and recovery.

**Evidence:** [native network limits][network], [SDK services][native-sdk],
[Server recipes][server-computation]. Earlier tracking: CG-02/03/07/11/12,
[#44][i44]; related acceptance reports [#238][i238]/[#301][i301]/[#426][i426].

<a id="cw-45"></a>
<a id="cw-45-complete-supported-server-operations-and-native-ui-controls"></a>

### CW-45: Finish moving supported Server and UI workflows to native components

**Priority: P1 for the full native-only target. Implementation: partly implemented.**

**What works:** Server exposes native definitions, resources, inspection,
configuration acceptance/receipts and selected result/lifecycle operations.
Everything already executes through ComputationGraph.

**Testing:** many Server unit/integration cases and selected live examples have
recorded passes. This does not establish a fully native Server/UI deployment.

**Remaining work:** remove dependence on older component/plugin interfaces for
the supported workflows. Preserve creation, loading, configuration, lifecycle,
results, snapshots, subscriptions, management and UI editing. Show accepted,
starting, ready and failed states correctly. Define saved-configuration/data
cutover. Do not retire a supported option or endpoint before its native
replacement works or its removal is explicitly accepted.

**Likely complexity:** implementation **Very high** across APIs, plugins,
configuration and UI; testing **Very high** for existing client/workflow behavior.

**Evidence:** [routes][server-routes], [native UI][native-ui], [Server tests][server-tests].
Earlier tracking: CG-04/09/14. This is not replacing a second execution engine.

<a id="cw-46"></a>
<a id="cw-46-qualify-server-contracts-and-truthful-recovery-test-verdicts"></a>

### CW-46: Prove complete Server workflows and stop treating incomplete recovery captures as passes

**Priority: P1. Implementation: Server/tests/comparator present; full workflow and completion evidence incomplete.**

**What works:** native Server operations, selected restart/retirement tests,
producer identity capture and a recovery comparator exist. The comparator can
report failed or inconclusive results rather than calling everything a pass.

**Testing:** the final selected record includes 448 Server unit/selected
integration executions. The broader ledger still lacks complete Server workflow
bindings. Existing Building Comfort snapshots do not prove that every expected
query result and destination operation finished.

**Remaining work:** cover delayed/failed startup, accepted configuration,
REST/SSE reconnects, stale readers and required snapshot/result workflows.
Establish an actual final processing/delivery boundary in baseline and recovered
runs, then capture compatible expected results. Record exact native binaries and
plugins. Scheduled `main`/nightly runs are not automatically evidence for this
branch. Never manufacture a completion flag from quiet output or receiver count.

**Likely complexity:** implementation **High** for remaining test orchestration
and terminal evidence; testing **Very high** across real Server workflows.

**Evidence:** [Server tests][server-tests], [recovery comparator][recovery-comparison],
[documented unfinished completion checks][scheduled-recovery].
Earlier tracking: CG-26, H01-H05,
[#791][i791]/[#881][i881]/[#929][i929]/[#935][i935];
PR [#948][p948] is a scenario lead.

<a id="cw-47"></a>
<a id="cw-47-finish-templates-stock-server-examples-and-tutorial-acceptance"></a>

### CW-47: Make existing examples and templates work on the native Server with exact checks

**Priority: P1 for existing required solutions; P3 for additional examples. Implementation: partly implemented.**

**What works:** native example work, selected browser/live checks and updated
developer/maintainer documentation exist. The OTel tutorial correlates application
telemetry; it does not implement Drasi-wide monitoring.

**Testing:** selected Move-a-Wall and GPU functional/restart/browser checks were
recorded. The GPU lab's full acceptance mode still rejects its unsupported
policy-context/transaction-completion requirement. Those selected passes are
not full GPU acceptance or proof of a stock-Server equivalent.

**Remaining work:** finish required native templates and stock-Server packaging,
readiness/reset behavior and exact result/effect checks. Preserve existing
Getting Started assertions. Keep guides consistent with actual capabilities;
describing a missing feature does not implement it. Additional tutorial breadth
is separate from preserving existing supported examples.

**Likely complexity:** implementation **High**, depending on CW-44/CW-45;
testing **High** across installation, browser use, reset and recovery.

**Evidence:** [solution documentation][server-readme], [stock GPU example][gpu-server],
[tutorials][tutorials], [OTel tutorial][otel-tutorial]. Earlier tracking: CG-16/17/28.

## Optional features and decisions

<a id="optional-capabilities-and-policy-decisions"></a>
<a id="optional-capabilities-and-explicit-policy-decisions"></a>

These are not automatic additions to the previous execution-and-isolation
package. Select a requirement before implementing a new capability.

<a id="cw-48"></a>
<a id="cw-48-stronger-event-time-guarantees"></a>

### CW-48: Know when all earlier source-dated input has arrived, or correct late results

**Priority: P3. Implementation: proposed stronger behavior not selected or implemented.**

**What works:** the merger can wait within configured lateness/time/size bounds.
It cannot know that no unseen earlier input will ever arrive.

**Testing:** bounded merger behavior is tested. That is not a test of
source-declared completeness or automatic corrections to already published results.

**Remaining work if selected:** define how sources declare earlier input
complete, how idle sources behave and how late corrections/deletions reach
downstream consumers. Define equal-time ordering and replay. Do not use arrival
time as a substitute for source time.

**Likely complexity:** implementation **High to Very high** across source and
result semantics; testing **High** for late input, idle sources and reconstruction.

**Evidence:** [current merger][time-merge]. Earlier tracking: DP-10 and event-time proposals.

<a id="cw-49"></a>
<a id="cw-49-opt-in-poison-input-quarantine"></a>

### CW-49: Optionally set aside an input that always fails

**Priority: P3. Implementation: optional skip/set-aside policy not selected or implemented.**

**What works:** failed input is retained rather than silently acknowledged.
Restart can therefore encounter the same deterministic error again.

**Testing:** failure and retained-work tests exist. They do not establish a
durable operator-controlled skip policy.

**Remaining work if selected:** define fail versus set-aside behavior, a saved
diagnostic identity, explicit operator actions and atomic progress/output changes.
Never silently skip other input. Useful failure reporting remains required
under CW-22 regardless of this decision.

**Likely complexity:** implementation **High** because processing progress
changes; testing **High** across restart, retries and unrelated input.

**Evidence:** [query recovery][query-recovery], [error reporting][errors].
Earlier tracking: QX-05.

<a id="cw-51"></a>
<a id="cw-51-rateburst-limits-and-larger-acknowledgement-windows"></a>

### CW-51: Optionally limit events per second or allow more unacknowledged deliveries

**Priority: P3. Implementation: proposed features not selected or implemented.**

**What works:** queue capacity limits buffered work. Acknowledged subscriber
delivery currently has its defined limited outstanding work.

**Testing:** current capacity and acknowledgement behavior is tested. Queue
size is not a tested events-per-second limit.

**Remaining work if selected:** choose whether rate excess waits, rejects or
drops, and define burst behavior. Expand simultaneous deliveries only with a
safe rule for advancing the completed prefix. These are protocol changes, not
one performance switch.

**Likely complexity:** implementation **Medium** for a bounded rate limiter,
**High** for expanded delivery windows; testing **High** for fairness,
cancellation, replay and bounded retry records.

**Evidence:** [queues][qos], [delivery][delivery]. Earlier tracking:
rate/window proposals and DP-11's replay-window question.

<a id="cw-52"></a>
<a id="cw-52-wider-atomic-visibility-and-architectural-extensions"></a>

### CW-52: Select any wider transaction or distributed-execution promise separately

**Priority: P3 unless a specific deployment requires it. Implementation: larger guarantees not selected or implemented.**

**What works:** complete source transactions can be processed atomically by one
supported query; selected local processing and output storage can share a commit.
Independent instances can exchange data.

**Testing:** those local boundaries have tests. They do not make several queries
or remote effects appear atomically to all observers.

**Remaining work if selected:** define the participants and failure/recovery
rules before promising wider atomic visibility. Graph feedback cycles,
distributed scheduling, automatic state migration, undoing an entire live
deployment and unloading active libraries are separate designs. Child hosting
does not implement them. Exactly-once remote effects require cooperation from
the destination.

**Likely complexity:** implementation/testing **Very high**; each selected
capability needs a separate plan rather than one combined task.

**Evidence:** [local transactions][transaction-guide], [instance links][bridge-readme].
Earlier tracking: CG-18's wider boundary and architectural proposals.

<a id="cw-53"></a>
<a id="cw-53-shared-query-language-and-value-capabilities"></a>

### CW-53: Select additional query-language functions and data types explicitly

**Priority: P2/P3 by use case. Implementation: mixed existing functionality and unselected requests.**

**What works:** both language registries already provide many functions.
For example, Cypher collection exists; this is not a claim that every requested
operation is absent everywhere.

**Testing:** existing language/function tests do not establish each requested
language/type combination. Reported unusual before-value behavior still needs
a reduced current example.

**Remaining work if selected:** specify the requested logarithm/exponential,
collection/order, string min/max, correlated existence, matching, row expansion,
UNION, hashing, escape and geographic capabilities individually. Define how
updates and restart affect results. General ORDER BY/LIMIT/SKIP remain
intentionally unsupported. Source fan-in is not query UNION.

**Likely complexity:** implementation **Low to Medium** for isolated scalar
functions, **High to Very high** for new query operators/types; testing
**Medium to High**, including saved-state compatibility where relevant.

**Evidence:** [Cypher functions][cypher-functions], [GQL functions][gql-functions],
[Cypher parser][cypher-parser], [GQL parser][gql-parser].
Earlier reports: [#211][i211], [#212][i212], [#217][i217], [#357][i357],
[#516][i516], [#598][i598], [#800][i800], [#801][i801], [#802][i802],
[#803][i803], [#804][i804], [#892][i892], [#895][i895], [#916][i916],
[#917][i917];
PR [#218][p218] is a scalar-function lead.

<a id="cw-54"></a>
<a id="cw-54-provider-atomic-compare-and-swap"></a>

### CW-54: Optionally update a stored value only if it still has the expected value

**Priority: P3. Implementation: general provider operation not selected or implemented.**

**What works:** managed configuration has revision checks. That does not give
every component a general atomic compare-and-update storage operation.

**Testing:** configuration revision tests are not tests of this proposed
provider/plugin capability.

**Remaining work if selected:** define value/version comparison, durability and
uncertain outcomes in real providers and the native plugin interface. A separate
read followed by an unprotected write is not equivalent.

**Likely complexity:** implementation **High** across providers and interfaces;
testing **High** with simultaneous writers and failed/uncertain commits.

**Evidence:** [storage interfaces][storage-interfaces].
Earlier tracking: [#918][i918].

<a id="cw-55"></a>
<a id="cw-55-choose-additional-configuration-retention-and-key-policies"></a>

### CW-55: Decide whether configuration cleanup and key handling need more automation

**Priority: P3. Implementation: explicit operations done; extra automatic policies not selected.**

**What works:** redb supports bounded retry-receipt batches, deleting a named
configuration snapshot only at the expected revision, and atomic rotation of
live encrypted records to a new key. Default indefinite receipts are deliberate.

**Testing:** the explicit operations have storage/management tests. They do not
prove automatic age-based cleanup, external key-service integration or secure
erasure of old backups.

**Remaining work if selected:** choose bounded in-memory receipt rules,
automatic snapshot retention, external key providers or erasure/backup policies.
Preserve rejection of expired retries. Configuration-snapshot deletion must not
rewind processing; key rotation does not re-encrypt old backups.

**Likely complexity:** implementation **Medium** for bounded retention,
**High** for key-provider/backup guarantees; testing **High** for expiry,
uncertain writes and old/new key recovery.

**Evidence:** [management guide][managed-guide], [redb configuration storage][configuration-store].
Earlier tracking: CG-19's optional policies, LI-12.

<a id="tracking-crosswalk"></a>

## Where earlier tracking numbers went

These are lookup references, not additional work items. Mapping an old finding
to a CW entry does not make a retired-interface repair active again.

### Original CG backlog

- CG-01: recovery checks exist; broader evidence is CW-39.
- CG-02/03: native input/consumer services exist; adoption is CW-44 and evidence CW-39/CW-46.
- CG-04: configuration runtime exists; remaining Server migration is CW-45/CW-46.
- CG-05/06/07: completed mailbox/listing/ownership/bootstrap fixes are retained; broader native binary checks are CW-41/CW-44.
- CG-08: retired ordinary factory API, CW-42.
- CG-09/14: Server/UI migration, CW-45.
- CG-10: ordinary/native bridge retired; native assembly remains CW-43.
- CG-11/12: connector breadth, CW-44, not rebuilding completed services.
- CG-13: selected execution done, CW-50; wider evidence CW-40/CW-41.
- CG-15: native reconstruction descriptions, CW-43.
- CG-16/17/28: required examples/templates/docs, CW-47.
- CG-18: local complete-source transactions exist; wider promises CW-52.
- CG-19: explicit configuration housekeeping exists; optional policies CW-55.
- CG-20: named configuration cases covered; broader providers CW-39.
- CG-21/22: CW-29/CW-36/CW-39/CW-40; not a new implementation of recovery.
- CG-23/24: CW-37/CW-39/CW-46.
- CG-25: actual plugin compatibility/lifetime evidence, CW-41.
- CG-26: Server/recovery verdict evidence, CW-46.
- CG-27: sustained measurements, CW-40.

<a id="complete-runtime-review-crosswalk"></a>
<a id="pre-review-topics-and-observability-packages"></a>

### Runtime review identifiers

Some old review numbers were reused. **GM-09 original** is resource naming;
**GM-09 Phase 0** is partial commit. **GM-10 original** is lost error types;
**GM-10 Phase 0** is incomplete resumption. **PX-13 original** is float
representation; **PX-13 Phase 0** is lost snapshot decode errors.

- Controller: GC-01/04/05 -> CW-04; GC-02/09 -> CW-05/CW-06;
  GC-03/06/15 -> CW-03; GC-07/11 -> CW-06; GC-08/14 -> CW-09/CW-31;
  GC-10 -> CW-34; GC-12 -> CW-33; GC-13 -> CW-35.
- Reconciliation: GM-01/07 and Phase 0 GM-09/10 -> completed CW-01/CW-02;
  GM-02 -> CW-08; GM-03/15 and original GM-09 -> CW-09;
  GM-04 -> CW-33; GM-05/06 -> CW-34; GM-08 -> CW-06;
  original GM-10 -> CW-22; GM-11 -> CW-03/CW-35; GM-12 -> CW-31;
  GM-13 -> CW-23/CW-31; GM-14/16 -> CW-35.
- Pipes: PC-01 fixed; PC-02 -> CW-34; PC-03 -> CW-30;
  PC-04/06/07 -> CW-22; PC-05 -> CW-04; PC-08 -> CW-32;
  PC-09 -> CW-21/CW-35; PC-10 -> CW-35/CW-36.
- Delivery/merger: DP-01 -> CW-05; DP-02 -> CW-29;
  DP-03/04/07 -> CW-21/CW-30/CW-31 as applicable; DP-05 -> CW-34;
  DP-06/09/11 -> CW-21; DP-08 -> CW-04/CW-32;
  DP-10 -> CW-48; DP-11 also informs CW-51.
- Query execution: QX-01/02 fixed; QX-03 -> CW-18; QX-04 -> CW-20;
  QX-05 -> CW-22/CW-49; QX-06 -> CW-06; QX-07 -> CW-34;
  QX-08 -> CW-31; QX-09/12 -> CW-29; QX-10 -> CW-30; QX-11 -> CW-03.
- Plugins/encoding: PX-01 live failure fixed, missing counter CW-26;
  PX-02/07/09 -> CW-18; PX-03 and Phase 0 PX-13 -> retired CW-15;
  PX-04 ordinary investigation excluded; PX-05/06 -> CW-19;
  PX-08/11 and original PX-13 -> CW-32; PX-10 -> CW-22; PX-12 fixed;
  PX-14 -> CW-34/CW-35.
- Runtime APIs: RT-01/02 -> CW-07; RT-03 through RT-06 -> narrowed CW-10;
  RT-07 -> CW-08; RT-08/13 -> CW-31; RT-09 -> CW-03;
  RT-10/12/15 -> CW-35; RT-11 -> CW-22; RT-14 -> CW-08/CW-34.
- Library: LI-01 fixed; LI-02 -> CW-10/CW-22; LI-03/04/10 -> CW-08;
  LI-05 -> CW-07 investigation; LI-06 -> CW-17; LI-07/08 -> CW-23;
  LI-09 -> CW-10; LI-11 -> CW-03/CW-10; LI-12 -> CW-55;
  LI-13 -> CW-10/CW-35.
- Tests: TS-01/03/04/10 -> CW-36; TS-02/05/06/08/09 -> CW-37;
  TS-07 -> CW-38.
- Monitoring proposal: A -> CW-24; B/E -> CW-25; C/H -> CW-26;
  D -> native CW-25/CW-26/CW-41; F/G -> CW-27 and Server evidence CW-46.

## Completion and maintenance rules

Close the stated requirement, not a broad heading. Record what changed, the
exact tested behavior and any remaining gap. A tested subset does not make an
unimplemented requirement complete; lack of universal proof does not make a
working, tested mechanism “not started.”

For bugs, demonstrate the current failure and verify the actual result or saved
state after the fix. For testing work, name the missing scenario and preserve
the implementation already present. For an optional feature, obtain a selected
requirement first. For retired work, say “not pursued,” not “fixed.”

Keep original approved requirements distinct from larger future ambitions.
Do not move an unfinished approved requirement into a follow-up merely because
other parts passed. Do not interpret unsupported Windows/security/remote
provisioning behavior as a promise of the selected Unix host.

A complete test run, test count, quiet receiver, successful startup or matching
snapshot does not alone prove full processing/delivery completion. Record
binary/provider/plugin versions and the actual end-of-work evidence. Never
weaken a verdict or silently change a stored format to obtain a pass.

Keep these CW identifiers stable. Update this document when code or evidence
changes; historical plans and session records must not become competing claims
of current completion.

[execution-plan]: computation-graph-execution-isolation-plan.md
[maintainer-guide]: computation-graph-maintainers-guide.md
[old-backlog]: computation-graph-backlog.md
[reliability-plan]: computation-graph-reliability-plan.md
[requirements]: ../tests/runtime_parity/requirements.tsv
[contracts]: ../tests/runtime_parity/computation-contracts.tsv
[parity-tooling]: ../tests/runtime_parity/
[lib-tests]: ../tests/
[controller]: ../src/computation/v1/graph/controller.rs
[execution]: ../src/computation/v1/graph/execution.rs
[resource-tasks]: ../src/computation/v1/graph/resource_tasks.rs
[reconcile]: ../src/computation/v1/graph/reconcile.rs
[commit-tests]: ../src/computation/v1/graph/reconcile_commit_tests.rs
[addition]: ../src/computation/v1/graph/addition.rs
[graph]: ../src/computation/v1/graph.rs
[instance]: ../src/computation/instance.rs
[runtime-inspection]: ../src/computation/runtime/inspection.rs
[management-runtime]: ../src/management/runtime.rs
[managed-guide]: managed-configuration.md
[topology]: ../src/computation/v1/graph/topology.rs
[lifecycle]: ../src/computation/v1/lifecycle.rs
[control]: ../src/computation/v1/control.rs
[registry]: ../src/computation/v1/graph/registry.rs
[inspection]: ../src/computation/v1/inspection.rs
[retirement]: ../src/computation/v1/graph/retirement.rs
[query-recovery]: ../src/computation/v1/query_recovery.rs
[query-delivery]: ../src/computation/v1/query_delivery.rs
[query-scheduling]: ../src/computation/v1/query_scheduling.rs
[query-state]: ../src/computation/internal/query_state.rs
[query-configuration]: ../src/computation/v1/query_configuration.rs
[query-recovery-tests]: ../tests/computation_query_recovery.rs
[task-tests]: ../tests/computation_task_execution.rs
[instance-tests]: ../tests/computation_instance.rs
[graceful-tests]: ../tests/computation_graceful_stop.rs
[failure-tests]: ../tests/computation_failure_policy.rs
[reconcile-tests]: ../tests/computation_reconciliation.rs
[mutation-tests]: ../../core/src/query/tests/mutation_integrity.rs
[persistent-mutation-tests]: ../tests/computation_mutation_integrity.rs
[source-recovery-tests]: ../../lib-integration-tests/tests/computation_source_recovery.rs
[api-tests]: ../tests/component_api_parity.rs
[budget-tests]: ../tests/computation_journal_budgets.rs
[transaction-query-tests]: ../../lib-integration-tests/tests/computation_transaction_query.rs
[managed-tests]: ../../lib-integration-tests/tests/computation_management.rs
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
[pipe-metrics]: ../src/computation/v1/pipe_metrics.rs
[legacy-source]: ../src/computation/v1/legacy_source.rs
[workers]: ../src/context/workers.rs
[component-logs]: ../src/managers/component_log.rs
[tracing-layer]: ../src/managers/tracing_layer.rs
[metrics]: ../src/metrics/
[errors]: ../src/error.rs
[change-receiver]: ../../components/host-sdk/src/proxies/change_receiver.rs
[old-proxies]: ../../components/host-sdk/src/proxies/
[old-sdk]: ../../components/plugin-sdk/src/ffi/
[native-sdk]: ../../components/computation-plugin-sdk/README.md
[host-tests]: ../../components/host-sdk/tests/
[native-admission-tests]: ../../components/host-sdk/tests/native_network/durable.rs
[network]: ../../components/computation-plugins/NETWORK.md
[host-readme]: ../../components/instance-host/README.md
[bridge-readme]: ../../components/computation-plugins/common/bridge/README.md
[bridge-codec]: ../../components/computation-plugins/common/bridge/src/codec.rs
[bridge-codec-tests]: ../../components/computation-plugins/common/bridge/tests/bridge/codec.rs
[otel-source]: ../../components/sources/otel/
[redb-wal]: ../../components/wals/redb/src/provider.rs
[rocksdb]: ../../components/indexes/rocksdb/
[configuration-store]: ../../components/state_stores/redb/src/configuration/
[index-factory]: ../src/indexes/factory.rs
[storage-interfaces]: ../../core/src/interface/
[workload-runner]: ../tests/measure-core-workloads.py
[resource-soak]: ../examples/core_resource_soak.rs
[transaction-guide]: computation-graph-transactions.md
[cypher-functions]: ../../functions-cypher/src/lib.rs
[gql-functions]: ../../functions-gql/src/lib.rs
[cypher-parser]: ../../query-cypher/src/lib.rs
[gql-parser]: ../../query-gql/src/lib.rs
[observability-design]: https://github.com/drasi-project/design-documents/tree/77b155319fd8160496ad059f6fdcb1e6bc93e43d/drasi-lib/tracing-logging
[timing-control]: https://github.com/drasi-project/drasi-core/blob/b8d1f38a63d53a134b069546d349800d18b0571b/lib/docs/query-test-control.md
[server-routes]: https://github.com/drasi-project/drasi-server/blob/f15706d0d65c5c829c0a653c0d977f4cad617d6a/src/api/v1/routes.rs
[server-computation]: https://github.com/drasi-project/drasi-server/blob/f15706d0d65c5c829c0a653c0d977f4cad617d6a/src/computation.rs
[server-readme]: https://github.com/drasi-project/drasi-server/blob/f15706d0d65c5c829c0a653c0d977f4cad617d6a/README.md
[server-tests]: https://github.com/drasi-project/drasi-server/tree/f15706d0d65c5c829c0a653c0d977f4cad617d6a/tests
[native-ui]: https://github.com/drasi-project/drasi-server/blob/f15706d0d65c5c829c0a653c0d977f4cad617d6a/ui/src/components/inspector/ComputationDetails.tsx
[gpu-server]: https://github.com/drasi-project/drasi-server/blob/f15706d0d65c5c829c0a653c0d977f4cad617d6a/examples/gpu-cluster-lab/server/README.md
[recovery-comparison]: https://github.com/drasi-project/test-infra/blob/55ff7eeba41a4222c892c75af526a64d9c16b801/e2e-test-framework/examples/recovery_comparison/README.md
[scheduled-recovery]: https://github.com/drasi-project/test-infra/blob/55ff7eeba41a4222c892c75af526a64d9c16b801/e2e-test-framework/examples/building_comfort/dynamic/scheduled_recovery.md
[tutorials]: https://github.com/drasi-project/learning-drasi-server/blob/72770826a214f99b65aa85d092e9b919e08254d8/README.md
[otel-tutorial]: https://github.com/drasi-project/learning-drasi-server/blob/72770826a214f99b65aa85d092e9b919e08254d8/tutorials/otel-observability/README.md
[i244]: https://github.com/drasi-project/drasi-core/issues/244
[i507]: https://github.com/drasi-project/drasi-core/issues/507
[i805]: https://github.com/drasi-project/drasi-core/issues/805
[i384]: https://github.com/drasi-project/drasi-core/issues/384
[i498]: https://github.com/drasi-project/drasi-core/issues/498
[i806]: https://github.com/drasi-project/drasi-core/issues/806
[i411]: https://github.com/drasi-project/drasi-core/issues/411
[i795]: https://github.com/drasi-project/drasi-core/issues/795
[i796]: https://github.com/drasi-project/drasi-core/issues/796
[i797]: https://github.com/drasi-project/drasi-core/issues/797
[i798]: https://github.com/drasi-project/drasi-core/issues/798
[i807]: https://github.com/drasi-project/drasi-core/issues/807
[i808]: https://github.com/drasi-project/drasi-core/issues/808
[i812]: https://github.com/drasi-project/drasi-core/issues/812
[i721]: https://github.com/drasi-project/drasi-core/issues/721
[i722]: https://github.com/drasi-project/drasi-core/issues/722
[i723]: https://github.com/drasi-project/drasi-core/issues/723
[i919]: https://github.com/drasi-project/drasi-core/issues/919
[i891]: https://github.com/drasi-project/drasi-core/issues/891
[i646]: https://github.com/drasi-project/drasi-core/issues/646
[i648]: https://github.com/drasi-project/drasi-core/issues/648
[i649]: https://github.com/drasi-project/drasi-core/issues/649
[i774]: https://github.com/drasi-project/drasi-core/issues/774
[i815]: https://github.com/drasi-project/drasi-core/issues/815
[i896]: https://github.com/drasi-project/drasi-core/issues/896
[i632]: https://github.com/drasi-project/drasi-core/issues/632
[i355]: https://github.com/drasi-project/drasi-core/issues/355
[i633]: https://github.com/drasi-project/drasi-core/issues/633
[i668]: https://github.com/drasi-project/drasi-core/issues/668
[i782]: https://github.com/drasi-project/drasi-core/issues/782
[i309]: https://github.com/drasi-project/drasi-core/issues/309
[i635]: https://github.com/drasi-project/drasi-core/issues/635
[i276]: https://github.com/drasi-project/drasi-core/issues/276
[i582]: https://github.com/drasi-project/drasi-core/issues/582
[i584]: https://github.com/drasi-project/drasi-core/issues/584
[i245]: https://github.com/drasi-project/drasi-core/issues/245
[i461]: https://github.com/drasi-project/drasi-core/issues/461
[i482]: https://github.com/drasi-project/drasi-core/issues/482
[i880]: https://github.com/drasi-project/drasi-core/issues/880
[i483]: https://github.com/drasi-project/drasi-core/issues/483
[i393]: https://github.com/drasi-project/drasi-core/issues/393
[i394]: https://github.com/drasi-project/drasi-core/issues/394
[i743]: https://github.com/drasi-project/drasi-core/issues/743
[i818]: https://github.com/drasi-project/drasi-core/issues/818
[i681]: https://github.com/drasi-project/drasi-core/issues/681
[i44]: https://github.com/drasi-project/drasi-core/issues/44
[i211]: https://github.com/drasi-project/drasi-core/issues/211
[i212]: https://github.com/drasi-project/drasi-core/issues/212
[i217]: https://github.com/drasi-project/drasi-core/issues/217
[i238]: https://github.com/drasi-project/drasi-core/issues/238
[i301]: https://github.com/drasi-project/drasi-core/issues/301
[i357]: https://github.com/drasi-project/drasi-core/issues/357
[i426]: https://github.com/drasi-project/drasi-core/issues/426
[i516]: https://github.com/drasi-project/drasi-core/issues/516
[i598]: https://github.com/drasi-project/drasi-core/issues/598
[i791]: https://github.com/drasi-project/drasi-core/issues/791
[i800]: https://github.com/drasi-project/drasi-core/issues/800
[i801]: https://github.com/drasi-project/drasi-core/issues/801
[i802]: https://github.com/drasi-project/drasi-core/issues/802
[i803]: https://github.com/drasi-project/drasi-core/issues/803
[i804]: https://github.com/drasi-project/drasi-core/issues/804
[i881]: https://github.com/drasi-project/drasi-core/issues/881
[i892]: https://github.com/drasi-project/drasi-core/issues/892
[i895]: https://github.com/drasi-project/drasi-core/issues/895
[i916]: https://github.com/drasi-project/drasi-core/issues/916
[i917]: https://github.com/drasi-project/drasi-core/issues/917
[i918]: https://github.com/drasi-project/drasi-core/issues/918
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
[p915]: https://github.com/drasi-project/drasi-core/pull/915
[p948]: https://github.com/drasi-project/drasi-core/pull/948
