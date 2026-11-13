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
Implementation and integration work is grouped separately from
[testing and qualification](#testing-and-qualification). That section owns test
quality, coverage gaps, execution/CI selection, reliability, cost and measured
qualification. Mixed implementation items link to their testing obligations
instead of maintaining a second test checklist.

The [testing guide](../../docs/contributing/contributing-code/contributing-code-tests/)
describes ordinary Cargo/CI execution. The custom profile runners, name
inventories and qualification ledgers have been removed. A passing test does
not qualify every provider, failure schedule or deployment.
The recent reconciliation, controller, partial-start, snapshot-generation,
ownership, rollback and lineage repairs are implemented; their broader
qualification boundaries remain below. Intentional child-worker test ignores
are exercised by their parent tests, not counted as disabled functional failures.

Native query integration is implemented; [CW-43](#cw-43) is closed. Its remaining
transport limit, broader native parity coverage, environment-dependent test
qualification, performance and reference-cleanup work have separate entries
[CW-76](#cw-76) through [CW-80](#cw-80). Query scheduling and the standalone
non-query scheduling profile in [CW-81](#cw-81) are implemented. The mixed
application proof in [CW-82](#cw-82) remains open. The memory-backed view
transform in [CW-68](#cw-68) and native-only reconstruction recipes in
[CW-70](#cw-70) are also implemented within their documented boundaries.

Native conversion of every plugin and old-adapter retirement are **not universal
merge prerequisites**. Supported ordinary adapters already run inside
ComputationGraph. The replacement is an unreleased, breaking v1 design; no
compatibility-shim or automatic old-state migration programme is implied.

| Work area | Main entries |
|---|---|
| Implementation, lifecycle and diagnostics | [Deadlines](#cw-06), [errors](#cw-22), [status delivery](#cw-23), [background ownership](#cw-17) |
| Managed component updates | [Safe rebootstrap coordination](#cw-92), [user-deferred graph-change/live-rebinding design review](#cw-94); state-preserving factory updates and lifecycle ownership are implemented |
| Native named references | [Deferred bind-once endpoints](#cw-93); native topology endpoints currently require assigned UUIDs |
| Testing quality and selection | [Testing overview](#testing-and-qualification), [production-path oracles](#cw-83), [honest runners](#cw-84), [CI selection](#cw-85), [duplication](#cw-86), [behavior ledgers](#cw-88) |
| Recovery and behavior qualification | [Pending input](#cw-14), [reset](#cw-20), [completion evidence](#cw-36), [combined failures](#cw-39), [FFI](#cw-41), [Server](#cw-46), [examples](#cw-90) |
| Query-plugin follow-ups | [Large items](#cw-76), [native parity coverage](#cw-77), [complete test run](#cw-78), [performance](#cw-79), [linked-reference retirement](#cw-80) |
| Integration and applications | [Connectors](#cw-44), [Server/UI](#cw-45), [examples/tutorials](#cw-47), [reconstruction](#cw-70) |
| Scaling and maintainability | [Publication](#cw-31), [storage](#cw-28), [hot paths](#cw-32), [decomposition](#cw-34) |
| Testing reliability and cost | [Asynchronous tests](#cw-37), [build/fixture cost](#cw-87), [shared conformance](#cw-89), [application benchmarks](#cw-40), [native-query comparison](#cw-79) |
| Public API and release | [API curation](#cw-69), [formats](#cw-18), [lints/MSRV](#cw-61), [release documentation](#cw-73) |
| Optional capabilities | [Non-query scheduling](#cw-81), [combined scheduling proof](#cw-82), [view transforms](#cw-68), [housekeeping](#cw-71), [pipe policies](#cw-74), [hosting profiles](#cw-75) |

Keep CW IDs and anchors stable. Review H/M identifiers and historical CG IDs
below identify the remaining portion of a finding, not acceptance of every
claim in its original wording. Record new work here instead of creating another
backlog or a second set of checklists in a reference guide.

## Implementation and integration

<a id="cw-06"></a>

### Lifecycle deadlines and contention (CW-06)

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
Native instance startup now queues generation-fenced member requests behind
reconciliation and cleanup, and joins existing activation instead of returning
a transient conflict. The existing lifecycle queue retains cancellation,
replacement fencing, retirement exclusion and stop supersession.

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

<a id="cw-92"></a>

### Coordinate safe rebootstrap during managed updates (CW-92)

**Kind:** Unsupported capability / follow-on implementation.
**Area:** Core, native SDK/Host, query plugin, Server.
**Priority:** Generalized native graph recovery; basic ordinary-query replacement
does not require this broader protocol. **Effort:** Large.
**Status:** Qualified ordinary-query reconstruction implemented; general reset
coordination remains deferred.

**Current boundary:** The registered ordinary native-query constructor supports
semantic `Recreate/Rebootstrap` and streaming-only `Recreate/Reset` with stable
bindings. Controlled startup completes query-owned rebuilding before the owner
confirms completion and releases activation. Memory and persistent atomic/non-atomic
publication use their actual reset protocols and the accepted operation identity.
Ordinary reaction recovery policies and source/sibling ownership remain independent.
Other factories reject data reset by default; downstream query resynchronization,
live input rebinding and shared resource replacement remain unsupported.

The factory can assess a change and report that reconstruction and rebootstrap
are necessary, but that assessment does not establish a safe graph-wide reset.
For example, changing a query's filter, joins or grouping can invalidate retained
state even when its output columns remain unchanged. Qualified ordinary updates
replace and rebuild that state rather than treating it as compatible.
Supported state-preserving updates still require an explicit factory plan;
initialization/disposal and accepted-update journaling are already implemented.

**Work:** Add a graph-owned execution protocol that can establish all of these
guarantees before discarding state:

1. **Authoritative snapshot and live handover.** Identify the actual input
   snapshot frontier and retain a consistent path to changes arriving after it.
   Provider availability or an empty bootstrap result is not proof.
2. **Exact queued-work treatment.** Identify which admitted work the snapshot
   supersedes, retain or replay later work, and prevent an unconfirmed output
   from being silently dropped. Persist the required execution identity and
   boundaries so retries and process restart cannot reset twice or resume an
   unconfirmed operation.
3. **Downstream reset acknowledgement.** Coordinate affected queries and
   stateful consumers, prove their resynchronization, and preserve shared history
   and other consumers' obligations. A local query reset must not delete data
   owned by another component or a shared recovery domain.

Reuse the existing configuration owner, lifecycle operations, bootstrap/read
services and recovery-domain ownership. Do not add a parallel runtime or assume
that a Boolean plan, the presence of a factory, or `reset_configuration: true`
provides these guarantees. Do not disguise a semantic change as `Preserve`.

**Finish when:** Real native query pipelines exercise both changed output shape
and changed semantics with unchanged columns. Independent result oracles verify
the snapshot/live boundary, retained post-frontier changes, dependent-query and
stateful-consumer resynchronization, and isolation of unaffected consumers.
Cancellation, cleanup failure and process exit before/after reset, bootstrap and
completion acknowledgement must either resume the same accepted operation safely
or leave it visibly blocked without losing ownership or data. Unsupported
provider/consumer combinations must continue to reject before destructive work.

<a id="cw-93"></a>

### Persist and resolve native named endpoints once (CW-93)

**Kind:** Unsupported authoring capability. **Area:** Core, Server, test-infra.
**Priority:** Medium; needed for separate out-of-order native declarations by name.

**Current boundary:** Native topology relationships require component UUIDs.
An edge to an already accepted component belongs in `relationships`, even when
that component was created in another request; `boundary_relationships` is
scope-boundary context, not a way to connect previous creations. A native
consumer can be declared disconnected, then explicitly wired after its producer's
UUID is available. Do not wait for activation before supplying required ports.
Ordinary declarations' pending name references do not provide this native
endpoint-authoring capability.

**Work:** Add a configuration-owner-managed native named-endpoint declaration
that can wait for a later component declaration and bind exactly once to its
assigned UUID. Preserve unresolved intent and the eventual binding across
acceptance, retries and restart. A rename or reuse of the old name must not
retarget a resolved edge. Validate the actual role, port and schema before
publishing the connection.

Do not derive component UUIDs from names, invent placeholder component objects,
enable external IDs as a workaround, or introduce a second mutable graph
registry. Until this contract exists, native helpers requiring named producers
must either require their declarations first or expose explicit two-step wiring.

**Finish when:** Separate consumer-before-producer requests, missing/wrong ports,
duplicate names, rename/name reuse, request retry and restart have public-API
tests proving the original binding cannot silently follow another object.

<a id="cw-94"></a>

### Review managed graph changes and live input rebinding (CW-94)

**Kind:** Design review / disabled functional coverage.
**Area:** Core, native SDK/Host, SSE sink.
**Status:** USER-DEFERRED until the other build and test failures are resolved;
not a fixed defect or an adopted implementation plan.

**Current boundary:** DrasiLib owns managed configuration changes; public raw
graph reconciliation cannot bypass that owner. The specification-only managed
update planner cannot assess changed live input or resource bindings and rejects
them before acceptance. Native update descriptions can carry actual input
bindings, but the current graph-factory assessment path does not supply them,
and the SSE factory does not advertise or implement managed update planning.
Construction-time input binding and a preview's replacement decision do not
constitute author-assessed permission to update a managed component.

**Disabled tests:** Only these two wrappers in
`components/host-sdk/tests/native_network/sse.rs`, executed by the
`native_network_loopback` target, have the explicit ignore reason
`CW-94: deferred pending graph-change design review`:

- `sse_tests::native_sse_declared_inputs_follow_a_live_rebind_on_current_thread`
- `sse_tests::native_sse_declared_inputs_follow_a_live_rebind_on_multi_thread`

Their bodies, correctly typed owner-scoped fixtures, expectations and
linked/loaded plugin loops remain intact. These are deferred functional failures,
not intentional crash-child workers, and their ignored executions provide no
passing coverage of live rebinding.

**Work:** After the remaining build/test failures are resolved, revisit the
user's desired ComputationGraph change design before selecting an implementation.
Review managed acceptance, actual binding/factory assessment, affected
generations, recovery and cleanup together. Do not adopt a temporary graph,
owner bypass, forced replacement strategy or unsupported-error oracle to turn
these tests green.

**Future coverage intent:** Preserve the current test bodies and assertions now
as reference, not a committed replacement or generation strategy. After agreeing
the user's desired managed graph-change design, revisit and re-enable coverage
for affected and unaffected components and actual live streams, retaining both
Tokio flavors and linked/loaded plugin paths. Until then, report this coverage as
deferred rather than claiming complete functional qualification.

<a id="cw-44"></a>
<a id="hosting-connectors-server-and-examples"></a>
<a id="current-required-family-boundary"></a>

### Finish the remaining data connections (CW-44)

**Kind:** Issue/integration and optional stronger profiles. **Area:** Core, Server.
**Priority:** High for required families. **Effort:** Very large.
**Origins:** CG-02/03/07/11/12/24; cleanup stage 6.

**Work:** Complete the selected connectors using their existing connection,
conversion and protocol logic:

| Area | Remaining work |
|---|---|
| Kafka and Kubernetes | Native sources and initial-data providers, independently packaged. |
| SQL Server | Close capture-table permission, large sink-batch and orphan-marker gaps. |
| Database discovery | Populate the remaining family catalogs through the existing native schema reader and Server APIs; factory configuration schemas are not source data schemas. |
| Network profiles | Implement stronger snapshot/progress replacement or destination-backed completion only for profiles that require them. Native gRPC TLS is not advertised; add it before recommending this connector for encrypted gRPC deployments. |
| HTTP ingress authentication | Protect standard single-event, batch and producer-session routes with explicit authentication and per-client request limits. Define non-loopback exposure rules. Webhook authentication alone does not protect these routes. |
| Application components | Move remaining old-interface application/example consumers to the native components. Test-consumer migration is owned by [CW-41](#cw-41) and [CW-77](#cw-77). |

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

Real-service, discovery, security-boundary and static/loaded conformance work
is consolidated in [CW-41](#cw-41), with restart coverage in [CW-14](#cw-14).
SQL Server's user-deferred environment gate remains explicit in [CW-78](#cw-78).

**Finish when:** Every required source, bootstrapper and reaction/sink exists,
and a capability-by-capability comparison finds no omitted working legacy
functionality. Its applicable [component qualification](#cw-41) and
[Server acceptance](#cw-46) must pass; skipped cases and matching counts are not proof.
SQLite has no legacy reaction to port; this work adds no public DrasiLib SQL API.

<a id="cw-45"></a>

### Finish native Server and client workflows (CW-45)

**Kind:** Integration/improvement. **Area:** Server, UI and clients.
**Priority:** High for required workflows. **Effort:** Very large.
**Origins:** CG-04/09/10/14/16; cleanup stages 5-6.

**Work:** Complete creation, editing, discovery, lifecycle and result viewing for
the remaining native components in Server, UI and clients. Make waiting,
accepted, running and failed states clear. Resolve the recorded full VS Code
type-check failures in the component and YAML tooling. The concrete gaps are:

- Route first-store Server seeds through Core's common creation preparation and
  acceptance path. `managed_configuration::configure` still assigns omitted
  UUIDs in the Host before storing the initial `DesiredInstance`. It enforces
  external-ID opt-in and preserves stored UUIDs, but duplicates allocation/policy
  instead of using the normal Core creation receipt and validation path. Keep
  the persistent-builder guard and genuine stored-record restoration; do not
  introduce pre-acceptance component construction or a temporary runtime.
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
  owner-assigned UUIDs, bind-once references and no accidental cross-deployment
  sharing.

Existing API/UI operations do not depend on every connector conversion. Only
workflows requiring a missing family depend on [CW-44](#cw-44); broader
reconstructible recipes belong to [CW-70](#cw-70).

**Finish when:** Required workflows run entirely through supported native
components and current instance-scoped APIs, without old factory registrations
or misleading success states. The stale-preview, acceptance, realization,
receipt and instance-isolation cases are owned by [CW-46](#cw-46).

<a id="cw-43"></a>
<a id="cw-43-optional-native-query-plugin"></a>

### Loadable query support and ordinary application integration (CW-43)

**Status:** Implemented; umbrella closed and retained as a completion anchor.
**Area:** Core, native SDK/Host, Server.
**Origin:** CG-11.

**Design:** [Continuous queries as standard plugin transformers](computation-graph-query-transformer-design.md)
describes the shared implementation and its contracts.
The [plugin guide](../../components/computation-plugins/transformers/query/README.md)
describes current registration and supported profiles.

**Completed implementation:**

| Concern | Implemented behavior |
|---|---|
| Execution and reuse | A normal native Transformer runs the existing Core evaluator. Shared configuration, builder setup, canonical results, publication, recovery and delivery helpers live in `drasi-query-common`; there is no Host evaluator RPC or result-row mirror. |
| Factory and ordinary APIs | Builders, runtime additions and pipelines require explicit factory registration. Empty/source-only instances need none. Server selects the unique eligible loaded factory for configured/API queries; absence is an error, never an implicit linked fallback. |
| Evaluation and lifetime | Cypher/GQL, relationships, joins, aggregates, archives, input-local middleware, atomic source transactions and Core future evaluation use retained evaluator/index adapters. Live input and bootstrap do not rebuild the evaluator per event. |
| Storage and recovery | Memory, persistent atomic and explicitly non-atomic profiles work through actual resource owners. Non-atomic publication preserves its three phases and deferred input visibility. SQLite bootstrap, query chains, shared/separate journals, restart, replacement and deletion are integrated. |
| Reset safety | Identified corrupt rows/output use shared AutoReset eligibility and ownership checks. Reset baselines retain logical and transport high-water marks separately. Foreign ownership, unknown storage failures, pending delivery and bound destinations cannot be bypassed to force reset. |
| Results and diagnostics | Ordinary snapshots/history, subscriptions, reactions and paged native reads use canonical state. Output gauges, logical advance counters, operation timing and committed progress are wired; replay/no-op input does not invent new output. |
| Cleanup and recovery readers | Retained recovery views require completed plugin and Host cleanup. Ordinary stopped reads remain fenced. Consumer recovery reuses existing policies and checkpointed sinks; abandoned transfers release provider work without requiring a follow lease. |
| Callers and architecture | All 19 standalone ordinary-query examples were migrated to explicit native registration and compile-checked. Matching native libraries and Server were built; dependency isolation, scoped lint/format checks and generated architecture checks passed. |

**Qualification:** Historical and current execution evidence is consolidated in
[CW-78](#cw-78); native behavioral coverage and its limits are in
[CW-77](#cw-77). Neither a successful build nor this implementation closure
establishes exhaustive coverage.

**Follow-up ownership:** [CW-76](#cw-76) covers oversized individual items;
[CW-77](#cw-77) covers missing native parity cases; [CW-78](#cw-78) covers the
complete current-code test run; [CW-79](#cw-79) covers performance comparison;
[CW-80](#cw-80) covers remaining linked-reference entry points. These are
bounded follow-ups, not an unfinished evaluator/factory implementation.
Architecture regeneration is complete, not an open work item.

The general non-query scheduler and former combined F0 proof are
[CW-81](#cw-81) and [CW-82](#cw-82), not missing query timers. Server composition
remains [CW-45](#cw-45), reconstructible recipes [CW-70](#cw-70), and release
packaging/documentation [CW-73](#cw-73). This closure is not a claim of exhaustive
test coverage, unlimited payloads or performance parity.

<a id="cw-47"></a>

### Finish required examples and normal builds (CW-47)

**Kind:** Issue/integration. **Area:** Core, Server, Learning, test-infra.
**Priority:** High for required examples. **Effort:** Large overall.
**Origins:** CG-17/28; cleanup stages 6/9.

**Work:** Implement the GPU Lab deployment using the standard Server and finish
examples for the remaining selected connectors. The stock-Server GPU directory
is still a reserved deployment, not the implemented embedded lab. Reuse its
shared domain/plugin/UI code.

**Implemented build repair:** The older copied Learning Trading frontend now
passes `tsc && vite build` after removing its unused `createdReaction` flag.
Compiler checks and cleanup behavior are unchanged. The runnable tutorial
continues to use the sibling Server application, not that older copy.

**Completed query caller migration:** All 19 standalone Core ordinary-query
examples register the native query factory. Nine checked-in PostgreSQL, MySQL
and Trading configurations
were regenerated from the actual native query factory; matching build and
startup plugin lists include the query library. [CW-78](#cw-78) records the
execution evidence. Library-owned low-level reference examples remain explicit
baselines until [CW-80](#cw-80). Neither example compilation nor passing Server
units substitutes for live application acceptance.

All six Learning Server bundles explicitly include the native query plugin and
its selected middleware features. Native plugin registration also retains the
query-authoring facade identities needed to restore generated definitions; it
does not select an implicit ordinary-query default. The shared local builder
keeps exact per-plugin artifacts rather than relying on a global cache.

Live walkthroughs, readiness/reset/restart checks and the distinction between
earlier evidence and current acceptance are consolidated in [CW-90](#cw-90).
This item owns missing example implementations and packaging, not a duplicate
application test matrix.

**Finish when:** Required examples build normally and run against the supported
Server with clear setup, shutdown and recovery behavior, as verified by
[CW-90](#cw-90).

<a id="cw-18"></a>

### Keep saved data and retries unambiguous (CW-18)

**Kind:** Format/release improvement. **Area:** Core contracts, storage, SDK/Host.
**Priority:** High. **Effort:** Medium to large. **Review:** M28.

**Work:** Audit current saved and transmitted formats. Define which fields
identify a request, result or configuration. Formatting or diagnostic changes
must not turn a retry into new work. Unsupported or damaged data must fail clearly.

Dispatch persisted/wire records from their version/header before decoding the
current payload layout, so unsupported versions are distinguished from corrupt
current records. State the current-format
upgrade/refusal/reset policy; diagnostics and formatting are not identity fields.
Golden bytes, identity and malformed-record checks are consolidated in
[CW-41](#cw-41).

**Finish when:** Current-format restart and retries preserve identity, and
incompatible records are rejected without silently resetting valid state.

<a id="cw-19"></a>

### Reject excessively deep data safely (CW-19)

**Kind:** Hardening. **Area:** Core contracts, codecs, providers.
**Priority:** High where input could terminate the process. **Effort:** Large.

**Work:** Audit supported decoders and internally constructed values beyond the
already bounded paths. Apply appropriate size and nesting limits, including safe
release of deeply nested values after an error or cancellation.

**Implemented:** Graph publication, JSON/binary codecs and encoded-size accounting
enforce a 1,024-entry lineage bound. Shared lineage destruction is iterative,
and diagnostic formatting does not recursively traverse ancestry. The broader
deep-value audit remains distinct from this envelope-lineage boundary.
Decoder/drop/allocation qualification is consolidated in [CW-41](#cw-41).

**Finish when:** Supported malformed-input cases fail with an error rather than
exhausting the call stack or terminating the process.

<a id="cw-66"></a>

### Retire mitigated legacy dependency paths (CW-66)

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

<a id="cw-22"></a>

### Make errors explain what happened and what to do (CW-22)

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
Query AutoReset now reuses shared typed eligibility and ownership checks
internally. Audit query-facing errors after the native boundary separately:
correct reset refusal is not proof that every plugin-owned Rust cause is
downcastable through the public API or explained clearly in Server/UI output.
Cross-surface error and typed-cause assertions are owned by [CW-10](#cw-10).

**Finish when:** A user can diagnose and recover from the selected failures
without guessing from a generic error or mistaking rejection for successful work.

<a id="cw-23"></a>

### Make live status and log updates reliable (CW-23)

**Kind:** API improvement. **Area:** Core, Server, SDK/Host.
**Priority:** High. **Effort:** Medium to large. **Origins:** CG-05/26; observer review.

**Work:** Complete operational-status and log handover, slow-reader and reconnect
behavior. Detect missed updates and provide a way to obtain current state.
Preserve instance identity and release observer state when its owner is removed.
Document the snapshot/changed acknowledgement contract for progress
observers so clients do not accidentally busy-loop without consuming a snapshot.
Coalesced status is not a full event log. Observer handover, acknowledgement,
callback and reconnect tests are consolidated in [CW-10](#cw-10).

**Finish when:** Readers either receive the required updates or are explicitly
told to refresh, and old readers cannot report a replacement as their old component.

<a id="cw-17"></a>

### Make components stop their own background work (CW-17)

**Kind:** Ownership audit/hardening. **Area:** Core, components and providers.
**Priority:** High. **Effort:** Medium to large.

**Work:** Audit remaining components and providers that create their own tasks,
threads or connections. Establish ownership before starting them and retain it
until their real work has ended, including during failed startup and cancellation.
Include already-submitted blocking storage work and rejected construction
results. The cleanup/leak distinction is verified in [CW-08](#cw-08);
supervised retry policy belongs to [CW-06](#cw-06).

**Finish when:** Removed or stopped components leave no unowned background work
that can affect a replacement or keep its resources unavailable.

<a id="cw-31"></a>

### Avoid rebuilding unchanged information for status updates (CW-31)

**Status:** Implemented; retained anchor, not outstanding work.
**Review:** M6/publication findings. **Origin:** execution-plan E2.

Desired snapshots, observed/registry maps and reports share unchanged data.
Dirty component captures refresh read capabilities without relying solely on
topology revision. Event translation visits changed members and uses immutable
kind lookups; producer updates select only their ordered edge range. Coherent
addition/provider publications, weak ownership, event order and history remain
covered.

The scoped publication measurements and their limits are retained under
[CW-40](#cw-40). See the
[maintainer guide](computation-graph-maintainers-guide.md#inspection-and-diagnosing-a-running-system)
for repeatable diagnostics. Whole-graph exports still cost work proportional
to their output; tombstone reclamation and wider workloads remain in
[CW-33](#cw-33) and [CW-40](#cw-40).

<a id="cw-33"></a>

### Reclaim bookkeeping left by removed components (CW-33)

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

### Finish memory limits for saved queues and results (CW-29)

**Kind:** Capacity improvement. **Area:** Core queues, retained state and providers.
**Priority:** Medium. **Effort:** Large.

**Work:** Add the missing byte limits for shared transactional queues, saved
output and current result rows. Reserve required capacity before processing
commits, and define what happens to an oversized input or a full buffer. Separate
encoded-envelope admission quotas, payload-cache bounds, receipt/progress
metadata, backend buffers and current-result state; none alone bounds total
process memory. Existing optional FIFO/retained/non-shared QoS byte budgets and
paged payload caches are not missing implementations.
Query snapshot/history paging removes whole-response transport caps, not total
result-memory limits. Fragmenting one oversized native item is [CW-76](#cw-76);
do not treat that transport change as completion of this capacity work.

**Finish when:** The selected limits hold through slow consumers, cancellation
and restart without accepting work that cannot be retained safely.

<a id="cw-28"></a>

### Reduce unnecessary disk work and growth (CW-28)

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

### Remove measured repetition from data processing (CW-32)

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
The query evaluator is retained across input operations, and canonical output
observations avoid a per-input metrics RPC or snapshot transfer. Further query
boundary optimizations should follow [CW-79](#cw-79)'s measurements, not reopen
those completed lifetime/read changes.

**Finish when:** Measured processing cost falls while exact output, identity,
replay and failure behavior remain unchanged.

<a id="cw-61"></a>

### Restore useful compiler and code-check warnings (CW-61)

**Status:** Implemented for the selected library/contracts and test-framework
packages; retained anchor, not outstanding work.
**Area:** Core, contracts, Server and test-infra.
**Review:** M37; MSRV/lint findings.

**Implemented:** `drasi-lib`, `drasi-computation-contracts` and
`drasi-query-common` deny unused-code warnings. The test framework enables those
checks in all eight packages, and its normal `make lint-check` runs the whole
workspace once without formatting sibling dependencies. Existing Clippy
restriction-lint scope is preserved rather than silently changing the panic
policy of previously unopted-in framework packages.

Unused imports, private wrappers, the superseded internal reaction query-provider
path and an unused profiling summary are removed. Independent ordering/encoding
oracles and recovery-inspection helpers remain under `cfg(test)`. Shared fixtures
retain documented, local exceptions because different test binaries use different
parts of them. Ownership-retaining fields are preserved. No public functionality,
ordinary plugin support, ABI versions or explicit linked query constructors were
removed; [CW-63](#cw-63) and [CW-80](#cw-80) were not executed as cleanup.

The supported Rust floor is declared as 1.95.0 in all Core, Server and framework
workspace package manifests, matching their existing pinned toolchains. Clippy
derives its minimum from those manifests instead of the obsolete separate 1.70
settings. Component lifecycle, management-store and query-read contracts enforce
public rustdoc and unchecked unwrap/expect restrictions. Unused direct library
dependencies are removed, and the Cypher function-registration dependency used
only by tests is a dev dependency. The earlier removal of 21 unused workspace
dependency aliases remains complete.

**Verification:** Strict default and feature-enabled all-target checks pass for
the selected library/shared packages. Shared contracts, affected graph-management
scenarios, legacy SDK callbacks and the retained legacy fixture are covered.
Previously ignored reconciliation acceptance and nested publisher errors are
checked explicitly. The framework's full affected package run also exposed
missing query-factory registration: embedded instances now use the statically
linked native query plugin through the validated Host ABI, with matching
middleware. All 144 test-run-host unit tests pass, including the five previously
failing native-instance cases. Server still builds against the cleaned-up library.

This is a scoped, enforceable cleanup, not a claim that every workspace crate is
free of unused code or that every platform/minimum-version combination is
qualified. Broader release and feature-matrix qualification remains in
[CW-85](#cw-85).

<a id="cw-63"></a>

### Remove the remaining old plugin support (CW-63)

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
The remaining linked query constructors have their own bounded retirement item,
[CW-80](#cw-80). Ordinary Source/Reaction ABI retirement stays here and is not a
prerequisite for using the native query plugin.

**Finish when:** The selected current interfaces are sufficient, unsupported
libraries/data fail clearly, and required functionality no longer depends on the
retired code.

<a id="cw-34"></a>

### Simplify the remaining management code (CW-34)

**Kind:** Maintainability improvement. **Area:** Core graph/query/pipe management.
**Priority:** Medium. **Effort:** Medium to large. **Review:** M1, M2, M9, M15, M27.

**Work:** Separate component ownership, commands, connections, scheduling and
shutdown where they are still difficult to reason about. Share repeated recovery
logic through small changes alongside the work that needs them. Concrete work:

- Consolidate event/completion dispatch across the main controller and
  reconciliation waits; preserve the repaired resource/peer/control servicing.
- Finish structural-validator consolidation across construction and live
  mutations. Absent dependencies remain valid waiting declarations; the
  generated/property and equivalent-entrypoint checks belong to [CW-10](#cw-10).
- Separate operation groups, reservations, connection/control state and shutdown
  into explicit ownership/state models. Clarify slot/status coherence and share
  relationship-policy predicates instead of adding independent registries.
- Factor remaining duplicated commit/recovery work from the large
  Host-processing/controller/reconciler/management modules. Share receiver,
  acknowledgement and retention-window logic where the contracts actually match.

**Completed query extraction:** Definition/configuration, evaluator setup,
canonical result state/readers, publication markers, recovery validation/reset
eligibility and delivery helpers are shared in `drasi-query-common`. Query
consumers use a common recovery-reader contract rather than a second policy
implementation. Do not schedule that extraction again or move the evaluator
into the Host; this item concerns only remaining duplication and unclear owners.

The specific validation holes and controller failure paths reported by the
review are fixed. These remaining extractions reduce future drift; they are not
substitutes for those fixes or a mandate for another executor.

**Finish when:** Changed areas have clear responsibilities and one authoritative
graph. [API/structural conformance](#cw-10), [combined failures](#cw-39) and
[matched measurements](#cw-40) must protect the refactor. Fewer files or lines
alone is not completion.

<a id="cw-24"></a>

### Let applications control logging and monitoring (CW-24)

**Kind:** Operational capability. **Area:** Core observability and applications.
**Priority:** Medium. **Effort:** Large.

**Work:** Implement the selected application-owned collection, filtering,
export and shutdown controls from the
[monitoring design](https://github.com/drasi-project/design-documents/tree/77b155319fd8160496ad059f6fdcb1e6bc93e43d/drasi-lib/tracing-logging).
Resolve the remaining profiling interfaces and dependency choices.

**Finish when:** Applications can choose the required monitoring level without
Drasi replacing their logging setup or disabling existing logs.

<a id="cw-25"></a>

### Follow a request through processing and delivery (CW-25)

**Kind:** Operational capability. **Area:** Core, plugins, Server and destinations.
**Priority:** Medium. **Effort:** Large.

**Work:** Carry useful request and attempt information across queues, plugins,
network calls and retries. Define how a result relates to several inputs, while
keeping diagnostic information separate from the identity of saved business work.

**Finish when:** Selected requests can be followed through actual completion,
with meaningful timing and no invented duration across a restart.

<a id="cw-26"></a>

### Make measurements consistent across components (CW-26)

**Kind:** Operational capability. **Area:** Core, plugins, Server.
**Priority:** Medium. **Effort:** Large.

**Work:** Implement the agreed measurements for availability, input/output,
completed handling, queue pressure, progress lag, errors and processing time.
Keep names and units consistent, prevent double counting and exclude sensitive data.
Include retained/QoS pressure, earliest-subscriber lag and blocked admission so
intentional lossless backpressure is visible rather than mistaken for a stalled
runtime.

**Implemented query slice:** Query committed-input and produced-output positions
can be read from their actual owners, with lifecycle/output identity and
persistence information. Server has optional OTLP export of those measurements.
`drasi.query.progress.sequence` distinguishes `input.committed` from
`output.produced`; filtered inputs advance the former without inventing output.
The standard-gRPC recovery evidence combines these observations with actual
recorded batches and final snapshots. Remaining work is all-component
instrumentation and qualification of the other profiles, not implementation of
this first exporter or another Server progress endpoint.

Native queries also expose canonical live-row/outbox gauges, retained bounds,
logical output advances, operation durations and snapshot-fetch counts through
the existing library metrics. Initialization includes bootstrap/recovered state;
replay and no-op input do not inflate advances, and unsupported providers do not
report meaningful-looking zeros. The Server OTLP exporter currently exports
query progress positions, not all of these output gauges. Broader metric export,
queue/lag/error instrumentation and consistent all-component coverage remain here.

**Finish when:** Enabled measurements report exact expected counts and timing
boundaries across normal work, retries, batching and failure. Those assertions
are consolidated with [public observation conformance](#cw-10) and
[completion evidence](#cw-36).

<a id="cw-27"></a>

### Export monitoring data without blocking useful work (CW-27)

**Kind:** Operational capability. **Area:** Core, Server, export adapters.
**Priority:** Medium. **Effort:** Large.

**Work:** Provide an optional application-owned export path for logs, measurements
and completed traces to OpenTelemetry services, local Drasi consumers, or both.
Bound waiting data, report dropped records and prevent monitoring its own output.

**Implemented first slice:** Server can own a dedicated OTLP/HTTP JSON query-
progress meter provider without replacing the application's global provider.
Export has a configured HTTP timeout, and cancelled shutdown retains its worker.
The broader logs/traces export and all-component measurements remain open.

**Finish when:** Slow or broken monitoring destinations cannot stall business
processing, and export/flush/shutdown have explicit ownership. Slow-destination
and cancellation proof belongs to [CW-05](#cw-05) and [CW-39](#cw-39).

<a id="cw-68"></a>

### Add a stateful view-transform component (CW-68)

**Status:** Implemented for the memory-backed profile; retained completion anchor.
**Area:** Core/native components.

The [view transformer](../../components/computation-plugins/transformers/view/README.md)
is a distinct `drasi.standard/view` factory with graph-change `in`/`out` ports.
It reuses map/relabel processors on reconstructed complete images, retaining
full input state and the actual emitted node/relationship associations. Filtering,
one-to-many changes, key changes, replacements, patches, property removals/nulls
and deletions retract or replace the recorded outputs. Whole-envelope staging
prevents partial state changes on failure; identity exchanges within a batch work.
Missing patch bases and output collisions reject explicitly. Existing stateless
map/jq components are unchanged.

It exposes coherent output snapshots and volatile producer progress. Same-object
stop/start retains its state and epoch; fresh reconstruction starts empty with a
new epoch. Stopped reads and permanent retirement are fenced. SDK semantics and
the [loaded Host boundary](../../components/host-sdk/tests/native_view.rs) cover
processing, snapshots, lifecycle and deprovisioning.

**Limits:** No durable storage, replay/follow, bootstrap/snapshot input, scheduling,
transaction participation or input deduplication is advertised. Current dataset
and fan-out memory are not capped; there is no growing history log. Faithful views
require full initial inputs and ordered lossless delivery. Broader independent
view-oracle and failure qualification remain in [CW-89](#cw-89) and [CW-39](#cw-39);
the memory-backed implementation is not a claim that those additional profiles
already exist.

<a id="cw-69"></a>

### Curate the public API before a stable release (CW-69)

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

**Finish when:** Representative downstream clients can use the curated
surface and its compatibility policy is explicit; documentation
distinguishes Rust API, plugin ABI, wire and persisted-format promises. Numeric
revision/fencing values are not treated as authorization capabilities.
Downstream compilation, API/semver and publication checks are owned by
[CW-85](#cw-85), with behavioral API conformance in [CW-10](#cw-10).

<a id="cw-70"></a>

### Complete reconstructible component and provider recipes (CW-70)

**Status:** Implemented for native factories/recipes; retained completion anchor.
**Area:** Core, SDK/Host and Server. **Origins:** CG-08/15; reliability W9.

`add_source_from_factory` and `add_reaction_from_factory` accept native
`ComponentAddition` declarations while preserving every existing legacy
object-based API. `ComponentAddition::declaration` can retain a missing factory;
failed unmanaged construction uses the existing retry reconciliation.
`GraphConfigurationSnapshot::reconstructible_topology` exports explicit
component, resource and pipe recipes with their original references, rejecting
opaque external objects/bindings instead of inventing empty settings.

The standard `drasi/source-time-merge` factory supports volatile graph-change
merging and an explicitly owned persistent `sourceTimeMergeStorage` recipe.
Host and Server share pinned implementation/configuration versions and the real
IndexBackend dependency. Provider registrations may be versioned; unversioned
registrations remain supported but cannot satisfy a pinned recipe. Server
defers resource acquisition to graph-owned constructors, retaining cancellation,
failed acquisition and cleanup ownership after declaration.

Clean-instance Core, Host and Server cases cover connections/resource scope,
missing factories/versions/secrets, construction failure, cancellation, retries
and cleanup. The persistent merger's frontier survives reconstruction and routes
a later old event to its late output.

**Limits:** Per the selected scope, there are no legacy-producing factory
constructors. Custom-schema mergers still use direct Rust construction. Durable
merger storage requires an independent persistent atomic provider, not shared
transaction groups or memory. Its canonical recipe namespace does not migrate
older direct-constructor state. Configuration restoration does not rewind
processing data. Broader recipe/failure combinations remain [CW-46](#cw-46).

<a id="cw-71"></a>

### Add optional configuration-store housekeeping integrations (CW-71)

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
recoverable ownership across cancellation and restart. Persistent namespace,
operator-recovery and race checks are consolidated in [CW-07](#cw-07) and
[CW-39](#cw-39).

<a id="cw-72"></a>

### Make component capabilities and wrapper delegation explicit (CW-72)

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
The query plugin's indexed processing, bootstrap-state, scheduling bindings and
output observation/recovery services are implemented and explicitly negotiated.
Generic non-query scheduled work is [CW-81](#cw-81); do not reopen query service
bindings as a blanket missing capability.

`CheckpointedSink` now forwards compatible hooks and checkpoints batched input.
The remaining capability is safe scoped progress deletion and combined
inner/wrapper retirement proof. Until that exists, retain explicit unsupported
results; never forward permanent deletion or a batch past the progress owner.

**Finish when:** Wrapped and unwrapped components provide equivalent advertised
behavior, demonstrated by the capability/delegation matrix in [CW-10](#cw-10).
Unsupported combinations reject before falsely promising successful cleanup.

<a id="cw-73"></a>

### Finish release and migration documentation (CW-73)

**Kind:** Release preparation/documentation improvement. **Area:** Core, Server and consumers.
**Priority:** High before publishing the replacement. **Effort:** Medium to large.
**Review:** remaining H10, M35, M36, M37.

**Work:** Write one release migration guide covering removed/renamed APIs,
instance-scoped Server configuration/routes, query inputs/results, supported
Server/plugin pairing and explicit backup/reset/rebootstrap procedures for old
state. This is a breaking replacement: do not restore deprecated aliases or
promise automatic conversion of incompatible stored data.

Current query-plugin source-build and explicit registration instructions are in
place. The release guide must carry that required registration, supported storage
profiles, per-item transport limits, retained recovery rules and reset-baseline
behavior into the published package/Server instructions. Verify distribution
metadata and matching native library availability rather than assume the local
build is already a published installable plugin.

When the matching packages are published, update development-branch navigation
and installation snippets to the supported release. Until then, keep truthful
source-build instructions rather than direct users to crates lacking these APIs.
Preserve commit-pinned evidence where provenance matters. Docs.rs/package,
dependency/publication metadata and documented-command checks are consolidated
in [CW-85](#cw-85), including the plural `build-native-network-plugins` target.
One known navigation failure is `docs/README.md` linking to a nonexistent root
`CONTRIBUTING.md`; repair that target as part of this documentation cleanup.

Reduce overlapping contract explanations in usage/configuration/reference
documents by assigning each topic an authoritative home and linking to it.
Keep the developer book's evolving example; its existing compilation of all
40 illustrations and 23 exact excerpts is retained evidence in [CW-85](#cw-85),
not missing implementation.
Generate versions/changelogs through release-plz, never by hand.

**Finish when:** A reader can build/install the advertised version, choose the
correct configuration and safely replace an old deployment using only the
published guidance. Links, docs.rs/package builds and example checks agree with
the release; design guides contain current behavior, not competing work queues.

<a id="cw-74"></a>

### Evaluate optional pipe throughput policies (CW-74)

**Kind:** Optional performance capability. **Area:** Core pipes and delivery.
**Priority:** Measurement-led future work. **Effort:** Medium to large.
**Origins:** pipe-guide improvement list; CG backlog's optional policies.

**Work:** Evaluate asynchronous rate/burst admission or delivery limits separately
from capacity quotas. If measured workloads need independently completing
in-flight deliveries, design a bounded acknowledgement window that advances
durable progress only through the contiguous completed prefix. Existing batch
receive is not missing, and this work must not silently change its contract.

**Finish when:** Each adopted policy enforces its chosen rate/burst or window
bound without changing ordering, shared QoS accounting or required delivery
obligations. Controlled-clock/failure cases are owned by [CW-39](#cw-39) and
measurements by [CW-40](#cw-40). Reject unjustified complexity.

<a id="cw-75"></a>

### Extend isolated-host platform and containment capabilities (CW-75)

**Kind:** Optional platform capability. **Area:** instance-host, bridge, Server.
**Priority:** Required before advertising an additional profile. **Effort:** Large.
**Origins:** execution/isolation bridge, hierarchical-host and prepared-peer phases.

**Work:** Define and implement a Windows hosting/supervision profile if required;
the existing process-host evidence is Unix. Select explicit kernel
CPU/memory/process containment where a deployment needs it; in-process unwind
handling cannot contain aborts or unsafe memory faults.

**Finish when:** Each adopted hosting/containment profile implements its ownership
and isolation contract and passes [CW-91](#cw-91)'s platform/network qualification.
This adds no remote component-method executor, distributed transaction promise
or automatic remote provisioning.

<a id="cw-76"></a>

### Transfer oversized native query records and envelopes (CW-76)

**Kind:** Transport capability gap. **Area:** Native contracts, ABI, SDK/Host.
**Priority:** High before claiming parity for large individual payloads.
**Effort:** Medium. **Origin:** CW-43 follow-up.

**Current boundary:** Snapshots, history and snapshot/live attachment are already
paged without a whole-result count or byte cap. However, each record/envelope
must fit the [16 MiB read-page limit](../../components/computation-plugin-abi/src/output_reads.rs);
the separate [64 MiB processing-message limit](../../components/computation-plugin-abi/src/lib.rs)
does not make such an item readable.

**Work:** Add negotiated bounded fragmentation/reassembly for individual items,
reusing the existing codecs and owned transfers. Cover snapshot, replay, recovery
view and follow paths consistently. Preserve schema/identity checks, coherent
snapshot/history heads, page order, cancellation and activation retirement.
Do not just raise all limits or allocate an unbounded second projection.

**Finish when:** An item valid for processing but larger than the old read-page
bound can be read and replayed intact, with stated end-to-end maximum and
allocation bounds. Boundary-size, malformed-fragment and cleanup proof is owned
by [CW-41](#cw-41).
Aggregate queue/result-memory quotas remain [CW-29](#cw-29).

<a id="cw-80"></a>

### Retire the remaining linked query runtime entry points (CW-80)

**Kind:** Breaking-release cleanup. **Area:** Core library, tests and examples.
**Priority:** Medium; after parity evidence. **Effort:** Medium.
**Origin:** CW-43 follow-up. **Depends on:** CW-77 and CW-79.

**Current boundary:** Ordinary query selection is already explicit and has no
automatic linked fallback. `ContinuousQueryFactory` and
`ContinuousQueryTransformer` still exist as explicitly selected reference
constructors, including in low-level examples and preservation tests.

**Work:** Inventory those remaining callers and registrations. Move supported
hosted-query behavior to the selected native factory, then remove the obsolete
library entry points and dependency/build paths whose last user has moved.
Retain the useful Core evaluator, shared implementation and behavior assertions.
Preserve a reproducible pinned comparison baseline before removing executable
reference fixtures; do not delete tests merely to reduce the change size.

**Finish when:** Supported application and test paths no longer need the linked
query runtime, missing native registration still fails explicitly, and current
native behavior coverage remains intact. This does not require deleting the
ordinary Source/Reaction compatibility stack tracked in [CW-63](#cw-63).

<a id="cw-81"></a>

### Expose transaction-owned scheduling to non-query native transformers (CW-81)

**Status:** Implemented and qualified for the standalone atomic-persistent
profile; retained completion anchor.
**Area:** Core, native contracts, SDK/Host. **Origin:** General scheduling work
separated from CW-43.

The negotiated `scheduled_processing` capability extends the existing
processing table without changing base native or legacy ABI layouts.
`NativeProcessingContext` requests, looks up, replaces, cancels, claims and
completes work through the existing revocable transaction mailbox. State,
scheduled requests and retained output commit together; deadline observations
publish only afterward. Incarnation tokens reject stale operations, and a
successful claim must be explicitly completed. No second transaction owner or
scheduler task hierarchy is introduced. Query scheduling reuses the same shared
wakeup implementation and retains its public re-exports and behavior.

The real [delayed-emission fixture](../../components/host-sdk/tests/fixtures/native_scheduling.rs)
and [standalone Host suite](../../components/host-sdk/tests/native_scheduling.rs)
cover controlled clocks, rollback, replacement/cancellation, due work without
input, loaded/static paths and real subprocess exits around request/completion
commits. Both Tokio runtime flavors are covered.

**Limits:** The qualified provider is atomic persistent RocksDB with persistent
checkpoints and declared process-restart durability. No-op memory and non-atomic
bundles reject rather than gain stronger guarantees. Bounds are 256-byte keys,
64-KiB payloads, 4,096 outstanding requests and 1 MiB of encoded scheduling state.
One committed completion does not mean exactly-once external delivery.
The mixed SQLite/query/non-query application remains [CW-82](#cw-82).

<a id="testing-and-qualification"></a>

## Testing and qualification

**Keep useful coverage; do not optimize for a smaller test count.** Recovery,
wire-format, shared semantic, real-browser and clean-package tests protect
different contracts. Backend, Tokio-flavor and static/loaded permutations are
not automatically redundant. The first improvements are stronger production-path
oracles, honest runners, explicit CI selection and removal of proven repeated
execution, followed by measured build/fixture improvements.

This section consolidates the full substantive test-suite review and existing
test/qualification work. The review was read-only across all four repositories;
it used source, checked-in CI and existing logs, not new executions or mutation
testing. Counts and scope are recorded under [audit evidence](#testing-audit-evidence)
and [CW-78](#cw-78). Passing execution is not proof of assertion quality.
Source citations identify repository-root-relative files; evidence log names
refer to the recorded `correctness-baseline-final/...` session artifacts, not
checked-in source files.

### Priorities and work ownership

| Priority | Consolidated work | Benefit | Effort | Confidence / risk |
|---|---|---|---|---|
| 1 | [Production-path credential tests](#cw-83) | Protect currently untested production request/error paths | Medium | High confidence; low risk with test-only injection and retained URL validation |
| 1 | [Honest test runners and smoke checks](#cw-84) | Stop false or misleading "all tests" results | Small-medium | High confidence; retain an actual stock-binary check |
| 1 | [Explicit execution of existing suites](#cw-85) | More meaningful coverage without inventing tests | Medium | High confidence in selection gaps; fixtures/features/platforms need care |
| 2 | [Proven duplicate execution](#cw-86) | Simpler selection and less repeated work | Small-medium | High confidence; verify surviving discovery and obligations |
| 2 | [Semantic/backend wiring](#cw-89), [native deltas](#cw-77), [tutorial result oracles](#cw-90) | Better output and storage regressions | Medium | High confidence in stated gaps; preserve unsupported boundaries |
| 2 | [Build and database-fixture cost](#cw-87), [asynchronous reliability](#cw-37) | Potentially largest speed benefit | Medium-large | Measure first; unsafe sharing can mask cleanup/restart defects |
| 3 | [Behavior-based evidence ledgers](#cw-88) | Lower maintenance without losing migration obligations | Small-medium | High confidence; name removal alone must not count as preserved coverage |

**First implementation batch:** replace copied Key Vault test paths, establish
one honest Server runner, relocate the two duplicated helper self-test groups,
bind already-existing cheap suites to CI, and strengthen the current
getting-started result assertions. Make each independently reviewable. Measure
build/database phases before pooling fixtures or changing serialization. These
are planned changes, not work completed by the audit.

### Test quality, selection and evidence

<a id="cw-83"></a>

#### Test real credential-provider behavior instead of copied implementations (CW-83)

**Kind:** Confirmed test-quality defect. **Area:** Core identity and secret stores.
**Priority:** High. **Effort:** Medium. **Confidence:** High; candidate bodies inspected.

Production `AzureKeyVaultSecretStoreProvider::get_secret` obtains a token and
performs the HTTP request. Six tests instead call a separate
`TestableProvider::get_secret` that copies URL, request, status and JSON logic:
`test_get_secret_success`, `test_get_secret_404_not_found`,
`test_get_secret_403_forbidden`, `test_get_secret_null_value`,
`test_get_secret_malformed_json`, and `test_url_encoding_prevents_path_traversal`.
They do not protect changes to the corresponding production path. The ignored
live-provider test checks only that a secret is nonempty.

The URL-encoding test is weaker still: it expects a 404 and checks the error
text, but does not require its mock route to match. Wiremock also returns 404
for an unmatched request, so the intended escaped path need not be observed.

**Work:** Keep the scenarios but construct the actual provider in its test
module with a fake `TokenCredential` and controlled HTTP client/endpoint. Assert
the requested scope, bearer token, exact path/query, matched request count,
returned value and appropriate error/cause. Keep production URL validation
separately tested; do not relax it to accommodate the fixture. Delete
`TestableProvider` only once these replacements protect the real path.

Also repair `test_with_workload_identity_creates_provider`, which discards the
constructor's `Result` and therefore proves only "does not panic." Make its
environment dependency explicit. Existing Azure identity tests check stored
scope fields, not the actual `CredentialContext` override used by
`get_credentials`; add a fake-credential assertion of the requested scope and
returned username/token. Preserve legitimate Send/Sync and trait-object compile
contracts.

**Finish when:** A targeted mutation of production request/path/status handling
breaks the replacements, exact requests are observed, and no copied provider
implementation remains as the behavioral oracle.

**Evidence:** `drasi-core/components/secret_stores/azure-keyvault/src/lib.rs:182-225,280-333,458-607`;
`drasi-core/components/identity/azure/src/provider.rs:133-154,189-194,230-304`.

<a id="cw-84"></a>

#### Make test entrypoints and smoke results honest (CW-84)

**Kind:** Confirmed runner defects. **Area:** Server, Learning and performance tooling.
**Priority:** High. **Effort:** Small-medium. **Confidence:** High, except the conditional performance-launcher consolidation.

| Existing entrypoint | Confirmed problem | Required change |
|---|---|---|
| `drasi-server/tests/run_all.sh:53-91` | All four scripts it conditionally seeks are absent; `FAILED=0` can print "All tests passed" after zero execution. | Retire it or forward to one owned, documented entrypoint with nonempty selection and execution checks. |
| `drasi-server/tests/run_all_cargo_tests.sh:6,25-67` | Under `set -e`, the first successful `((PASSED++))` returns a failing status. It names absent `api`, `api_create_query_joins` and `library_integration` targets, then repeats selected work with broad `cargo test`. | Replace the obsolete target list/aggregator rather than preserve misleading success/count logic. |
| `drasi-server/tests/plugin_smoke_test.sh:124-160,176-201,281-315,370-397` | Generated config uses removed root `id/sources/queries/reactions`; `make build-dynamic` is absent; `--skip-build` maps static/dynamic labels to the same binary; arbitrary non-2xx creation responses become skips. | Use current `instances` configuration, one identified artifact set, explicit required plugins and failure on unexpected HTTP responses. Keep genuine external-prerequisite outcomes distinct. |
| `drasi-server/Makefile:191-211` and `CLAUDE.md:44-51,381-385` | `make test-all` calls that smoke script and promises VS Code execution it does not perform; the guidance names absent `tests/run_working_tests.sh`. | Align commands, documentation and actual selected suites. The successful Cargo baseline is not proof that this aggregate command works. |
| `learning-drasi-server/tutorials/trading/app/package.json:30` | `npm test` is a successful `echo 'No tests configured yet'`. | Remove the success-shaped no-op or replace it with real assertions; do not attribute the separate Server Trading suite to this app. |

Preserve an actual stock-binary test rather than substituting only Router tests.
`single_runtime_test::native_binary_loads_plugins_and_processes_http_events`
already loads independent HTTP/log libraries and checks exact query rows and
reaction output (`drasi-server/tests/single_runtime_test.rs:658-827`). Its
"native" label identifies the graph runtime, not the HTTP/log plugin ABI family.
Current `DrasiServerConfig` rejects unknown root fields
(`drasi-server/src/config/types.rs:92-164`).

**Conditional candidate:** `drasi-core/query-perf/run_perf_tests` is a legacy
diagnostic launcher, not a qualification gate. It invokes development-profile
`cargo run` 12 times; the program discards output correctness, and an unknown
scenario exits successfully (`query-perf/src/main.rs:49-59,187-212`).
Do not delete it until [CW-40](#cw-40) maps and preserves its four scenarios and
memory/RocksDB/Redis distinctions. Confidence in consolidation benefit is
medium; measure first.

**Finish when:** Each advertised runner executes its declared scope, fails on
empty/unexpected selection and real regressions, and reports unavailable
prerequisites separately. Obsolete paths are removed or intentionally forwarded,
with their useful coverage preserved.

<a id="cw-85"></a>

#### Bind existing suites to explicit CI and release gates (CW-85)

**Kind:** Confirmed checked-in selection gaps and build qualification.
**Area:** Core, Server, Learning, test-infra and CI.
**Priority:** High. **Effort:** Medium. **Confidence:** High for checked-in selection; remote branch protection was not inspected.

| Surface | Existing gap and evidence |
|---|---|
| Core Host SDK | Selection repaired: `make test` runs all Host SDK targets with `test-support,registry,fetcher,watcher`, after shared plugin preparation. Passing selection is not a claim that existing identity/lifecycle defects are fixed. |
| Native MySQL and PostgreSQL/HTTP | Discovery repaired: the existing integration matrix follows `#[path]` modules and recognizes Podman/native-library fixtures. CI provisions the CLI and matched plugins. Explicit SQL Server credentials and prepared cross-OS peers remain outside automatically provisioned jobs. |
| Server and UI | Primary Rust CI runs `cargo test --locked`, not ignored stock-binary/E2E cases or `make test-all`. The separate React workflow does not select every control-UI, VS Code, GPU/Move-a-wall workspace or connector qualification suite. See `drasi-server/.github/workflows/test.yml:1-13`, `.github/workflows/react-package.yml:1-45,175-238` and the respective package scripts. |
| Learning | Deterministic tutorial unit/CLI tests are not explicit CI jobs. Scheduled/same-repository PR evaluation selects getting-started only and accepts an agent-written `## STATUS: SUCCESS`; fork PRs cannot run that secret-dependent job. Keep exploratory evaluation, but not as the only oracle. See `learning-drasi-server/.github/workflows/tutorial-evaluation-scheduled.yml:17-40,64-96,180-218`. |
| Test infrastructure | PR jobs build images and lint; no general `cargo test` PR job was found. The manual completed-recovery workflow runs Python completion-checker tests, not all framework units. See `test-infra/.github/workflows/build.yml:17-31,40-87`, `.github/workflows/lint.yml:17-28`, `.github/workflows/e2e-building-comfort-completion.yml:3-13,55-59`. |

**Work:** Bind useful existing tests before adding more. Give cheap
protocol/metadata/SQLite/secret-store Host tests and optional-feature units
owned jobs; put real database tests in declared environment jobs. Add existing
control-UI, tutorial and test-infrastructure units to path-appropriate PR gates.
Fix discovery with a small explicit exception/target list or manifest/path-aware
discovery, not a replacement framework. "Not selected" and "not available" must
not become passing outcomes.

For [CW-61](#cw-61), execute the declared minimum Rust version and supported
feature combinations; a Clippy setting is not build evidence. For
[CW-69](#cw-69) and [CW-73](#cw-73), bind downstream compilation, API/semver,
docs.rs/package builds, publication/dependency metadata and documented commands.
Keep the developer book's existing 40 illustration and 23 exact-excerpt checks,
including compilation of a copied starter
(`drasi-core/lib/docs/developer-guide/check_book.py:23-48,87-131`).
Actual release installation/library availability remains a prerequisite, not
something local source compilation proves.

**Proposed schedule using existing selectors:** This is the intended policy,
not a claim that every row is already wired into CI. Prepare matching plugins
and declared environments once; never run crash workers independently.

| Gate | Existing selectors | Selection rule |
|---|---|---|
| Fast PR: engine/data | `cargo test -p drasi-core-types -p drasi-core -p drasi-query-cypher -p drasi-query-gql -p drasi-computation-contracts`; `cargo test -p shared-tests in_memory::` | Literal semantics and shared memory conformance; add `drasi-core --features parallel_solver` for solver changes. |
| Fast PR: graph/query/SDK | `cargo test --locked -p drasi-lib --lib --tests`; `make build-test-plugins` then focused `cargo test --locked -p drasi-host-sdk --test native_query_plugin <filter>` | Directly affected tests, not overlapping wrapper groups. Linked reference tests are not loaded-plugin proof. |
| PR: ABI/loaded artifacts | `cargo test -p drasi-host-sdk --features test-support --test native_query_plugin`; pipeline/loopback/wire and explicitly selected additional Host targets | Preserve both runtimes and static/loaded permutations; give registry/fetcher/watcher units a feature job and keep cross-platform FFI distinct. |
| PR: Server | `make test-tooling`; `cargo test --locked`; explicit ignored stock-binary case in `single_runtime_test` with `DRASI_NATIVE_TEST_PLUGINS` | Repair [CW-84](#cw-84)'s aggregate claims; use the documented complete Cargo invocation on an environment-qualified broader job. |
| PR: JavaScript/UI | Existing React/Trading packed/browser commands; `npm --prefix ui test`; VS Code `npm test`; affected GPU/Move-a-wall package/workspace tests | Bind currently unselected tests. Keep live-server and deterministic fake-transport evidence separate; parallelize heavy browser jobs rather than hide failures with retries. |
| PR: tutorials/test-infra | Per-directory `python3 -m unittest discover -s <tutorial>/scripts -p 'test_*.py'`; root `scripts` tests; CLI with `DRASI_SSE_CLI`; `cargo test --manifest-path e2e-test-framework/Cargo.toml`; standalone data-tools manifests; comparator/checker Python tests | Tutorial directories need separate processes where module names collide. Agent-written tutorial success is supplementary. |
| Broader/nightly | Explicit feature commands in `test.yml`; discovered ignored-container targets; Building Comfort completion workflow; `verify_demo.py --exercise` | Full capability/backend/fault and bootstrap/live/restart evidence, with only proven identical execution removed under [CW-86](#cw-86). |
| Release/platform | `make test-ffi-layout-mismatch` on actual nightly/Linux; explicitly configured real SQL Server and cross-OS peers; independently built ABI fixtures | Missing prerequisites remain unqualified. Controlled long-running hosts for performance/soaks, not contention beside the main PR gate. |

Changed transaction/ownership code still needs targeted deterministic fault and
crash parents on the PR; do not move all recovery coverage to nightly. Conversely,
workspace tests and overlapping focused selections must not be chained
as if each adds wholly new coverage.

**Finish when:** Each required suite has an explicit owner, selector, feature/
platform requirement and CI schedule, and recorded execution distinguishes
selected, passed, blocked and omitted work. Preserve [CW-78](#cw-78)'s
environment boundaries and [CW-88](#cw-88)'s evidence semantics.

<a id="cw-86"></a>

#### Consolidate proven repeated test execution (CW-86)

**Kind:** Confirmed redundancy, not blanket test deletion.
**Area:** Core, Server and test selection. **Priority:** Medium.
**Effort:** Small-medium. **Confidence:** High for the inspected candidates.

| Candidate | Consolidation and surviving coverage | Loss/risk and required proof |
|---|---|---|
| Five Server fixture tests | `test_create_test_solution_template`, `test_template_with_variables_has_variables`, `test_scriptfile_bootstrap_template`, `test_multi_source_join_template_has_joins`, `test_create_jsonl_file` are included in 18 binaries: 90 executions. Move the self-tests to one target and retain all actual API/dataflow consumers. Prefer parsed YAML/JSONL to string-presence assertions. | Removes 85 repeated executions without losing the five assertions. Verify one discovered copy of each. Evidence: `drasi-server/tests/test_support/solution_helpers.rs:557-609`, `test_support/mod.rs:17-22`, and final `server-rust.log`. |
| Four MySQL helper tests | `test_setup_mysql`, `test_create_stored_procedure`, `test_stored_procedure_operations`, `test_clear_procedure_log` run in the standalone helper target and both storedproc/routing targets: 12 executions of four bodies. Move self-tests out of the imported module and run once. | Removes eight duplicate executions; retain the SQL behaviors and distinct reaction/routing checks. Do not share mutable SQL state merely to reduce count. Evidence: `drasi-core/components/reactions/storedproc-mysql/tests/mysql_helpers.rs:378-482`; final `workspace.log:5869-5923`. |
| Identical `default` / `no-default-features` batch | `drasi-lib` currently has `default = []`; both final listings contain the same 74 executable paths/hashes and 1,826 discovered addresses. Execute one identical complete batch while retaining an explicit minimal-feature policy/check. | Avoids repeating 1,819 passes and seven permitted ignores. Expand the matrix again when defaults differ. This does not justify removing `extra-capabilities`, `parallel_solver`, backend or Tokio-flavor coverage. Evidence: `drasi-core/lib/Cargo.toml:35-40` and final default/no-default discovery logs. |

Copied provider tests are replaced under [CW-83](#cw-83); no-op and obsolete
runners under [CW-84](#cw-84). They are not separate removal tasks here.
Parameter tables, shared scenarios, compile contracts and static/loaded
boundaries must be assessed by the failures they detect, not their size.

**Finish when:** Discovery proves the useful assertions still run once in the
intended scope, preserved consumers remain selected, and supported permutations
have not been mistaken for duplication. A count reduction alone is not success.

<a id="cw-88"></a>

#### Keep evidence tied to behavior rather than permanent test-name floors (CW-88)

**Kind:** Test-maintenance and evidence-policy improvement.
**Area:** Core, Server, test-infra and CI. **Priority:** Medium.
**Effort:** Small-medium. **Confidence:** High; protect migration obligations.

The custom original-name inventories, mappings, qualification checkers and
calendar-based exception gates have been removed. Cargo now discovers the
workspace and Host SDK tests directly; supported feature and service jobs are
visible in the ordinary workflows.

**Work:** Review the asserted outcome, construction, backend and failure
boundary when renaming or consolidating tests. Keep crash parent drivers and
explicit service prerequisites. Do not replace the removed machinery with
another permanent test-count floor or profile-list framework.

Keep successful execution and broad replacement qualification distinct;
[CW-36](#cw-36) owns proof of completed processing/delivery and
[CW-78](#cw-78) owns the recorded execution baseline.

**Execution:** See `Makefile`, `.github/workflows/test.yml` and the
[testing guide](../../docs/contributing/contributing-code/contributing-code-tests/).

**Finish when:** Important behaviors remain traceably enforced while obsolete
names/wrappers can be consolidated safely. Coverage declarations cannot be
satisfied merely by a passing nearby test or a larger count.

### Test reliability and execution cost

<a id="cw-37"></a>

#### Make asynchronous tests reliable (CW-37)

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

Concrete fixture consolidation and unnecessary test-binary build/startup
overhead are owned by [CW-86](#cw-86) and [CW-87](#cw-87), which record the
concrete duplication/cost findings. Preserve required process isolation,
especially global plugin callbacks, crash workers and resource probes.

The SSE restart fixture now checks a new client/new listener, complete old-stream
cleanup, route bounds and listener release. It does not prove that a
pre-restart pooled HTTP client reconnects transparently; retain or add that as a
separately specified client/transport contract if required. Negative observation
windows and external watchdogs are not interchangeable with avoidable sleeps.

**Finish when:** Selected tests reliably prove the intended behavior under load
and on both supported runtime styles, without weakened assertions or inflated timeouts.

**SSE evidence:** `drasi-core/components/host-sdk/tests/native_network/sse.rs:138-214`.

<a id="cw-87"></a>

#### Measure and reduce build churn and fixture overhead (CW-87)

**Kind:** Measured testing-cost improvement. **Area:** Core, Server and CI.
**Priority:** Medium; potentially the largest speed benefit. **Effort:** Medium-large.
**Confidence:** High in recorded costs; proposed speedups need measurement.

The recorded local run used a shared target, serial builds, no incremental/debug
information, real fixtures and changing feature graphs. The test-time column
sums libtest target wall time, including fixture setup/cleanup but excluding
prior Cargo compilation. These are not individual-test CPU times or universal
CI performance figures.

| Recorded phase | Cargo-reported build time | Summed target test time |
|---|---:|---:|
| Core workspace | 464 s | 793.66 s |
| Runtime matrix discovery | 2,861.92 s | Discovery only |
| Runtime matrix execution | 52.06 s | 2,221.05 s |
| Complete Host run before fixture follow-up | 226 s | 1,331.55 s |
| Complete Server Rust run | 220 s | 104.14 s |
| Library/Host documentation | 179 s | 29.47 s |
| Corrected complete network target | 1.57 s | 51.68 s |

The network log also contains a separate 110-second development check, not
110 seconds of tests. Fixture builds were separate: `fixtures.log:63-144`
records repeated 82-86-second builds and later 141/118-second builds. Cache
repair is not normal test execution. Sources are the `Finished` and
`test result` lines in the final baseline logs and all 36 listing/execution logs.

**Work, in order:**

1. Stabilize build graphs before micro-optimizing bodies. Runtime discovery
   compiled `drasi-lib` and `drasi-plugin-sdk` 22 times each. Listing is not
   intrinsically slow; successive package/feature selections trigger compilation.
   Group compatible builds, keep feature choices stable and separate incompatible
   host/plugin/qualification targets. Preserve artifact provenance; indiscriminate
   cache deletion is not an optimization.
2. Measure database start, readiness, assertions and cleanup separately.
   `native_mysql_components` took 1,020.87 seconds for 31 reported cases.
   `mysql-lifecycle` took 743.70 seconds: 586.13 in its 98-case library target and
   157.57 in its ten-case integration target. Existing logs cannot attribute a
   percentage to startup or justify an invented speedup.
3. Implement [CW-86](#cw-86)'s proven repeated-execution removals first.
   Identical no-default execution consumed 37.34 seconds of target time; the
   independent MySQL helper target took 33.01 seconds. Per-helper times in the
   other targets were not recorded, so do not extrapolate a saving.
4. Trial an owned shared service plus independent per-test databases only for
   non-restart cases. Garnet already uses a process-level Redis fixture with
   UUID scopes and fresh connections. Keep dedicated containers/volumes for
   restart, binlog retention, corruption, credentials and ownership. Verify
   shuffled/concurrent execution and cleanup before removing serialization.

Record per-test/fixture timings, cold/warm caches and stable feature-group
comparisons. Coordinate clock/isolation work with [CW-37](#cw-37). Unnecessary
getting-started sleeps are addressed in [CW-90](#cw-90); negative blocking
windows and watchdogs must not be shortened indiscriminately.

**Evidence:** Final `host-sdk.log:3535`,
`runtime/mysql-lifecycle.log:1039-1055`;
`drasi-core/components/indexes/garnet/tests/scenario_tests.rs:36-64,92-128`.
Preserve output-equivalence checks before throughput comparisons, as already
tested in
`test-infra/e2e-test-framework/examples/building_comfort/local/native_comparison/test_run_comparison.py:99-124`.

**Finish when:** Matched measurements show shorter feedback cycles without
lost coverage, weaker isolation, hidden cleanup failures or changed result
oracles. Application performance qualification remains [CW-40](#cw-40) and
[CW-79](#cw-79), not this test-runner cost item.

### Semantic, application and API coverage

<a id="cw-89"></a>

#### Restore shared semantic and backend conformance wiring (CW-89)

**Kind:** Confirmed coverage wiring gaps and semantic-oracle qualification.
**Area:** Core evaluator, shared tests, indexes and stateful view transforms.
**Priority:** Medium-high. **Effort:** Medium. **Confidence:** High for named gaps.

`truefor_grouped_sum` and `sliding_window_avg_grouped` are wired in memory but
absent from both persistent scenario consumers. `prev_unique_with_match` is
present in RocksDB but absent in Garnet. This violates the existing "wire all
three" convention; add supported cases or record an actual unsupported contract,
not a raw count comparison.

**Evidence:** `drasi-core/shared-tests/AGENTS.md:5-9`,
`shared-tests/src/in_memory/mod.rs:599-653`,
`components/indexes/rocksdb/tests/scenario_tests.rs:460-476`,
`components/indexes/garnet/tests/scenario_tests.rs:513-521` (all paths in Core).

Keep the shared scenarios and real backend distinctions. The duplicate-named
curbside scenario uses a new vehicle identity with a reused license plate and
is not a duplicate body
(`drasi-core/shared-tests/src/use_cases/curbside_pickup/mod.rs:296-460`).
The Garnet fixture runs Redis 7 for backend protocol conformance
(`drasi-core/shared-tests/src/redis_helpers.rs:52-70`); it does not by itself
qualify a different service's durability.

For [CW-68](#cw-68)'s implemented view component, use an independently recomputed
view oracle after patches (removed properties and explicit nulls), replacement,
filtering, relationship changes and one-to-many identity changes. Preserve
stateless map/jq behavior; [CW-39](#cw-39) owns its failure/restart matrix.
Native-query emitted-delta gaps are consolidated in [CW-77](#cw-77), not cloned
as new shared-scenario copies.

**Finish when:** Each supported shared scenario is selected for every applicable
consumer/backend, unsupported cases are explicit, and literal/independent
oracles catch semantic errors rather than reproduce the implementation.

<a id="cw-77"></a>

#### Complete query behavior coverage through the loaded plugin (CW-77)

**Kind:** Behavioral-parity qualification. **Area:** Core, Host SDK, test fixtures.
**Priority:** High before claiming complete native behavioral parity.
**Effort:** Medium to large, split by behavior. **Origin:** CW-43 follow-up.

**Status:** Native correctness baseline established; exhaustive behavioral
parity remains a qualification item.

**Historical coverage:** The native-query Rust target distinguishes linked and
loaded construction modes. The previously recorded plugin suite passed 176
cases; six additional entries are subprocess fixtures exercised by their
required parents, not ordinary standalone tests.

Eight semantic scenarios now share exact assertions between the retained
reference tests and static/loaded native query construction on memory/RocksDB
and both Tokio flavors. These are 64 native scenario executions inside two
Rust test cases, not 64 additional independently discovered tests. They cover
optional/disconnected contexts, lists/maps, numeric comparisons, optional joins,
canonical replacement and ordered mutations.

**Confirmed oracle gap:** Six of the eight language/scenario variants check
their per-step snapshots but not emitted deltas; canonical replacement and
ordered batch already check both. Extend the existing shared `Step` expectations
for optional/disconnected and aggregation transitions rather than duplicate the
scenarios. Selecting a persistent provider here is not a restart test.
Evidence: `drasi-core/lib/tests/fixtures/query_semantics.rs:76-145,246-435` and
`drasi-core/components/host-sdk/tests/native_query_plugin/semantics.rs:10-99`.

Named native cases also cover ordinary creation/read/subscription APIs, retained
evaluation, bootstrap and reset, independent non-atomic writers, durable delivery
and shared journals, scheduling, archives, middleware and large paged reads.
Native/composite bootstrap ownership now accepts the actual native query
factory while preserving role, owner, slot and progress checks. Strict bootstrap
refusal retains its typed cause, and managed startup joins the actual lifecycle
owner without duplicate activation.

The library semantics, recovery, timer and ordinary API tests that register the
explicit linked reference remain reference evidence. Connector tests that register that factory are also reference
evidence. Keep useful low-level Core unit tests; migrate the hosted behavior,
not every evaluator unit test into an FFI test.

**Remaining:** Fill the delta gap above and additional supported-behavior gaps
without inferring completeness from names/counts. [CW-89](#cw-89) owns shared
engine/backend wiring; [CW-85](#cw-85) owns CI selection of existing native cases.
Native Garnet is not positively qualified: its current
provider lacks an explicit restart-durability guarantee and the bounded reads
required by the native query. A real native refusal/no-write test passes;
linked Garnet reconstruction does not substitute for native functionality.
In particular, `garnet_native_and_standard_adapters_recover_primary_output_and_separate_reset_markers`
constructs the linked transformer; the actual-native case
`native_query_rejects_unqualified_garnet_without_touching_storage` proves refusal
without writes. Evidence:
`drasi-core/lib-integration-tests/tests/computation_garnet_recovery.rs:87-158,164-219`.

**Finish when:** Each supported behavior has a passing, identified native case
for its applicable middleware/storage/runtime combinations.
Include actual Garnet/RocksDB capabilities, non-atomic independent writers and
shared-journal restrictions; unsupported or unavailable profiles stay explicit.
Fix reproduced discrepancies without weaker assertions or new skips. Merely
rerunning existing suites belongs to [CW-78](#cw-78).

<a id="cw-90"></a>

#### Strengthen live example and tutorial acceptance (CW-90)

**Kind:** Confirmed weak assertions and application qualification.
**Area:** Server, Learning, Core examples and test-infra.
**Priority:** High for required examples. **Effort:** Medium-large.
**Depends on:** Only the missing implementations in [CW-47](#cw-47).

The real Server getting-started CI script leaves bootstrap and aggregation
correctness unchecked: `test_query_results` waits five seconds and reads query
configuration, and `test_aggregation_results` checks only query ID/success.
The data assertion inserts Alice, sleeps five seconds and greps for "Alice."
Because the initial seed contains different people, that is a real but shallow
CDC smoke, not an automatically vacuous assertion.

**Work:** Strengthen that existing test with literal seeded filtered rows and
histogram, a uniquely identified insert, exact update/delete/count transitions,
bounded result polling/readiness, HTTP status and parsed JSON. Remove its
unconditional sleeps without weakening diagnostics. Evidence:
`drasi-server/tests/integration/getting-started/run-integration-test.sh:156-220`,
`setup-postgres.sh:81-93`, `config.yaml:57-83` in the same directory, and
`drasi-server/.github/workflows/integration-test-getting-started.yml:52-64`.

Retain the seven migrated tutorials' existing scenario checks and automate
uncovered walkthrough/CI cases with exact rows, effects and visible updates,
bounded waits and repeatable reset. The Learning getting-started evaluator
already has expected-output scenarios; that is distinct from the weak Server
shell assertions above. Reuse existing tools rather than introduce another
tutorial framework. Building Comfort's verifier already derives an independent
source-data arithmetic oracle and checks all six query/dashboard snapshots
(`learning-drasi-server/tutorials/building-comfort/scripts/verify_demo.py:34-130`).

**Recorded Learning acceptance:** All seven tutorials have matching local-build
evidence. Getting Started covers real source changes, joins, aggregates, timers
and full Server restart; SSE CLI exercises the real stream and its binary-dependent
checks. Building Comfort and Curbside cover exact query/dashboard data, real
database changes and rendered live panels, including alert retraction and
timer-only changes. High-risk Containers uses actual Kubernetes Pods and
PostgreSQL risk rows. OTel uses the real Collector/store services for rollouts,
SLO cycles, absent-exporter timing and dependency expiry/recovery. Trading covers
all eleven queries, fifty prices, eight holdings, query-chain snapshot/live
updates, real UI CRUD, SSE stop/restart with an offline change and reload without
reprovisioning. Building Comfort's open browser also resynchronizes after a full
Server restart. Owned disposable infrastructure is cleaned up.

This is local macOS/Podman evidence, not a Linux CI/devcontainer run, optional
external Discord delivery or a new proof of every recovery profile. The weak
Server shell assertions above and broader application/platform qualification
remain separate work. Runnable Stockroom book variants and native network
lessons are also checked against the current local dependency lockfile.

Move-a-Wall, the embedded GPU Lab and tutorial migrations have earlier live
evidence. Re-run affected acceptance against the current bundle, including
snapshot/restart; do not reopen completed migrations or label old evidence as
a current run. Preserve GPU's real handover/atomic paired-value/SIGKILL/slot/
missing-history checks and Trading's real provisioning/CRUD/live/reconnect
tests. In-process projection/UI units cannot substitute for them.

The agentic tutorial workflow is supplementary, not an independent sole gate.
The six SSE CLI subprocess methods require `DRASI_SSE_CLI`; all skip without it
(`learning-drasi-server/tutorials/sse-cli/tests/test_cli.py:1-20,86-195`).
[CW-85](#cw-85) owns CI selection and explicit environment jobs.

**Finish when:** Required examples build and run normally with observable
readiness, exact data/effects, safe reset/shutdown/recovery and recorded
current-source provenance. Compilation alone, a success marker or a request
returning 200 does not establish this.

<a id="cw-46"></a>

#### Complete the Server workflow and recovery checks (CW-46)

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

Include [CW-45](#cw-45)'s stale previews, rejected acceptance, delayed/failed
realization, receipt lookup after lost replies and two instances created from
the same template. For [CW-70](#cw-70)'s reconstructible recipes, cover missing
factories, incompatible versions, unavailable secrets, failed construction and
cancelled restoration, with truthful retryable declarations and no orphaned
resources.

**Completed boundary:** Configured and API-created ordinary queries use explicit
native registration without a linked fallback. [CW-78](#cw-78) owns current and
historical execution totals, including the query-progress OTLP, plugin-dependent
E2E and crash-parent cases. Resolver reconstruction is not automatically
packaged-server/network crash coverage. Keep the actual stock-binary check
identified in [CW-84](#cw-84), and live tutorial acceptance in [CW-90](#cw-90).

**Finish when:** The selected current-version workflows have trustworthy
end-to-end results, including failure and restart, rather than only matching
intermediate captures.

<a id="cw-10"></a>

#### Finish public status and result consistency checks (CW-10)

**Kind:** Issue and qualification. **Area:** Core, Server.
**Priority:** High. **Effort:** Medium to large. **Origin:** review's inspection findings.

**Work:** Check remaining public outcomes for partial startup/shutdown, missing
or duplicate components, replaced query readers, missing result history and
initial-result/live-update races. Define the compatibility view's legacy
projection versus full native inspection explicitly: generic native
transformers must not be silently presented as a complete graph when omitted.
Preserve the repaired `Reconfiguring` status and initialized, generation-fenced
snapshot/history reads during quiescence.

**Consolidated conformance obligations:**

- For [CW-22](#cw-22), assert consistent public classifications, retained typed
  causes, actual operation context and user-visible outcomes across library,
  REST, logs and UI. A correct reset refusal alone does not prove that every
  plugin-owned cause remains inspectable.
- For [CW-23](#cw-23), exercise snapshot/changed acknowledgement, coalesced
  readiness/failure, late callbacks, slow subscribers, disconnect/reconnect and
  replacement. Check that clients consume the snapshot rather than busy-loop,
  and that coalesced status is not presented as a complete event history.
- For [CW-34](#cw-34) and [CW-69](#cw-69), generate/property-test streams, roles,
  references, cycles and unused bindings through equivalent entrypoints at the
  same validation phase. Missing prerequisites remain valid waiting states.
- For [CW-72](#cw-72), compare wrapped/unwrapped advertised capabilities through
  failure, batching, progress, retirement, cancellation and scoped deletion.
  Unsupported combinations must reject rather than report successful cleanup.
- For [CW-26](#cw-26), assert exact counts, units and timing boundaries across
  normal work, retries, batching and failure; use [CW-36](#cw-36) for actual
  end-to-end completion rather than infer effects from counters alone.

**Implemented:** The instance root in both `get_graph` and retained read-only
views follows the existing library running flag instead of a constant. It is
Stopped before startup and after stop, and Running while a partial startup still
owns active work. This is not an aggregate component-readiness or cleanup-success
claim; individual failures remain visible separately.

Native query output generation now remains observable after stop without
reopening ordinary reads. Canonical metrics, coherent paged reads, retained
recovery, replacement fencing and no-op/replay progress have focused coverage.
Do not reopen those implementations; extend native parity under
[CW-77](#cw-77) and keep broader public inspection outcomes in this item.

**Finish when:** Supported library and Server operations report the same truthful
state and results through replacement, restart and failure.

<a id="cw-13"></a>

#### Verify remaining query matching reports (CW-13)

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

<a id="cw-12"></a>

#### Investigate reported errors in grouped results (CW-12)

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

### Recovery, lifecycle and failure qualification

<a id="cw-14"></a>

#### Extend native-input recovery qualification (CW-14)

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
The recorded 28-case run predates the query-plugin cutover. Its hosted-query
construction must be identified and rerun under [CW-77](#cw-77) and
[CW-78](#cw-78) before citing it as current loaded-query evidence.

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

#### Make passing recovery checks prove completion (CW-36)

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
This run also predates the native query-factory cutover; it is not an acceptance
run of the current query plugin.

**Finish when:** Incomplete captures cannot pass, and each required behavior has
an exact expected result and a matching recorded run.

<a id="cw-20"></a>

#### Verify interrupted reset across storage providers (CW-20)

**Kind:** Qualification. **Area:** Core and persistent providers.
**Priority:** High. **Effort:** Large, mainly failure testing.

**Work:** Exercise process failure during query reset and removal with the
remaining supported storage providers and their optional operations. Exercise
failure before/after reset markers, partial state deletion, cancelled submitted
I/O, reopen and repeated deprovisioning. Use real reopen for every provider
advertising persistence; a memory provider is not crash-durable evidence.

**Completed query slice:** Native atomic and independent-writer non-atomic
profiles have actual publication/reset process-exit coverage. Identified
missing/corrupt rows and output, Strict refusal, AutoReset, foreign ownership,
nonzero reset baselines and handled consumer progress have loaded-query cases.
The Host grants baseline initialization once, only inside an authorized
bootstrap reset. RocksDB and Garnet staged checkpoint clearing is implemented;
the executed Garnet clear/reopen case is not a full native-query crash matrix.
Remaining native query/provider combinations are recorded in [CW-77](#cw-77);
this item retains broader provider reset/removal qualification.

**Finish when:** Partly cleared state cannot reopen as healthy, and explicit
recovery restores correct rows, progress and output identity.

<a id="cw-05"></a>

#### Extend safe-stop coverage to remaining shared resources (CW-05)

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

#### Verify overlapping application changes (CW-07)

**Kind:** Qualification. **Area:** Core, Server management.
**Priority:** High. **Effort:** Large, mainly testing.

**Work:** Combine saved configuration changes with conflicting start, stop,
replacement and removal requests. Include failed construction and cancelled
callers while unrelated operations continue. Preserve stale-preview rejection,
isolated start groups and control/peer/resource completion servicing already
fixed by the review repairs; extend adversarial schedules rather than replacing
those contracts with all-or-nothing deployment rollback.
Include [CW-71](#cw-71)'s retention versus concurrent snapshot changes, expired
request replay, namespace isolation and key rotation/unavailability. Coordinate
persistent restart schedules with [CW-39](#cw-39), not a duplicate housekeeping
test matrix.

**Finish when:** Accepted changes remain owned, conflicting changes stay ordered,
and the reported result accurately distinguishes applied changes from failures.

<a id="cw-08"></a>

#### Complete cleanup checks across native providers (CW-08)

**Kind:** Qualification. **Area:** Core, providers, SDK/Host.
**Priority:** High. **Effort:** Large, mainly testing.

**Work:** Extend combined cleanup checks to the remaining native component,
storage and plugin combinations. Verify that failed or cancelled cleanup retains
the resources and errors needed for a later retry.
For [CW-17](#cw-17), account for tasks, threads, connections, already-submitted
blocking storage and rejected construction results. Measure retained
failed-cleanup owners separately from actual leaks. Rust allocation/task/FD
checks do not qualify all foreign allocations or whole-process RSS.

**Completed query slice:** Retained recovery reads wait for actual Host cleanup
even after plugin STOP succeeds. Cancelled cleanup blocks restart, uncertain
query state stays unreadable, and deletion retires old readers. Stop also releases
an abandoned read transfer when no follow lease exists. These repairs and their
focused cases are complete; other provider/component combinations remain open.

**Finish when:** Each selected combination releases its actual resources or
reports unfinished ownership truthfully, including after repeated cleanup attempts.

<a id="cw-39"></a>

#### Test the remaining combinations of failures (CW-39)

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
Apply the same failure matrix to newly implemented capabilities rather than
duplicating it under their implementation entries: [CW-68](#cw-68)'s view
snapshot/lifecycle behavior (replay only if a future profile advertises it),
[CW-71](#cw-71)'s persistent housekeeping and key
recovery, [CW-27](#cw-27)'s slow monitoring destinations, and [CW-29](#cw-29) /
[CW-74](#cw-74)'s capacity and rate/window bounds. Use controlled clocks for
rate/burst/window checks through cancellation, slow consumers, failed replies
and restart; preserve ordering and contiguous completion.
The specific query/non-query scheduled-work chain formerly called F0 is tracked
in [CW-82](#cw-82), rather than duplicated as another query-plugin completion gate.

**Finish when:** Each selected combination has exact expected data, progress,
effects and cleanup outcomes rather than an inference from single-failure tests.

<a id="cw-03"></a>

#### Check remaining extension failure boundaries (CW-03)

**Kind:** Qualification/hardening. **Area:** Core, providers and plugin boundaries.
**Priority:** High. **Effort:** Medium to large.

**Work:** Audit uncovered factory, provider, data-validation and destruction
callbacks for catchable Rust failures. Add protection only where a concrete
unprotected path is found.

**Finish when:** The selected callback failures leave useful diagnostics, prevent
reuse of damaged objects and allow unrelated healthy work to continue.

<a id="cw-21"></a>

#### Investigate remaining retry and completion questions (CW-21)

**Kind:** Investigation. **Area:** Core merge, admission and delivery recovery.
**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

**Work:** Check clock changes during time-based merging, retries without an
original timestamp, content checks on output confirmation, expired replay
records and incorrect uncertainty reported for writes never attempted.

**Finish when:** Each concern has a clear result distinguishing rejected, saved
and genuinely uncertain work, with a fix where a failure is reproduced.

<a id="cw-35"></a>

#### Resolve the remaining ownership and limit findings (CW-35)

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

<a id="cw-82"></a>

#### Prove a mixed query and non-query scheduled pipeline (CW-82)

**Kind:** Optional cross-component qualification. **Area:** Core, native fixtures.
**Priority:** Required before advertising CW-81's combined profile.
**Effort:** Medium. **Origin:** Former query-design F0 proof. **Depends on:** CW-81.

**Work:** Compose the existing native SQLite source/bootstrap provider, one
stateful non-query transformer, the loaded query and a handled capture consumer.
Use real persistent storage and the common scheduling contract, with no new
connector, evaluator or graph owner.

[CW-81](#cw-81)'s standalone atomic-persistent scheduling contract is qualified:
controlled clocks, rollback, replacement/cancellation, completion and real
process-restart cases pass. That proof does not establish this mixed application
or qualify additional storage profiles.

Bootstrap while live input continues and compare exact transformed rows and
gap-free handover. Cancel initialization and verify retained ownership and
awaited cleanup. Schedule, replace and cancel work with component-state changes;
rollback must leak neither state nor timers. Advance the physical clock without
new source input and verify both owners' due output and provenance.

**Finish when:** Both runtimes pass real subprocess exits around bootstrap
completion, scheduling-request commit and due-work/output commit. Fresh processes
recover the exact checkpoints, pending work, rows and output identities without
duplicate committed completion or abandoned cleanup. This qualifies the new
combined scheduling profile, not the already completed query implementation.
Other multi-failure scenarios remain [CW-39](#cw-39).

### Native boundaries, platforms and execution qualification

<a id="cw-41"></a>

#### Complete checks using real plugin libraries (CW-41)

**Kind:** Safety audit/qualification. **Area:** ABI, SDK/Host, instance-host and contracts.
**Priority:** High. **Effort:** Large. **Review:** M41. **Origins:** CG-05/06/25.

**Work:** Extend malformed-data, version-rejection, cancellation, delayed-callback
and replacement tests using independently built libraries. Measure per-instance
resource release through repeated use; distinguish it from libraries intentionally
kept loaded for the process lifetime.

**Connector and format obligations moved from implementation entries:**

- For [CW-44](#cw-44), compare required working legacy behavior against real
  native sources, initial-data providers and sinks, using static and freshly
  built dynamic libraries, both Tokio flavors and actual Server consumers.
  Include failure/cancellation/cleanup, bounded CDC paging, streamed snapshots,
  credentials, source schema catalogs and procedure effects. Restart ownership
  is shared with [CW-14](#cw-14); unavailable SQL Server remains [CW-78](#cw-78).
  Move applicable old-interface test consumers to actual native construction
  while retaining their useful assertions and explicitly identified reference
  comparisons.
- Authenticate/rate-limit rejection on ordinary HTTP single-event, batch and
  producer-session routes must produce no graph changes. Qualify gRPC TLS only
  after it is implemented; webhook-only authentication or REST bootstrap does
  not establish stronger ingress/snapshot guarantees.
- For [CW-18](#cw-18), keep fixed byte/identity fixtures for envelopes, graph
  definitions, QoS/delivery metadata and query state. Check version/header
  dispatch before payload decoding and refusal without resetting valid state.
- For [CW-19](#cw-19), cover excessive size/depth and error/cancellation-path
  destruction. For [CW-76](#cw-76), cover boundary-size and malformed fragments
  across snapshot/replay/recovery/follow without partial results, lost
  acknowledgements, unbounded allocations or leaked provider retention.

The query round added real-library processing, paged-read, retained-recovery,
malformed-observation and reset-baseline cases. These are scoped evidence, not
completion of the unsafe-code audit or fuzzing below. Query-specific missing
behavior coverage belongs to [CW-77](#cw-77).

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
No conventional libFuzzer/honggfuzz or proptest/quickcheck harness was found in
the audit, but custom seeded/model tests already exist. Choose additional
tooling for a concrete missing boundary, not because a framework name is absent.

**Finish when:** The selected libraries reject invalid calls and release instance
resources without letting old callbacks act on replacements.

Include fixture disposal in the evidence: normal, repeated and failed-start
runs must remove only their owned containers and volumes. An environment failure
or a skipped database case is not a successful plugin run.

<a id="cw-67"></a>

#### Qualify the pinned CI Protocol Buffers compiler (CW-67)

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

<a id="cw-91"></a>

#### Qualify isolated hosts and prepared-peer deployment profiles (CW-91)

**Kind:** Platform/network qualification extracted from [CW-75](#cw-75).
**Area:** instance-host, bridge, Server and CI.
**Priority:** Required before advertising an additional profile. **Effort:** Large.
**Depends on:** The selected platform/containment capabilities in CW-75.

Qualify prepared-peer links across real partitions and prolonged disconnects,
beyond local/VM tests. Existing process-host evidence is Unix; Windows cannot
be inferred from compilation or another platform's run.

For each advertised platform/provider, prove parent/child/grandchild ownership,
parent death, child graph failure while its PID survives, version/authentication
rejection, lost admission receipts, stale connections, namespaced persistent
reopen and interrupted reset. Measure resources across the entire process tree.
Report a partition as unknown remote state, not a stopped peer or permission to
launch an unfenced duplicate.

Keep the existing kernel-containment, in-process unwind and native-abort
boundaries explicit. This qualification adds no remote component-method
executor, distributed transaction promise or automatic remote provisioning.
[CW-67](#cw-67) owns pinned compiler/build-wire verification;
[CW-41](#cw-41) owns the independent-library contract matrix.

**Finish when:** Current-source evidence supports each advertised hosting and
network profile, with unavailable environments recorded under [CW-78](#cw-78)
rather than counted as passes.

<a id="cw-78"></a>

#### Run the complete existing suites on the current query integration (CW-78)

**Kind:** Execution/CI qualification gap. **Area:** Core, Server, CI.
**Priority:** High before declaring the current code fully tested.
**Effort:** Medium plus any reproduced failures. **Origin:** CW-43 follow-up.

**Status:** Locally available execution baseline completed. Environment-bound
checks below remain open; this is not an unconditional all-platform pass.

The historical workspace command, the now-removed runtime runner,
complete Host SDK targets and Server suites were executed against frozen
production source and freshly rebuilt matching libraries. Exact commands,
per-target outcomes and source/lock hashes are retained in the
`correctness-baseline-final` session evidence set. Its inputs identify Core
`09c43c2d` and Server `ef3d7b45` plus the uncommitted correctness repairs.
Those source changes were subsequently committed as Core `a764d26d` and Server
`2d0f5e7`; the original frozen-source evidence remains identified separately.

| Executed scope | Result |
|---|---|
| Workspace outside separately covered library, Host SDK and integration packages | 4,982 passed; 233 existing ignored entries |
| Runtime runner: all 36 test batches | 9,098 passing executions; 50 explicitly permitted ignored entries |
| Complete Host SDK with registry/fetcher/watcher/test-support | 812 passing cases after the isolated network-fixture follow-up; eight parent workers and ten unavailable environment cases excluded from direct execution |
| Library and Host SDK documentation tests | 135 passed |
| Server unit, binary and integration targets, including available ignored/E2E cases | 1,013 passed; two crash workers and two unavailable SQL Server cases excluded from direct execution |
| Server documentation and tooling | Four doctests passed, one ignored documentation example; 70 Python tests passed |
| Standalone Core query examples | All 19 compiled offline against current source |

These scopes overlap: repeated feature/runtime executions are not unique tests
and must not be summed into a unique-test total. Parent-driven workers remain
required through their drivers. The 36 batches have 5,496 distinct
`(package, binary, test-name)` addresses, still merging feature variants and not
counting inner scenarios. Documentation totals include compile-only examples.
The 19 standalone examples use generated, gitignored lockfiles; their normal
offline refresh changed no package versions and is not behavioral acceptance.
The Host run initially reported one SSE
fixture failure caused by reusing a client keep-alive pool after listener
restart. The test now creates a new client for the new listener, retaining its
route-limit and cleanup assertions; the exact case and its complete 34-case
network target pass. Original failed logs remain preserved. No production code
changed after the frozen run.
Specifically, `host-sdk.log` / `host-sdk.exit-code` retain 811 passes, one
failure and 18 filtered entries. `sse-restart-focused.log`,
`sse-restart-fixed.log` and `network-followup.log` record reproduction, the exact
repair and target rerun. This is not a fresh complete Host rerun.

The same evidence set retains `workspace.log` / `workspace.exit-code`,
`runtime/*.list.log`, `runtime/*.discovered.tsv`, `runtime/*.executed.tsv`,
`runtime/*.log` and `runtime/matrix.exit-code`, plus
`library-host-docs.log`, `server-rust.log` / `server-rust.exit-code`,
`server-docs.log` / `server-docs.exit-code`, and
`server-tooling.log` / `server-tooling.exit-code`. Standalone compile evidence
is in `correctness-query-examples/*-normal.log`; none of those build logs
establishes a behavioral pass.

**Historical implementation evidence retained from [CW-43](#cw-43):** Runs
preceding Core
[`09c43c2d`](https://github.com/drasi-project/drasi-core/commit/09c43c2d8c5f5236589700006b0fedc281fc388f)
and Server
[`ef3d7b45`](https://github.com/drasi-project/drasi-server/commit/ef3d7b45d1dc31d66414549887c705964de261a8)
had 174 loaded-query cases, 1,235 no-default library unit cases, 400 Server
units (including both-runtime query-progress OTLP cases), four shared-query units
and 40 linked-reference fault/recovery/source-transaction cases. Five library
ignores and parent-driven native/recovery workers were retained. These earlier
runs are not substitutes for the current-code matrix above.

The repairs also corrected missing explicit factory registration, transparent
storage-capability forwarding, native checkpoint/binary-codec inspection, an
unpolled QoS publisher and a phantom MySQL output binding. The runner rejects
missing native coverage and does not count captured child failure diagnostics
as independent test executions. Changed Core/Host/Server code passes its lint
checks; changed Rust files pass formatting checks.

**Remaining environment gates:** Eight Host and two Server real SQL Server
cases require the user-approved supported environment; two FFI layout-seed
cases require their actual nightly/Linux allocator setup; the cross-OS
instance-host case requires `DRASI_NETWORK_CONTAINER`. None is counted as a
pass. Other live credential/OS integrations can be explicitly ignored, and some
connector cases are compiled out on ARM rather than appearing in ignore totals
(for example `drasi-core/components/sources/mssql/tests/integration_test.rs:1968-1973`).
Native Garnet refusal is not positive restart qualification. Pre-existing format drift in unchanged files remains separate
[lint/release work](#cw-61). The broader requirement ledger remains
`replacement_qualified=false` (32 of 60 requirements satisfied); successful
execution does not close its other partial/unqualified requirements or prove
that every test is useful.

**Finish when:** Existing required suites pass with traceable current-revision
results. Until then, failures and unavailable environments remain named blockers,
not exemptions from a full-pass claim. Preserve parent-driven subprocess fixtures and real backend
cleanup. Selection policy is [CW-85](#cw-85) and evidence semantics [CW-88](#cw-88).
Add new native assertions under [CW-77](#cw-77), Server failure scenarios under
[CW-46](#cw-46), and live example acceptance under [CW-90](#cw-90); do not turn
this run into a replacement test framework.

### Application and native-query performance qualification

<a id="cw-40"></a>

#### Measure larger and longer-running applications (CW-40)

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
The narrower native-query versus linked-reference comparison is
[CW-79](#cw-79). It feeds this larger application benchmark effort without
repeating the same measurement work or implying a universal speedup.

**Retained scoped evidence from [CW-31](#cw-31):** Synthetic 128-publication
runs at 4,096 nodes fell from about 256 ms to 0.33 ms. Native allocation
samples show substantially less retained publication memory, with live
controller/notification measurements on both runtimes. This is reporting
overhead, not equivalent query-throughput improvement.

The legacy `query-perf/run_perf_tests` launcher is a conditional consolidation
candidate, not a current correctness gate; [CW-84](#cw-84) records its defects.
Preserve its four scenarios and memory/RocksDB/Redis distinctions before any
replacement. Existing release workloads and resource soaks are valuable:
output equivalence precedes throughput comparison, and live Rust allocations,
tasks and descriptors do not measure every foreign allocation/RSS. Test-runner
build/fixture cost is [CW-87](#cw-87), not application performance.

**Finish when:** The selected deployment profiles have repeatable measurements,
exact result checks and explicit operating limits.

<a id="cw-79"></a>

#### Measure native query overhead against the linked implementation (CW-79)

**Kind:** Performance qualification and measured fixes. **Area:** Core, Host SDK.
**Priority:** High before claiming performance parity. **Effort:** Medium.
**Origin:** CW-43 follow-up.

**Work:** Compare graph-driven projection, joins/aggregates, scheduled output,
large reads and slow-consumer/backpressure workloads with equivalent inputs,
providers, persistence guarantees and expected results. Pin source revisions,
toolchain and features. Use the retained pre-plugin revision where needed and
keep an explicit linked reference available for matched comparisons; a different
recovery or storage profile is not a fair baseline.

Measure throughput, end-to-end/control tail latency, CPU, allocations, memory,
evaluator setup count and codec/service-boundary cost on both runtimes. Separate
static native-ABI overhead from dynamic loading. Establish workload-specific
acceptable bounds before signoff and fix material regressions using Core/shared
helpers, not another evaluator, cached row mirror or weakened recovery.

**Finish when:** Reproducible matched runs meet the agreed bounds with exact
results and bounded ownership. Unacceptable overhead remains named, reproduced
work; a measurement report alone is not performance parity. Earlier
root-only/diagnostic measurements do not close this item. Larger deployment
and long-duration comparisons against main remain [CW-40](#cw-40).

<a id="testing-audit-evidence"></a>

### Audit evidence, retained value and limitations

The audit included tracked and non-ignored untracked source in all four existing
working copies, including the native semantic and SSE fixture changes. Its
companion session artifact, `test-suite-inventory.tsv`, has **3,678 records**
covering files/helpers, Cargo packages/targets, documentation, scripts,
workflows, scenarios, fixtures and execution evidence. Records contain
fingerprints, count method, entrypoints, construction/runtime markers, CI
selection, review depth and caveats; they are not 3,678 unique executable tests.

| Repository | Inspected base HEAD, plus then-uncommitted changes | Files scanned, excluding caches |
|---|---|---:|
| drasi-core | `09c43c2d8c5f` | 2,581 |
| drasi-server | `ef3d7b45d1dc` | 1,008 |
| learning-drasi-server | `ae4b8aa9865c` | 264 |
| test-infra | `5b775d19391b` | 488 |

HEAD alone did not identify that snapshot. Rust declaration counting excluded
comments/string literals and distinguished macro templates, shared scenarios,
rustdoc, integration targets and examples; Python used AST declarations and
JavaScript declaration sites. Parameter tables, loops, macro expansion and
browser projects were not guessed into unique-test totals. Ignored source-like
files were checked outside dependency/build/cache trees; additional compiled
VS Code JavaScript was mapped to its TypeScript source, not counted again.
Candidate bodies/fixtures were inspected; other bodies were inventoried or
sampled. Checked-in runner/workflow selection and existing logs were reviewed;
no new tests, builds, containers or network operations ran for the audit.

**Retained value by family:** These assessments explain what to preserve while
executing the work above. They are not a second list of test tasks.

| Family | Assessment and evidence boundary |
|---|---|
| Core values, Cypher/GQL, scalar/aggregate functions | Mostly useful small deterministic tests. Expected ASTs protect grouping/parsing; literal null/error/numeric results are valuable despite short bodies. Prefer modest shared setup, not opaque giant matrices. Examples: `drasi-core/query-cypher/src/tests.rs:38-158`; `core/src/evaluation/functions/numeric/tests/round_tests.rs:36-171` in Core. |
| Shared engine/index scenarios | Reused behavioral oracles across memory, RocksDB transactions/archive and Redis-backed Garnet/cache are distinct coverage, not duplicate implementations. Preserve their permutations; repair the concrete wiring gaps in [CW-89](#cw-89). |
| Graph lifecycle, pipes, QoS, transactions, redb/WAL | Strong fault, cancellation, ownership, replay and real-reopen coverage. The QoS model has an independent accepted-event/cursor ledger across 128 seeds and 1,024 actions per seed inside one test (`drasi-core/lib-integration-tests/tests/computation_qos.rs:756-940`). Preserve it rather than infer absent property testing from absent framework names. |
| Crash parents/workers | Keep the external process boundaries and exact state/identity checks. Native input qualification kills after a proven boundary, reopens in a different PID and compares against a no-fault run (`drasi-core/lib-integration-tests/tests/native_input_crash_recovery.rs:1118-1229`). Differential recovery complements literal semantic oracles; ignored workers are not missing functionality. |
| Reference versus native query execution | Construction determines the evidence. The native target checks actual loaded factories and literal ADD/UPDATE/DELETE (`components/host-sdk/tests/native_query_plugin.rs` in Core). A graph/index test named native may still use the linked query. |
| Shared native semantic cases | Good literal per-step snapshots across static/loaded, memory/RocksDB and both runtimes. Preserve the shared assertions; [CW-77](#cw-77) owns the missing emitted-delta checks. Persistent selection alone is not restart evidence. |
| Codecs, native/legacy SDK and FFI | Handwritten golden MessagePack bytes are stronger than roundtrips alone (`drasi-core/components/computation-contracts/src/graph_codec/tests.rs:31-74`). Independently mismatched-layout libraries protect a boundary matching-build loopbacks cannot (`components/host-sdk/tests/ffi_layout_mismatch_test.rs:15-42` in Core). Retain packaging/platform permutations. |
| Sources, bootstrap and common protocols | Distinguish pure mapping/configuration, controlled transport, and real CDC/snapshot/handover fixtures. Constructor validation is not live handover proof (`drasi-core/components/bootstrappers/mysql/src/tests.rs:1-68`). Retain real process/volume ownership for restart/retention. |
| Middleware | Expected-event/identity tests are useful: unwind checks cardinality and concrete derived nodes/relationships (`drasi-core/middleware/src/unwind/tests.rs:30-185`). Pure transforms and optional/native integration protect different boundaries. |
| Reactions and destination completion | Actual SDK/loopback/service-emulator checks add value beyond config. SQS gates the HTTP reply and checks payload, incomplete stop, restart ownership and FIFO metadata (`drasi-core/components/reactions/aws-sqs/src/lifecycle_tests.rs:26-152`). Enqueue/Accepted completion is not remote exactly-once proof. |
| Identity and secret stores | Retain configuration/capability/schema and meaningful compile contracts. Repair copied provider oracles in [CW-83](#cw-83). Live credential/OS behavior remains limited; the keyring missing-key case is explicitly environment-ignored (`drasi-core/components/secret_stores/keyring/src/lib.rs:135-182`). |
| Server API/config/persistence/native resources | Strong real-Router isolation, refusal, preservation and replay cases. Reconstruction checks exact changes, query identity, journal UUID, no new append and sink cursor (`drasi-server/tests/native_computation_test/shared_storage.rs:498-590`). This is not automatically packaged-server/network-crash evidence. |
| Plugin/build/CI Python tooling | Valuable negative controls reject success-shaped installer output without binaries, preserve existing artifacts and reject changed pins (`drasi-server/tests/test_plugin_setup.py:66-143`). Runtime execution is not the only useful test boundary. |
| React package/public contracts | Strong ownership/cancellation/protocol tests with intentional fake transports, complemented by browsers and clean tarballs. Material config changes cancel old work/reject stale events (`drasi-server/dev-tools/react/test/DrasiLifecycle.test.tsx:107-185`); `packed-consumer.mjs:18-70` forbids source/link rescue and checks lock identities. Keep both levels. |
| Trading browser/live/visual | Preserve deterministic multi-engine clock/accessibility/visual cases separately from real provisioning/CRUD/live/reconnect. Live assertions use literal rows/counts and restricted endpoint translation (`drasi-server/examples/trading/app/test/live/trading.live.spec.ts:43-157`). Real Trading is Chromium-only; fixture-driven Trading and package examples cover other engines (`app/playwright.config.ts:32-37`, `app/test/live/playwright.config.ts:12-27` under Trading; `drasi-server/dev-tools/react/examples/playwright.config.ts:27-31`). Keep fixed visual environments and no automatic retries. |
| Control UI and VS Code | Useful malformed-stream, initial snapshot, reconnect and stale-disposal tests (`drasi-server/ui/tests/query-results-stream.test.mjs:35-103`; `dev-tools/vscode/drasi-server/src/test/suite/query-watcher.test.ts:1-104` in Server). The extension's isolated Electron boundary is distinct. CI selection, not assertion value, is the main stated gap. |
| GPU Lab and Move-a-wall | Preserve standalone Rust projections/epochs, UI record contracts and real native-input qualification. GPU writes during handover and checks paired atomic values, SIGKILL/restart, slot ownership and fail-closed missing history (`drasi-server/examples/gpu-cluster-lab/embedded/ops/check-native-input.py:85-181`). Root Cargo does not execute this standalone workspace. Move-a-wall's identity/malformed-record checks are useful (`examples/move-a-wall/ui/src/records.test.ts:5-35` in Server). |
| Learning tutorials/SSE CLI | Units protect ownership/configuration/no-overwrite/error propagation, not walkthrough completion. Retain the independent Building Comfort oracle and real CLI subprocess boundary described in [CW-90](#cw-90); missing CLI selection must remain visible. |
| Test-infra framework/E2E/data tools | Comparator negatives protect missing/duplicate/conflicting/reordered events, identity, incomplete captures and snapshot multiplicity (`test-infra/e2e-test-framework/test-run-host/src/recovery_comparison.rs:376-594`). Completion checkers reject stale ownership/counters without recorded batches. Standalone GDELT/Wikidata generated-file tests are outside the E2E Cargo workspace; keep them explicitly selected. |
| Documentation, examples, performance and soaks | Rustdoc/no-run protects compilation, not behavior. Preserve the copied-starter/book checks in [CW-85](#cw-85). Release workloads validate output/provenance; resource soaks measure live Rust allocations/tasks/FDs, not every foreign allocation or RSS (`drasi-core/lib/tests/measure-core-workloads.py:33-53,112-148`; `lib/examples/core_resource_soak.rs:22-153` in Core). |

**Review limits:** This is not mutation testing, a fresh coverage/flakiness study
or a fresh all-language execution. Most bodies were inventoried, not manually
reviewed assertion by assertion. Existing evidence is strongest for the specified
Rust/runtime/tooling runs; current JavaScript, tutorial, GPU, Move-a-wall,
cloud-provider and test-infra E2E behavior was not requalified by new audit runs.
No conventional Criterion harness was found, but custom release workloads and
soaks exist. Missing framework names are not evidence that their testing style
is absent.

Remote reusable workflow bodies, branch-protection settings, remote run history
and credentials were not fetched. Build/cache outputs are not independent tests;
generated VS Code tests map to source. [CW-78](#cw-78) retains the unavailable
SQL Server, allocator/layout, cross-OS, live cloud/OS and native Garnet limits.
Some substantive Server/test-infra evidence remains outside the global ledger:
`H01/H02/H04` do not mean "no implementation/no tests," and a nearby passing
suite does not make them covered. Refusal tests and fixture-only fixes remain
scoped evidence, not stronger production guarantees.
