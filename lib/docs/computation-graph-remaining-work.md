# ComputationGraph remaining work

This is the single active list of known issues, investigations, qualification
gaps and suggested improvements for ComputationGraph across **drasi-core**,
**drasi-server**, **learning-drasi-server** and **test-infra**. It incorporates
the remaining work from the architecture review, CG backlog, reliability,
execution/isolation and breaking-release cleanup plans. Resolved defects and
incorrect findings are not outstanding work.

**How to read it:** An **issue** is an established implementation or build gap;
an **investigation** needs a reproduction before a fix is selected;
**qualification** needs stronger evidence for an implemented contract; an
**improvement** adds capability, efficiency or maintainability. High priority
means important for the affected deployment or release claim, not that every
item blocks every merge. Optional capabilities are identified explicitly.
Areas identify the owning repositories, not assigned individuals.
Resolved anchor stubs are labeled explicitly and are not open work.

The [executable requirement ledger](../tests/runtime_parity/requirements.tsv)
and [contract checks](../tests/runtime_parity/computation-contracts.tsv) record
coverage. They are evidence indexes, not competing work lists. A passing named
profile does not qualify every provider, failure schedule or deployment.
The recent reconciliation, controller, partial-start, snapshot-generation,
ownership, rollback and lineage repairs are implemented; their broader
qualification boundaries remain below. Intentional child-worker test ignores
are exercised by their parent tests, not counted as disabled functional failures.

Native conversion of every plugin, old-adapter retirement and dynamically
loadable queries are **not universal merge prerequisites**. Supported ordinary
adapters already run inside ComputationGraph. The replacement is an unreleased,
breaking v1 design; no compatibility-shim or automatic old-state migration
programme is implied.

| Work area | Main entries |
|---|---|
| Correctness, lifecycle and diagnostics | [Deadlines](#cw-06), [query matching](#cw-13), [errors](#cw-22), [status](#cw-10), [ownership investigations](#cw-35) |
| Recovery and qualification | [Pending input](#cw-14), [reset](#cw-20), [completion evidence](#cw-36), [combined failures](#cw-39), [FFI](#cw-41), [Server](#cw-46) |
| Integration and applications | [Connectors](#cw-44), [Server/UI](#cw-45), [examples/tutorials](#cw-47), [reconstruction](#cw-70) |
| Scaling and maintainability | [Publication](#cw-31), [storage](#cw-28), [hot paths](#cw-32), [decomposition](#cw-34), [benchmarks](#cw-40), [parallel tests](#cw-37) |
| Public API and release | [API curation](#cw-69), [formats](#cw-18), [lints/MSRV](#cw-61), [release documentation](#cw-73) |
| Optional capabilities | [Loadable queries](#cw-43), [view transforms](#cw-68), [housekeeping](#cw-71), [pipe policies](#cw-74), [hosting profiles](#cw-75) |

Keep CW IDs and anchors stable. Review H/M identifiers and historical CG IDs
below identify the remaining portion of a finding, not acceptance of every
claim in its original wording. Record new work here instead of creating another
backlog or a second set of checklists in a reference guide.

<a id="cw-06"></a>

## 1. Lifecycle deadlines and contention (CW-06)

**Status:** Implemented; retained anchor, not outstanding work.
**Area:** Core, Server. **Review:** M4, M18, M31.

**Implemented:** Startup has a configurable 600-second overall budget;
stop/shutdown have 60 seconds. Construction/configuration, resource construction,
activation and cleanup have separate configurable budgets. Core builders,
configuration snapshots, Server YAML/environment resolution and instance creation
carry the same settings. Typed expiry reports owner, operation, phase, budget
and request acceptance. Cancelled/timed-out stops retain cleanup ownership and
block unsafe restart; shutdown does not close configuration storage underneath
unfinished processing. Dropped-management cleanup uses visible exponential
backoff rather than fixed one-second retries.

Startup is now owned independently of its caller. Stop/shutdown supersede that
wait and join its coordinator, including when the original caller is unpolled or
dropped. Accepted component operations remain graph-owned; late startup work
cannot resurrect a stopped instance. Managed realization releases the instance
gate before construction/startup waits and retains its pending graph result.
Acknowledged interrupted construction can be retried through reconciliation or
instance restart. Server captures native plugin bindings before releasing its
registry guard and awaiting resource preparation.

The [configuration guide](computation-graph-configuration.md#library-settings)
describes defaults and retry behavior. Focused coverage includes total budgets
across phases, lock admission, unpolled/dropped startup callers, component and
instance stop retries, managed construction/reconciliation ownership, resource
construction/disposal and plugin-registry guard release.

These changes do not permit abandoning uncertain configuration commits or
interrupting previously running owners during an uncommitted resource cutover.
Other conflicting lifecycle work still has a bounded wait. Nonyielding native
code cannot be preempted by an asynchronous deadline. Wider deployment/provider
qualification remains in [CW-14](#cw-14), [CW-41](#cw-41) and [CW-46](#cw-46),
not an unfinished instance-startup contention fix.

<a id="cw-44"></a>
<a id="hosting-connectors-server-and-examples"></a>
<a id="current-required-family-boundary"></a>

## 2. Finish the remaining data connections (CW-44)

**Kind:** Issue/integration and optional stronger profiles. **Area:** Core, Server.
**Priority:** High for required families. **Effort:** Very large.
**Origins:** CG-02/03/07/11/12/24; cleanup stage 6.

**Work:** Complete the selected connectors using their existing connection,
conversion and protocol logic:

| Area | Remaining work |
|---|---|
| Kafka and Kubernetes | Native sources and initial-data providers, independently packaged and exercised against real services. |
| SQL Server | Close capture-table permission, large sink-batch and orphan-marker gaps; qualify bounded CDC paging, streamed snapshots, credentials, replay, procedure effects and cleanup against real SQL Server. |
| Database discovery | Populate and qualify the remaining family catalogs through the existing native schema reader and Server APIs; factory configuration schemas are not source data schemas. |
| Network profiles | Implement stronger snapshot/progress replacement or destination-backed completion only for profiles that require them. Native gRPC TLS is not advertised; add and qualify it before recommending this connector for encrypted gRPC deployments. |
| HTTP ingress authentication | Protect standard single-event, batch and producer-session routes with explicit authentication and per-client request limits. Define non-loopback exposure rules and test that rejected requests produce no graph changes. Webhook authentication alone does not protect these routes. |
| Application components | Move remaining old-interface test and example consumers to the native components. |

HTTP bootstrap, adaptive batching, multi-item gRPC delivery, output templates,
native SSE snapshot readers and the qualified PostgreSQL/MySQL/SQLite slices
already exist. Preserve those capabilities rather than reopening the old
network/database implementation lists. HTTP REST bootstrap does not establish
an atomic snapshot/live cut, and event-only sinks must not advertise replacement
or completion guarantees they do not implement.

The HTTP access-control work is deferred from
[PR #988's ingress review](https://github.com/drasi-project/drasi-core/pull/988#discussion_r4330093015).
Until it is implemented, standard ingress must be isolated or protected by an
authenticated, rate-limited proxy. Documenting that restriction is not a fix.
Future family conversions should be separate, reviewable changes; retaining
required ordinary plugins through graph-owned adapters does not block runtime
qualification or require immediate old-stack deletion.

SQL Server's real-database qualification is **deferred by the user** until a
usable test environment is available. Keep the family explicitly incomplete;
compilation, unit tests and authored database scenarios do not close that gate.

**Finish when:** Every required source, bootstrapper and reaction/sink exists,
and a capability-by-capability comparison finds no omitted working legacy
functionality. Qualify real services, static and freshly built dynamic libraries,
both Tokio runtimes, and actual Server consumers, including failure, cancellation,
restart and cleanup. Skipped cases and matching test counts are not proof.
SQLite has no legacy reaction to port; this work adds no public DrasiLib SQL API.

<a id="cw-45"></a>

## 3. Finish native Server and client workflows (CW-45)

**Kind:** Integration/improvement. **Area:** Server, UI and clients.
**Priority:** High for required workflows. **Effort:** Very large.
**Origins:** CG-04/09/10/14/16; cleanup stages 5-6.

**Work:** Complete creation, editing, discovery, lifecycle and result viewing for
the remaining native components in Server, UI and clients. Make waiting,
accepted, running and failed states clear. Resolve the recorded full VS Code
type-check failures in the component and YAML tooling. The concrete gaps are:

- Expose the existing Rust named-configuration-snapshot operations through HTTP,
  including revision-aware create/list/delete/restore and clear receipt semantics.
- Expose supported replacement, in-place settings changes, input/resource
  reconnection and safe quiescing with preview/revision checks. Keep persisted
  desired-state acceptance distinct from realized readiness and rollback.
- Add native-component UI lifecycle/editing controls; its inspector currently
  exposes inspection only, despite existing REST start/stop/removal operations.
- Provide first-class persisted wiring from an ordinary Server-configured query
  into an arbitrary native input. Core execution/data contracts already permit
  this composition, but Server validates native definitions separately and
  rejects external component/pipe/boundary bindings. Preserve snapshot/live
  handover and replacement ownership rather than introduce another graph owner.
- Extend solution templates to native topology and resource recipes, with
  instance-scoped ID rewriting and no accidental cross-deployment sharing.

Existing API/UI operations do not depend on every connector conversion. Only
workflows requiring a missing family depend on [CW-44](#cw-44); broader
reconstructible recipes belong to [CW-70](#cw-70).

**Finish when:** Required workflows run entirely through supported native
components and current instance-scoped APIs, without old factory registrations
or misleading success states. Exercise stale previews, rejected acceptance,
delayed/failed realization, receipt lookup after lost replies and two instances
created from the same template.

<a id="cw-14"></a>

## 4. Extend native-input recovery qualification (CW-14)

**Kind:** Qualification. **Area:** Core, Server, test-infra.
**Priority:** Medium for broader coverage; high before claiming an unqualified
deployment profile. **Effort:** Large across additional providers/connectors.
**Origins:** CG-01/22/24; reliability W2/W5.

**Qualified:** The
[native-input crash matrix](../../lib-integration-tests/tests/native_input_crash_recovery.rs)
passes 28 real SIGKILL cases: admission QoS (12), retained inputs (8), and shared
journals (8), with eager/paged storage and both Tokio runtimes. Fresh processes
reconstruct accepted input beyond the committed consumer cursor before/after
query commit and after an admission commit whose reply was lost. Checks compare
exact receipts, producer identities, payloads, checkpoints, outbox, projection,
outputs and cleanup. This closes the previously unproved RocksDB
process-restart boundary; no production recovery defect was reproduced.

**Remaining work:** Extend this evidence to the remaining connector/provider and
initial-loading combinations. It is not power-loss qualification, a guarantee for
memory-only queries, or proof that an external effect happens exactly once.

**Implemented repair:** Ordinary HTTP/gRPC WAL cleanup waits for the actual
startup subscription batch, protects replay registration against pruning, and
retains submitted cleanup through cancellation. Missing retained history fails
explicitly. This closes the discovered fast-subscriber/slow-subscriber startup
race; it does not qualify every connector-specific restart profile.
The ordinary source-WAL case is verified in [CW-36](#cw-36); it is no longer an
unproved blanket requirement.

**Finish when:** A fresh process restores exact outstanding frames and stable
retry identities without losing accepted work, inventing progress or deleting
unhandled messages. Exclude duplicate effects only where the destination's
declared deduplication/transaction contract guarantees that; report permitted
at-least-once repeats rather than promising universal exactly-once effects.

<a id="cw-36"></a>

## 5. Make passing recovery checks prove completion (CW-36)

**Kind:** Qualification. **Area:** Core, test-infra, Server.
**Priority:** High. **Effort:** Medium to large. **Origins:** CG-01/26.

**Work:** Extend proof-bearing completion checks to the remaining required
transport/deployment profiles. Keep older snapshot-only or advisory workflows
clearly separate from tests that prove processing and delivery completed.
Update the requirement checklist as those additional profiles are qualified.

**Verified standard-gRPC profile:** The
[strict workflow](https://github.com/drasi-project/test-infra/blob/13c24c57f5c0f4bd4fb56ae493d0bd9359e9cd57/.github/workflows/e2e-building-comfort-completion.yml)
builds matching components and requires a completed reference and SIGKILL
recovery. Both queries reached input 100,000; all 99,981 room outputs and 49,860
aggregate outputs matched with no duplicates or reordering. Final state matched
an independent source replay: 12 room rows and 3 floor rows. The incomplete v1
reference and the failed pre-fix recovery remain separately identified evidence.
The executed run used macOS, Rust 1.95.0 debug builds, freshly built ordinary
gRPC source/reaction adapters and a 100,000-entry outbox per query. It does not
qualify native gRPC factories, every transport or every lost-reply window.
The manual workflow recipe is present; a successful Linux CI execution is not
claimed. The older scheduled multi-transport snapshot gate remains distinct.

**Finish when:** Incomplete captures cannot pass, and each required behavior has
an exact expected result and a matching recorded run.

<a id="cw-46"></a>

## 6. Complete the Server workflow and recovery checks (CW-46)

**Kind:** Qualification. **Area:** Server, Core, test-infra.
**Priority:** High. **Effort:** Large. **Origins:** CG-20/26; cleanup stage 9.
**Depends on:** CW-36 and only the affected CW-44/45 capabilities.

**Work:** Cover the remaining combinations of delayed or failed startup, saved
configuration, reconstruction, reconnecting clients, old readers, native providers
and interrupted delivery. Include unavailable secrets/provider recipes, rejected
versions, lost acceptance replies, concurrent snapshot/configuration changes,
slow observers and reconnect after a result-history gap. A client must obtain
an authoritative replacement snapshot or explicit failure, not retain stale rows.
Record the actual Server and plugin versions used.

The latest review repairs have targeted Core/Server and integration coverage,
not a fresh full live transport/tutorial matrix. Rebuild the relevant artifacts
and qualify their actual public workflows; do not relabel an earlier bundle's
evidence as a run of the new code.

**Finish when:** The selected current-version workflows have trustworthy
end-to-end results, including failure and restart, rather than only matching
intermediate captures.

<a id="cw-13"></a>

## 7. Verify remaining query matching reports (CW-13)

**Kind:** Investigation. **Area:** Core evaluator/parsers.
**Priority:** Conditional on reproduction. **Effort:** Medium to large.

**Work:** Reproduce optional-match filters, outer-variable references, list membership and
relationship-end reports, including changes and removals.

**Implemented:** The shared query builder rejects quantified relationship
patterns with a typed unsupported-feature error before evaluator construction.
Both Cypher and GQL are covered, including quantified single-hop spellings and
later Cypher query parts. Ordinary unquantified edges and fixed chains remain
supported. Full variable-length execution was not added.

**Finish when:** Each remaining report has a tested fix or evidence that it does
not reproduce.

**Reports:** [#795](https://github.com/drasi-project/drasi-core/issues/795),
[#796](https://github.com/drasi-project/drasi-core/issues/796),
[#797](https://github.com/drasi-project/drasi-core/issues/797),
[#798](https://github.com/drasi-project/drasi-core/issues/798),
[#807](https://github.com/drasi-project/drasi-core/issues/807),
[#808](https://github.com/drasi-project/drasi-core/issues/808),
[#812](https://github.com/drasi-project/drasi-core/issues/812).

<a id="cw-43"></a>
<a id="cw-43-optional-native-query-plugin"></a>

## 8. Finish loadable query support and application definitions (CW-43)

**Kind:** Optional capability. **Area:** Core, native SDK/Host, Server.
**Priority:** High only when this packaging capability is selected. **Effort:** Large.
**Origin:** CG-11.

**Work:** Resolve the separate performance/design investigation, then make query
processing an independently loadable component using the existing evaluator.
Carry its real storage, initial loading, scheduled work and result-reading
capabilities across that boundary. Built-in native queries and query chaining
already work without a dynamically loaded query engine. Do not duplicate that
implemented path. Remaining Server composition workflows belong to
[CW-45](#cw-45); general reconstructible definitions, including merger/provider
recipes, are tracked in [CW-70](#cw-70).

**Finish when:** Applications work with query support present or absent, missing
support is reported clearly, and loaded queries preserve required results,
transactions, restart and cleanup behavior.

<a id="cw-47"></a>

## 9. Finish required examples and normal builds (CW-47)

**Kind:** Issue/integration and qualification. **Area:** Server, Learning, test-infra.
**Priority:** High for required examples. **Effort:** Large overall.
**Origins:** CG-17/28; cleanup stages 6/9.

**Work:** Implement the GPU Lab deployment using the standard Server, finish
examples for the remaining selected connectors, and verify readiness, reset and
expected effects. The stock-Server GPU directory is still a reserved deployment,
not the implemented embedded lab. Reuse its shared domain/plugin/UI code.

**Implemented build repair:** The older copied Learning Trading frontend now
passes `tsc && vite build` after removing its unused `createdReaction` flag.
Compiler checks and cleanup behavior are unchanged. The runnable tutorial
continues to use the sibling Server application, not that older copy.

Retain the seven migrated tutorials' existing scenario checks and automate
uncovered walkthrough/CI profiles with exact expected rows, effects and visible
updates, bounded waits and repeatable reset. The Getting Started evaluator
already checks expected output; it does not need another evaluation framework.

**Qualification boundary:** Move-a-Wall, the embedded GPU Lab and tutorial
migrations have earlier live evidence. Run their affected acceptance scenarios
against the latest review-repair bundle, including snapshot/restart behavior;
do not reopen completed migrations or describe this as the older OTel/WAL build.

**Finish when:** Required examples build normally and run against the supported
Server with clear setup, shutdown and recovery behavior.

<a id="cw-18"></a>

## 10. Keep saved data and retries unambiguous (CW-18)

**Kind:** Format/release improvement. **Area:** Core contracts, storage, SDK/Host.
**Priority:** High. **Effort:** Medium to large. **Review:** M28.

**Work:** Audit current saved and transmitted formats. Define which fields
identify a request, result or configuration, and add fixed examples of expected
bytes or identities. Formatting or diagnostic changes must not turn a retry into
new work. Unsupported or damaged data must fail clearly.

Dispatch persisted/wire records from their version/header before decoding the
current payload layout, so unsupported versions are distinguished from corrupt
current records. Add golden bytes/identity fixtures for envelopes, graph
definitions, QoS/delivery metadata and query state. State the current-format
upgrade/refusal/reset policy; diagnostics and formatting are not identity fields.

**Finish when:** Current-format restart and retries preserve identity, and
incompatible records are rejected without silently resetting valid state.

<a id="cw-19"></a>

## 11. Reject excessively deep data safely (CW-19)

**Kind:** Hardening/qualification. **Area:** Core contracts, codecs, providers.
**Priority:** High where input could terminate the process. **Effort:** Large.

**Work:** Audit supported decoders and internally constructed values beyond the
already bounded paths. Apply appropriate size and nesting limits, including safe
release of deeply nested values after an error or cancellation.

**Implemented:** Graph publication, JSON/binary codecs and encoded-size accounting
enforce a 1,024-entry lineage bound. Shared lineage destruction is iterative,
and diagnostic formatting does not recursively traverse ancestry. The broader
deep-value audit remains distinct from this envelope-lineage boundary.

**Finish when:** Supported malformed-input cases fail with an error rather than
exhausting the call stack or terminating the process.

<a id="cw-66"></a>

## 12. Retire mitigated legacy dependency paths (CW-66)

**Kind:** Dependency remediation. **Area:** Core and affected consumers.
**Priority:** Medium for upstream replacements; reassess before expanding usage.
**Effort:** Depends on compatible upstream releases.

**Origin:** The RustSec findings recorded during
[PR #988's dependency review](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591475780).

**Remediated:** Supported dependency paths and consumer locks now use patched
TLS, Tokio, XML, QUIC, crypto and gRPC compression versions; maintained `imbl`
replaces `im`/`sized-chunks`. Tiberius retains the existing TDS 7.3 profile.
HTTP/XML, MSSQL, AWS/Azure safeguards, signature verification, storage and
test-infra HTTP/2/telemetry behavior have executable coverage.

The reviewed raw audit counts fell from 30 to 7 in Core, 18 to 3 in Server,
and 21 to 6 in test-infra. These are recorded scan results, not zero-advisory
claims. The retained findings have the following boundaries:

| Retained path | Mitigation or unaffected usage |
|---|---|
| Legacy Azure Core 0.20/0.21 | Per-call transport safeguards preserve SDK signing/TLS and redact authorization before retry/success/cancellation logging. SharedKey/Table capabilities remain available. |
| Azure's quick-xml 0.31 | A 0.41 preflight rejects duplicate/excessive attributes in fixed-schema service metadata; the affected namespace resolver is not used. User blob content remains streamed and is not treated as XML. |
| RSA 0.9 | MySQL uses public-key OAEP encryption, not the affected private-key decryption/signing operations. |
| MySQL's zstd 0.13 / zstd-safe 7 | Existing read-side decoding does not use the affected context reuse, write-side, experimental, dictionary or multithreaded operations. |
| ZIP 0.6 build tooling | Extraction is limited to the trusted bundled Swagger UI archive, not user-supplied archives. |
| Old rand transitive dependency | The advisory's required logging feature is disabled. |

**Remaining work:** Adopt compatible patched Azure/Table and MySQL compression
releases when available, and re-audit before changing these usage boundaries.
The safeguards do not patch every use of the retained libraries. Raw scanners
remain nonzero; no new advisory exceptions or weakened allocator/license rules
were added merely to make a check green. Prior Azure debug/trace logs may have
contained credentials: affected operators should rotate their account keys.

<a id="cw-20"></a>

## 13. Verify interrupted reset across storage providers (CW-20)

**Kind:** Qualification. **Area:** Core and persistent providers.
**Priority:** High. **Effort:** Large, mainly failure testing.

**Work:** Exercise process failure during query reset and removal with the
remaining supported storage providers and their optional operations. Exercise
failure before/after reset markers, partial state deletion, cancelled submitted
I/O, reopen and repeated deprovisioning. Use real reopen for every provider
advertising persistence; a memory provider is not crash-durable evidence.

**Finish when:** Partly cleared state cannot reopen as healthy, and explicit
recovery restores correct rows, progress and output identity.

<a id="cw-22"></a>

## 14. Make errors explain what happened and what to do (CW-22)

**Kind:** Issue/audit and API improvement. **Area:** Core, Server, UI.
**Priority:** High. **Effort:** Large. **Review:** H8, remaining M17.

**Work:** Present the same failure consistently through the library, Server, UI,
logs and monitoring. Identify the affected component, what stopped, what work is
saved or unfinished, whether cleanup failed, and what can safely be retried.
Audit remaining public `expect`/panic and error-erasure paths, distinguishing
proven internal invariants from recoverable failures. Check legacy source
initialization/retry state and middleware recovery poisoning/retry classification.
Replace fabricated edge identifiers and ambiguous debug-only/duplicated cause
messages with actual operation context.

Typed source-subscription failures, reconciliation aggregates and the named
manager/inspection panic paths have already been repaired. Preserve
`DrasiError::classification()` and typed causes instead of replacing that working
mechanism with string errors or assuming classification was lost.

**Finish when:** A user can diagnose and recover from the selected failures
without guessing from a generic error or mistaking rejection for successful work.

<a id="cw-23"></a>

## 15. Make live status and log updates reliable (CW-23)

**Kind:** Qualification/API improvement. **Area:** Core, Server, SDK/Host.
**Priority:** High. **Effort:** Medium to large. **Origins:** CG-05/26; observer review.

**Work:** Complete operational-status and log handover, slow-reader and reconnect
behavior. Detect missed updates and provide a way to obtain current state.
Preserve instance identity and release observer state when its owner is removed.
Document and exercise the snapshot/changed acknowledgement contract for progress
observers so clients do not accidentally busy-loop without consuming a snapshot.
Test coalesced readiness/failure updates, late callbacks, slow subscribers,
disconnect/reconnect and replacement; coalesced status is not a full event log.

**Finish when:** Readers either receive the required updates or are explicitly
told to refresh, and old readers cannot report a replacement as their old component.

<a id="cw-10"></a>

## 16. Finish public status and result consistency checks (CW-10)

**Kind:** Issue and qualification. **Area:** Core, Server.
**Priority:** High. **Effort:** Medium to large. **Origin:** review's inspection findings.

**Work:** Check remaining public outcomes for partial startup/shutdown, missing
or duplicate components, replaced query readers, missing result history and
initial-result/live-update races. Define the compatibility view's legacy
projection versus full native inspection explicitly: generic native
transformers must not be silently presented as a complete graph when omitted.
Preserve the repaired `Reconfiguring` status and initialized, generation-fenced
snapshot/history reads during quiescence.

**Implemented:** The instance root in both `get_graph` and retained read-only
views follows the existing library running flag instead of a constant. It is
Stopped before startup and after stop, and Running while a partial startup still
owns active work. This is not an aggregate component-readiness or cleanup-success
claim; individual failures remain visible separately.

**Finish when:** Supported library and Server operations report the same truthful
state and results through replacement, restart and failure.

<a id="cw-03"></a>

## 17. Check remaining extension failure boundaries (CW-03)

**Kind:** Qualification/hardening. **Area:** Core, providers and plugin boundaries.
**Priority:** High. **Effort:** Medium to large.

**Work:** Audit uncovered factory, provider, data-validation and destruction
callbacks for catchable Rust failures. Add protection only where a concrete
unprotected path is found.

**Finish when:** The selected callback failures leave useful diagnostics, prevent
reuse of damaged objects and allow unrelated healthy work to continue.

<a id="cw-05"></a>

## 18. Extend safe-stop coverage to remaining shared resources (CW-05)

**Kind:** Qualification. **Area:** Core, shared resources and hosted instances.
**Priority:** High. **Effort:** Medium to large.

**Work:** Exercise graceful stop and explicit forced interruption with remaining
providers and component-private shared resources. Verify who owns unfinished
work when a stop times out or its caller stops waiting. Include multi-stage
required fan-out, a disconnected consumer, shared transaction domains and
bidirectional instance links whose shutdown must preserve required return paths.

**Finish when:** No supported stop path abandons accepted work, hides incomplete
cleanup or interrupts an unreported shared owner.

<a id="cw-07"></a>

## 19. Verify overlapping application changes (CW-07)

**Kind:** Qualification. **Area:** Core, Server management.
**Priority:** High. **Effort:** Large, mainly testing.

**Work:** Combine saved configuration changes with conflicting start, stop,
replacement and removal requests. Include failed construction and cancelled
callers while unrelated operations continue. Preserve stale-preview rejection,
isolated start groups and control/peer/resource completion servicing already
fixed by the review repairs; extend adversarial schedules rather than replacing
those contracts with all-or-nothing deployment rollback.

**Finish when:** Accepted changes remain owned, conflicting changes stay ordered,
and the reported result accurately distinguishes applied changes from failures.

<a id="cw-08"></a>

## 20. Complete cleanup checks across native providers (CW-08)

**Kind:** Qualification. **Area:** Core, providers, SDK/Host.
**Priority:** High. **Effort:** Large, mainly testing.

**Work:** Extend combined cleanup checks to the remaining native component,
storage and plugin combinations. Verify that failed or cancelled cleanup retains
the resources and errors needed for a later retry.

**Finish when:** Each selected combination releases its actual resources or
reports unfinished ownership truthfully, including after repeated cleanup attempts.

<a id="cw-17"></a>

## 21. Make components stop their own background work (CW-17)

**Kind:** Ownership audit/qualification. **Area:** Core, components and providers.
**Priority:** High. **Effort:** Medium to large.

**Work:** Audit remaining components and providers that create their own tasks,
threads or connections. Establish ownership before starting them and retain it
until their real work has ended, including during failed startup and cancellation.
Include already-submitted blocking storage work and rejected construction
results. Measure retained failed-cleanup owners separately from actual leaks;
supervised retry policy belongs to [CW-06](#cw-06).

**Finish when:** Removed or stopped components leave no unowned background work
that can affect a replacement or keep its resources unavailable.

<a id="cw-39"></a>

## 22. Test the remaining combinations of failures (CW-39)

**Kind:** Qualification. **Area:** Core, providers, connectors and test-infra.
**Priority:** High. **Effort:** Large. **Origins:** CG-20/22/23/24; reliability W4-W6.

**Work:** Cover lost history with storage failures, clock changes with restart,
held late input across crashes, disconnected consumers with damaged history,
and lost destination replies with failed completion writes. Add direct proof
that a failed configuration read cannot cause saved state to be deleted.
Include concurrent live changes during bootstrap, independently failing/slow
multi-source snapshots, unavailable source positions, repeated timer bursts and
reconstruction after each transaction participant boundary. Compare rows,
progress, scheduled work and external effects with an independent oracle.

**Finish when:** Each selected combination has exact expected data, progress,
effects and cleanup outcomes rather than an inference from single-failure tests.

<a id="cw-41"></a>

## 23. Complete checks using real plugin libraries (CW-41)

**Kind:** Safety audit/qualification. **Area:** ABI, SDK/Host, instance-host and contracts.
**Priority:** High. **Effort:** Large. **Review:** M41. **Origins:** CG-05/06/25.

**Work:** Extend malformed-data, version-rejection, cancellation, delayed-callback
and replacement tests using independently built libraries. Measure per-instance
resource release through repeated use; distinguish it from libraries intentionally
kept loaded for the process lifetime.

Complete the separately requested
[PR #988 native FFI safety review](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591475780):
inventory unsafe operations in the ABI, SDK, Host and instance-host paths;
check each pointer's lifetime, callback/release order and cross-thread use;
and add failing cases for any uncovered contract. A `SAFETY` comment or a
validated function-pointer assertion is not itself proof that the complete
boundary was reviewed. The broader audit is deferred, not reported as passed.

Fuzz envelope/binary/graph-change/query/wire decoding, including error-path drop
and bounded allocation. Use Miri for applicable unsafe helpers and loom or
equivalent models for acknowledgement/journal/pipe interleavings. Justify
cross-thread `Send`/`Sync` and release/callback ordering against the actual opaque
handle contracts. Unsafe-code counts or missing defensive null checks alone
are not findings; any reported defect needs a supported-call reproduction.

**Finish when:** The selected libraries reject invalid calls and release instance
resources without letting old callbacks act on replacements.

Include fixture disposal in the evidence: normal, repeated and failed-start
runs must remove only their owned containers and volumes. An environment failure
or a skipped database case is not a successful plugin run.

<a id="cw-40"></a>

## 24. Measure larger and longer-running applications (CW-40)

**Kind:** Performance qualification. **Area:** Core, Server, test-infra.
**Priority:** High for release claims. **Effort:** Large. **Review:** M39.
**Origins:** CG-13/27; execution-plan performance qualification.

**Work:** Measure growing application sizes, busy and slow branches, large
messages, shared storage, outages and prolonged operation. Set acceptable limits
for response time, processing cost, memory and disk use before declaring success.

Produce a reproducible **comparison against main**, pinning both revisions,
toolchain, features, input/result oracle, providers and storage guarantees.
Measure throughput, end-to-end and control tail latency, CPU, allocations,
memory/disk growth and cancellation responsiveness. Cover one/many queries,
small/large payloads, fast/slow fan-out, disabled/enabled recovery and both runtime
flavors. Existing branch-only measurements and old percentages are not this
comparison. Preserve the [CPU-scaling reference](computation-graph-maintainers-guide.md#measure-the-path-actually-changed)
as a separate, bounded acceptance workload rather than a universal speedup claim.

**Finish when:** The selected deployment profiles have repeatable measurements,
exact result checks and explicit operating limits.

<a id="cw-31"></a>

## 25. Avoid rebuilding unchanged information for status updates (CW-31)

**Status:** Implemented; retained anchor, not outstanding work.
**Review:** M6/publication findings. **Origin:** execution-plan E2.

Desired snapshots, observed/registry maps and reports share unchanged data.
Dirty component captures refresh read capabilities without relying solely on
topology revision. Event translation visits changed members and uses immutable
kind lookups; producer updates select only their ordered edge range. Coherent
addition/provider publications, weak ownership, event order and history remain
covered.

Synthetic 128-publication runs at 4,096 nodes fell from about 256 ms to 0.33 ms.
Native allocation samples show substantially less retained publication memory;
live controller/notification measurements cover both runtimes. This is reporting
overhead, not a claim of equivalent query-throughput improvement. See the
[maintainer guide](computation-graph-maintainers-guide.md#inspection-and-diagnosing-a-running-system)
for the repeatable diagnostics. Whole-graph exports still cost work proportional
to their output; tombstone reclamation and wider workloads remain in
[CW-33](#cw-33) and [CW-40](#cw-40).

<a id="cw-33"></a>

## 26. Reclaim bookkeeping left by removed components (CW-33)

**Kind:** Long-running resource improvement. **Area:** Core graph bookkeeping.
**Priority:** Medium. **Effort:** Medium to large. **Review:** remaining M7.

**Work:** Measure repeated removal and addition within one long-running graph,
then reclaim or reuse entries no longer needed by pending work or cleanup.
Preserve the distinction between an old component and a replacement. Live-member
scans are fixed; node/slot tombstone reclamation is not. Consider reusable arenas
and typed indexes only with generation-safe handles and retained cleanup/job
references accounted for.

**Finish when:** Keeping a stable number of active components does not accumulate
unnecessary entries indefinitely, and delayed old requests cannot affect new ones.

<a id="cw-29"></a>

## 27. Finish memory limits for saved queues and results (CW-29)

**Kind:** Capacity improvement. **Area:** Core queues, retained state and providers.
**Priority:** Medium. **Effort:** Large.

**Work:** Add the missing byte limits for shared transactional queues, saved
output and current result rows. Reserve required capacity before processing
commits, and define what happens to an oversized input or a full buffer. Separate
encoded-envelope admission quotas, payload-cache bounds, receipt/progress
metadata, backend buffers and current-result state; none alone bounds total
process memory. Existing optional FIFO/retained/non-shared QoS byte budgets and
paged payload caches are not missing implementations.

**Finish when:** The selected limits hold through slow consumers, cancellation
and restart without accepting work that cannot be retained safely.

<a id="cw-28"></a>

## 28. Reduce unnecessary disk work and growth (CW-28)

**Kind:** Storage/concurrency improvement. **Area:** Core retained, QoS, delivery and merge stores.
**Priority:** Medium. **Effort:** Medium to large. **Review:** M10, M22, M23, M25.

**Work:** Measure full-state rewrites, repeated reads, small writes and obsolete
stored records. Improve the expensive cases while preserving exact saved progress
and retry identity. Specifically measure whole-buffer time-merge persistence,
whole-metadata QoS/delivery ledger rewrites, receipt/cursor/head updates and
per-item acknowledgement/decode work. Evaluate per-entry/per-stream records,
safe write coalescing and shorter journal locks where the measurements justify
them. Keep actual transaction ownership, crash recovery and subscriber membership
atomic; do not release a lock early if that invalidates the commit protocol.

Retained and QoS batch receive already exists. The remaining batching question
is persistence/acknowledgement cost, not a missing receive API. `Busy` is bounded
admission, not silent loss; preserve explicit caller retry/backpressure behavior.
Known pre-commit rollback/revocation handling is repaired. Reproduce other older
storage reports before treating them as bugs.

**Finish when:** Selected workloads show lower storage cost or bounded growth
without changing results, recovery or independent query retention choices.

<a id="cw-32"></a>

## 29. Remove measured repetition from data processing (CW-32)

**Kind:** Measured performance improvement. **Area:** Core processing, codecs and pipes.
**Priority:** Medium. **Effort:** Medium. **Review:** M13, M14, M24, M29.

**Work:** Find significant repeated encoding, hashing, metadata construction and
history scans in the retained processing paths. Include codec round-trips used
only for validation, size measurement, progress-map cloning, payload copies,
per-event port parsing/hex formatting, repeated lock acquisition and boxed
futures. Replace idle polling with owner notifications where practical, and
measure peer-handler wake fan-out and shared-queue head-of-line delay.

Use existing batched/poll paths before adding another fast-path API. Cache only
data whose identity is truly stable: encoded size and validation may depend on
branch-specific annotations as well as an immutable event payload. Profile
actual outbox reads and commit topology rather than assuming every read clones
all history or every output needs a separate transaction. Preserve required
ordering, contiguous completion and independent subscriber obligations.

**Finish when:** Measured processing cost falls while exact output, identity,
replay and failure behavior remain unchanged.

<a id="cw-37"></a>

## 30. Make asynchronous tests reliable (CW-37)

**Kind:** Test-infrastructure improvement. **Area:** Core, Server, Learning, CI.
**Priority:** Medium. **Effort:** Medium to large.
**Review:** M34; test-organization findings.

**Work:** Replace timing guesses with actual completion signals where tests
depend on them. Replace calendar-date assumptions with controlled clocks.
Isolate shared test state and reproduce the Server Trading
`dialogAccessibility.test.tsx` failure under parallel execution. Preserve
separate checks for valid deferred construction and genuinely invalid topology;
do not exclude failing cases from a claimed full-suite pass.

**Already fixed:** The stale empty-graph and missing-dependency lifecycle
expectations, real subscription-count/cleanup checks, public doctests and the
Server telemetry fixture's assumed immediate query readiness. Preserve the
intentional child-worker ignores: the parent tests execute them with the required
state and watchdogs.

**Implemented timer slice:** The future-notification deduplication regression
uses the existing `QueryTestControl`, fixed logical input times and explicit
before-due, due and stale-wake checks on both Tokio runtimes. Actual reaction
observations fence input processing and downstream receipt separately. This
removes wall-clock waiting from that regression, not every timing assumption
or the workspace-wide serialization policy.

Replace the workspace-wide `--test-threads=1` workaround only after identifying
and isolating the tests that actually share ports, files or global state. Add a
normal parallel CI run without dropping serialized coverage where it is still
needed. This is the deferred
[PR #988 parallel-testing follow-up](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591575864).

Share repeated integration fixtures/helpers and measure unnecessary test-binary
build/startup overhead. Consolidate only where it preserves required process
isolation, especially global plugin callbacks, crash workers and resource probes.

**Finish when:** Selected tests reliably prove the intended behavior under load
and on both supported runtime styles, without weakened assertions or inflated timeouts.

<a id="cw-61"></a>

## 31. Restore useful compiler and code-check warnings (CW-61)

**Kind:** Build/API hygiene. **Area:** Core, contracts, test-infra and CI.
**Priority:** Medium. **Effort:** Medium overall; small packages can be done separately.
**Review:** M37; MSRV/lint findings.

**Work:** Enable unused-code warnings in manageable parts of the project, remove
genuine leftovers and keep only justified exceptions. Reproduce and resolve the
recorded test-infra code-check failures rather than hiding them with broad suppression.
Remove genuinely unused dependencies; introduce public rustdoc and justified
panic/expect lint coverage incrementally in computation, management and contracts.

Declare and test the actual supported Rust minimum, aligning manifests,
dependencies and lint configuration. The Clippy MSRV setting of 1.70 is not
proof that the current crate builds on 1.70. Exercise supported feature
combinations; do not mechanically adopt newer standard-library conveniences
until that policy permits them.

The recorded Rust 1.95 `make lint-check` failure starts in unchanged
`test-data-store` code. The focused `test-run-host` check also reports an unused
`ProfilerSummary`, derivable defaults, checked-then-unwrapped options and manual
multiple-of checks. Passing the receiver's 139 unit tests does not close these
lint failures.

**Finish when:** Selected packages pass their normal checks with useful warnings
enabled across supported configurations.

<a id="cw-67"></a>

## 32. Qualify the pinned CI Protocol Buffers compiler (CW-67)

**Kind:** CI qualification. **Area:** Core native/coverage workflows.
**Priority:** Medium. **Effort:** Small to medium.

**Work:** Run the updated native FFI/build/wire matrix and both coverage jobs on
their actual supported runners. Verify the pinned compiler and resulting
HTTP/gRPC libraries on Linux x86_64, macOS ARM64 and Windows x86_64.

**Implemented:** A [shared setup action](../../.github/actions/setup-protoc/action.yml)
uses the [pinned installer](../../ci/install_protoc.py) for protoc 36.2, matching
the locally used compiler. Official archive SHA-256 values are fixed per
platform; extraction, executable version and standard includes are checked
before publishing `PROTOC`, `PROTOC_INCLUDE` and `PATH`. Target-cache keys include
the installer pin and runner architecture. Corrupt archives, unsupported
platforms, wrong versions and existing installation directories fail explicitly.

Installer tests and real macOS/Linux ARM64 execution pass locally. All 12
checked-in schemas compile in their actual protocol groups, producing identical
descriptors with the pinned macOS/Linux tools and the existing local compiler.
This does not replace the full CI matrix or establish Windows execution.
The remaining qualification follows
[PR #988's CI supply-chain review](https://github.com/drasi-project/drasi-core/pull/988#pullrequestreview-5591475780).

**Finish when:** CI records the expected compiler version, rejects an unexpected
artifact, and the existing cross-platform native build/wire checks pass.

<a id="cw-63"></a>

## 33. Remove the remaining old plugin support (CW-63)

**Kind:** Breaking-release cleanup. **Area:** Core, Server and remaining consumers.
**Priority:** Medium. **Effort:** Medium to large. **Review:** M21.
**Depends on:** Required consumer moves in CW-44/45.

**Work:** Remove old hosts/adapters and obsolete binary-version branches once
their required users have moved. Remove bare-row decoding and guessed-schema
fallbacks that turn malformed current input into apparent success. Inventory
ordinary hosts/proxies/factories, duplicated legacy index/provider adapters,
static registrations and old ABI field gates before removal. Port useful
behavior assertions to current interfaces; remove compatibility-only fixtures,
dependency edges and build paths only after their last required user moves.

**Finish when:** The selected current interfaces are sufficient, unsupported
libraries/data fail clearly, and required functionality no longer depends on the
retired code.

<a id="cw-34"></a>

## 34. Simplify the remaining management code (CW-34)

**Kind:** Maintainability improvement. **Area:** Core graph/query/pipe management.
**Priority:** Medium. **Effort:** Medium to large. **Review:** M1, M2, M9, M15, M27.

**Work:** Separate component ownership, commands, connections, scheduling and
shutdown where they are still difficult to reason about. Share repeated recovery
logic through small changes alongside the work that needs them. Concrete work:

- Consolidate event/completion dispatch across the main controller and
  reconciliation waits; preserve the repaired resource/peer/control servicing.
- Finish structural-validator consolidation across construction and live
  mutations, with generated/property cases for streams, roles, references,
  cycles and unused bindings. Compare equivalent entry points at the same
  validation phase; absent dependencies remain valid waiting declarations.
- Separate operation groups, reservations, connection/control state and shutdown
  into explicit ownership/state models. Clarify slot/status coherence and share
  relationship-policy predicates instead of adding independent registries.
- Extract query ledgers/commit preparation and repeated recovery work from the
  large query/controller/reconciler/management modules. Share receiver,
  acknowledgement and retention-window logic where the contracts actually match.

The specific validation holes and controller failure paths reported by the
review are fixed. These remaining extractions reduce future drift; they are not
substitutes for those fixes or a mandate for another executor.

**Finish when:** Changed areas have clear responsibilities and one authoritative
graph. Differential/property tests and both-runtime failure schedules preserve
processing, cancellation, recovery and cleanup, while matched measurements show
no added per-event cost. Fewer files or lines alone is not completion.

<a id="cw-24"></a>

## 35. Let applications control logging and monitoring (CW-24)

**Kind:** Operational capability. **Area:** Core observability and applications.
**Priority:** Medium. **Effort:** Large.

**Work:** Implement the selected application-owned collection, filtering,
export and shutdown controls from the
[monitoring design](https://github.com/drasi-project/design-documents/tree/77b155319fd8160496ad059f6fdcb1e6bc93e43d/drasi-lib/tracing-logging).
Resolve the remaining profiling interfaces and dependency choices.

**Finish when:** Applications can choose the required monitoring level without
Drasi replacing their logging setup or disabling existing logs.

<a id="cw-25"></a>

## 36. Follow a request through processing and delivery (CW-25)

**Kind:** Operational capability. **Area:** Core, plugins, Server and destinations.
**Priority:** Medium. **Effort:** Large.

**Work:** Carry useful request and attempt information across queues, plugins,
network calls and retries. Define how a result relates to several inputs, while
keeping diagnostic information separate from the identity of saved business work.

**Finish when:** Selected requests can be followed through actual completion,
with meaningful timing and no invented duration across a restart.

<a id="cw-26"></a>

## 37. Make measurements consistent across components (CW-26)

**Kind:** Operational capability/qualification. **Area:** Core, plugins, Server.
**Priority:** Medium. **Effort:** Large.

**Work:** Implement the agreed measurements for availability, input/output,
completed handling, queue pressure, progress lag, errors and processing time.
Keep names and units consistent, prevent double counting and exclude sensitive data.
Include retained/QoS pressure, earliest-subscriber lag and blocked admission so
intentional lossless backpressure is visible rather than mistaken for a stalled
runtime.

**Implemented first slice:** Query committed-input and produced-output positions
can be read from their actual owners, with lifecycle/output identity and
persistence information. Server has optional OTLP export of those measurements.
`drasi.query.progress.sequence` distinguishes `input.committed` from
`output.produced`; filtered inputs advance the former without inventing output.
The standard-gRPC recovery evidence combines these observations with actual
recorded batches and final snapshots. Remaining work is all-component
instrumentation and qualification of the other profiles, not implementation of
this first exporter or another Server progress endpoint.

**Finish when:** Enabled measurements report exact expected counts and timing
boundaries across normal work, retries, batching and failure.

<a id="cw-27"></a>

## 38. Export monitoring data without blocking useful work (CW-27)

**Kind:** Operational capability/qualification. **Area:** Core, Server, export adapters.
**Priority:** Medium. **Effort:** Large.

**Work:** Provide an optional application-owned export path for logs, measurements
and completed traces to OpenTelemetry services, local Drasi consumers, or both.
Bound waiting data, report dropped records and prevent monitoring its own output.

**Implemented first slice:** Server can own a dedicated OTLP/HTTP JSON query-
progress meter provider without replacing the application's global provider.
Export has a configured HTTP timeout, and cancelled shutdown retains its worker.
The broader logs/traces export and all-component measurements remain open.

**Finish when:** Slow or broken monitoring destinations cannot stall business
processing, and export/flush/shutdown have explicit ownership.

<a id="cw-12"></a>

## 39. Investigate reported errors in grouped results (CW-12)

**Kind:** Investigation. **Area:** Core evaluator and indexes.
**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

**Work:** Reproduce the reported missing first results, incorrect removal of
groups, zero-valued groups and averages containing null values. Check changes,
restart and cleanup of old group records.

**Finish when:** Each report is either closed with evidence or fixed with exact
result and recovery checks.

**Reports:** [#384](https://github.com/drasi-project/drasi-core/issues/384),
[#498](https://github.com/drasi-project/drasi-core/issues/498),
[#806](https://github.com/drasi-project/drasi-core/issues/806),
[#411](https://github.com/drasi-project/drasi-core/issues/411).

<a id="cw-21"></a>

## 40. Investigate remaining retry and completion questions (CW-21)

**Kind:** Investigation. **Area:** Core merge, admission and delivery recovery.
**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

**Work:** Check clock changes during time-based merging, retries without an
original timestamp, content checks on output confirmation, expired replay
records and incorrect uncertainty reported for writes never attempted.

**Finish when:** Each concern has a clear result distinguishing rejected, saved
and genuinely uncertain work, with a fix where a failure is reproduced.

<a id="cw-35"></a>

## 41. Resolve the remaining ownership and limit findings (CW-35)

**Kind:** Investigation, not confirmed defects. **Area:** Core lifecycle and storage.
**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.
**Review:** M8 and remaining ownership/limit findings.

**Work:** Resolve the specific review concerns about shared-resource ownership,
lock ordering, numeric limits and expired references. The bounded questions are:

- Can a transient control failure leave a component blocked after its
  prerequisite recovers, without a usable retry/reconciliation path?
- Does `open_run` mutate `Starting` before epoch-exhaustion validation in a way
  that prevents a safe retry? Treat this as a numeric-boundary case, not a normal
  startup failure.
- What should `Unbind` with `RemovalPolicy::Reject` reject, and do the public
  contract and behavior agree for connected and already-waiting consumers?
- Can rejected additions/resources or the reaction adapter's competing
  deprovision paths release a claim too early or retain an unreported owner?
- Does timeout/cancellation during `SourceTransactions` preserve the submitted
  storage operation's owner and uncertainty through actual completion?

Reproduce an observable failure before changing these paths. Missing prerequisites
and retained/fenced cleanup ownership are not themselves failures.

**Finish when:** Each named concern has a protected fix or an evidence-backed
closure, rather than remaining an open-ended code-cleanup task.

<a id="cw-68"></a>

## 42. Add a stateful view-transform component (CW-68)

**Kind:** Optional capability. **Area:** Core/native components.
**Priority:** Future capability. **Effort:** Large.

**Work:** Add a distinct component that maintains an output dataset as a
transformation of the current input dataset. Keep the existing map/jq components
as stateless event mappers: they apply configured rules to meaningful incoming
events and ignore events or rules that make no sense in that context. A view
transform is neither a new mode for those mappers nor a replacement for them.

The new component should use the standard graph-change ports and support nodes,
relationships, additions, full replacements, property patches and deletions.
Retain the input information and actual emitted-output associations needed to
reconstruct patches and maintain the view correctly. In particular, retract
outputs that no longer pass a filter, delete children removed by one-to-many
transformations, and remove old identities when a mapping changes an output key.
Do not infer missing input values from a patch or rerun a historical
non-deterministic transformation to guess what was previously emitted.

Reuse shared codecs, identity, lifecycle and transaction facilities. Storage is
an implementation choice for this component, not a requirement imposed on every
component. Report memory-backed and durable recovery honestly; only advertise
snapshot, replay and transactional capabilities backed by real implementations.
When durable output is supported, commit its view state, output and progress
coherently, with bounded retention and cancellation-safe cleanup.

**Finish when:** Incremental results agree with an independently recomputed view
after patches (including removals and explicit nulls), replacements, filtering,
relationship changes and one-to-many identity changes. Required snapshot/live
handover, replay, failure, restart and cleanup scenarios pass for each advertised
capability. Existing stateless map/jq event-mapping behavior remains available
and unchanged by this new component.

<a id="cw-69"></a>

## 43. Curate the public API before a stable release (CW-69)

**Kind:** API improvement. **Area:** Core, shared contracts and SDK consumers.
**Priority:** High before advertising a stable API. **Effort:** Large. **Review:** H9.

**Work:** Replace accidental wildcard exposure with intentional exports and
document what the `v1` namespace promises. Review public construction, builders,
non-exhaustive types and third-party collection exposure; choose extensibility
per type instead of mechanically hiding every field. Group the facade by its
actual responsibilities while retaining one runtime owner.

Make inspection/configuration contracts deliberate: stable structured values
instead of `Debug` strings, explicit component kinds rather than catch-all role
fallbacks, documented casing/translation between public and factory settings,
and consistent `ManagementStatus.converged` field/method meaning. Define builder
setting precedence, including repeated `with_management` calls. Clarify
`BroadcastPipe`'s single-receiver drop-oldest contract/name without confusing it
with QoS multicast. Preserve typed error classification and causes.

**Finish when:** Representative downstream clients compile against the curated
surface; public API/semver checks detect unintended changes; documentation
distinguishes Rust API, plugin ABI, wire and persisted-format promises. Numeric
revision/fencing values are not treated as authorization capabilities.

<a id="cw-70"></a>

## 44. Complete reconstructible component and provider recipes (CW-70)

**Kind:** Capability improvement. **Area:** Core, SDK/Host and Server.
**Priority:** Medium; high for deployments requiring those recipes. **Effort:** Large.
**Origins:** CG-08/15; reliability W9.

**Work:** Offer graph-owned factory construction through ordinary source/reaction
APIs as well as the existing object-based path, so construction failure can
remain a visible, retryable declaration. Extend concrete provider/component
recipes with real dependency, implementation/version and secret-reference
information, including required source-time merger/provider combinations.
Exports must distinguish inspectable objects from reconstructible definitions;
an arbitrary application object cannot be recreated from invented empty settings.

**Finish when:** Each advertised recipe reconstructs on a clean instance with
the same connections and resource scope. Missing factories, incompatible
versions, unavailable secrets, failed construction and cancelled restoration
leave truthful, retryable declarations and no orphaned resources.

<a id="cw-71"></a>

## 45. Add optional configuration-store housekeeping integrations (CW-71)

**Kind:** Optional operational improvement. **Area:** Core management, providers, Server.
**Priority:** Medium for long-lived managed deployments. **Effort:** Large.
**Origin:** CG-19.

**Work:** Add explicitly chosen snapshot-age retention and application key-provider
integration. Existing bounded receipt expiry, revision-checked snapshot deletion
and atomic live-record key rotation are implemented; do not replace them with a
second store. Define backup handling and key recovery without implying secure
erasure or re-encryption of old backups. Configuration snapshots do not rewind
query data or receipt batches.

**Finish when:** Retention cannot delete a concurrently changed snapshot, expired
requests cannot be reapplied as new work, and unavailable/rotated keys preserve
recoverable ownership across cancellation and restart. Demonstrate namespace
isolation and operator recovery with a real persistent store.

<a id="cw-72"></a>

## 46. Make component capabilities and wrapper delegation explicit (CW-72)

**Kind:** API/design improvement. **Area:** Core component contracts, wrappers, SDK/Host.
**Priority:** Medium. **Effort:** Large. **Review:** remaining M19, M20.
**Origin:** CG-11's remaining native service capabilities.

**Work:** Define which hooks are required, optional or unsupported, and reduce
fragile manual delegation through an appropriate capability/delegation design.
Document asynchronous `Send`/`Sync`, serialization and cancellation obligations.
Any ABI extension must be explicit and versioned rather than inferred from a
trait name or a successful no-op default.

Select further native services by concrete need, including resource injection
and in-place plugin reconfiguration where currently unsupported. Reuse scoped
services rather than pass arbitrary Rust objects or storage pointers across FFI.

`CheckpointedSink` now forwards compatible hooks and checkpoints batched input.
The remaining capability is safe scoped progress deletion and combined
inner/wrapper retirement proof. Until that exists, retain explicit unsupported
results; never forward permanent deletion or a batch past the progress owner.

**Finish when:** A capability matrix and conformance tests show that wrapped and
unwrapped components provide equivalent advertised behavior, including failure,
batch progress, retirement, cancellation and scoped deletion. Unsupported
combinations reject before falsely promising successful cleanup.

<a id="cw-73"></a>

## 47. Finish release and migration documentation (CW-73)

**Kind:** Release preparation/documentation improvement. **Area:** Core, Server and consumers.
**Priority:** High before publishing the replacement. **Effort:** Medium to large.
**Review:** remaining H10, M35, M36, M37.

**Work:** Write one release migration guide covering removed/renamed APIs,
instance-scoped Server configuration/routes, query inputs/results, supported
Server/plugin pairing and explicit backup/reset/rebootstrap procedures for old
state. This is a breaking replacement: do not restore deprecated aliases or
promise automatic conversion of incompatible stored data.

When the matching packages are published, update development-branch navigation
and installation snippets to the supported release. Until then, keep truthful
source-build instructions rather than direct users to crates lacking these APIs.
Preserve commit-pinned evidence where provenance matters. Verify docs.rs builds,
package dependency/publication metadata and documented commands, including the
plural `build-native-network-plugins` target.
One known navigation failure is `docs/README.md` linking to a nonexistent root
`CONTRIBUTING.md`; repair that target as part of this documentation cleanup.

Reduce overlapping contract explanations in usage/configuration/reference
documents by assigning each topic an authoritative home and linking to it.
Keep the developer book's evolving example and existing compilation of all
40 illustrations and 23 exact excerpts; example compilation is not missing work.
Generate versions/changelogs through release-plz, never by hand.

**Finish when:** A reader can build/install the advertised version, choose the
correct configuration and safely replace an old deployment using only the
published guidance. Links, docs.rs/package builds and example checks agree with
the release; design guides contain current behavior, not competing work queues.

<a id="cw-74"></a>

## 48. Evaluate optional pipe throughput policies (CW-74)

**Kind:** Optional performance capability. **Area:** Core pipes and delivery.
**Priority:** Measurement-led future work. **Effort:** Medium to large.
**Origins:** pipe-guide improvement list; CG backlog's optional policies.

**Work:** Evaluate asynchronous rate/burst admission or delivery limits separately
from capacity quotas. If measured workloads need independently completing
in-flight deliveries, design a bounded acknowledgement window that advances
durable progress only through the contiguous completed prefix. Existing batch
receive is not missing, and this work must not silently change its contract.

**Finish when:** For each adopted policy, controlled-clock tests prove the chosen
rate/burst or window bound through cancellation, slow consumers, failed replies
and restart. Ordering, shared QoS accounting and required delivery obligations
remain intact. Reject the proposal if measurements do not justify its complexity.

<a id="cw-75"></a>

## 49. Extend isolated-host platform and network qualification (CW-75)

**Kind:** Optional platform capability and qualification. **Area:** instance-host, bridge, Server.
**Priority:** Required before advertising an additional profile. **Effort:** Large.
**Origins:** execution/isolation bridge, hierarchical-host and prepared-peer phases.

**Work:** Define and implement a Windows hosting/supervision profile if required;
the existing process-host evidence is Unix. Qualify prepared-peer links across
real network partitions and prolonged disconnects, beyond the existing local/VM
tests. Select explicit kernel CPU/memory/process containment where a deployment
needs it; in-process unwind handling cannot contain aborts or unsafe memory faults.

**Finish when:** Each advertised platform/provider profile proves parent,
child and grandchild ownership; parent death; child graph failure while its PID
survives; version/authentication rejection; lost admission receipts; stale
connections; namespaced persistent reopen; and interrupted reset. Measure
resources across the whole process tree. A partition reports unknown remote
state, not a stopped peer or permission to launch an unfenced duplicate.
This adds no remote component-method executor, distributed transaction promise
or automatic remote provisioning.
