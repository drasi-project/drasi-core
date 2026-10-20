# ComputationGraph remaining work

<a id="cw-06"></a>

## 1. Make starting and stopping dependable (CW-06)

**Priority:** High. **Effort:** Large.

**Work:** Set separate time limits for startup steps, individual components and
resources, and the whole shutdown. A timeout must report what remains unfinished
and who still owns it, without abandoning work or pretending cleanup succeeded.

**Finish when:** Slow or failing components cannot cause an unexplained indefinite
wait, and cleanup can be safely retried after a timeout or cancellation.

<a id="cw-44"></a>
<a id="hosting-connectors-server-and-examples"></a>
<a id="current-required-family-boundary"></a>

## 2. Finish the remaining data connections (CW-44)

**Priority:** High. **Effort:** Very large.

**Work:** Complete the selected connectors using their existing connection,
conversion and protocol logic:

| Area | Remaining work |
|---|---|
| Kafka and Kubernetes | Native sources and initial-data providers, independently packaged and exercised against real services. |
| SQL Server | Native source, initial-data provider and stored-procedure output. |
| MySQL | Independently packaged source and initial-data providers, remaining required modes, and stored-procedure output. |
| SQLite | Independently packaged live source and coordinated initial loading, usable in a complete Server deployment. |
| Database discovery | Expose available tables, fields and types through the native source and Server interfaces. |
| Network recovery | Resolve required snapshot/progress replacement and destination-completion gaps that the current HTTP/gRPC/SSE contracts do not cover. |
| Application components | Move remaining old-interface test and example consumers to the native components. |

**Finish when:** Each selected connection works through the new interfaces with
equivalent functional and failure coverage, including real libraries and services.

<a id="cw-45"></a>

## 3. Finish native Server and client workflows (CW-45)

**Priority:** High. **Effort:** Very large. **Depends on:** CW-44.

**Work:** Complete creation, editing, discovery, lifecycle and result viewing for
the remaining native components in Server, UI and clients. Make waiting,
accepted, running and failed states clear. Resolve the recorded full VS Code
type-check failures in the component and YAML tooling.

**Finish when:** Required workflows run entirely through supported native
components and current instance-scoped APIs, without old factory registrations
or misleading success states.

<a id="cw-14"></a>

## 4. Recover safely while saved input is still waiting (CW-14)

**Priority:** High for deployments needing this recovery mode. **Effort:** Large.

**Work:** Establish safe reconstruction after a process dies with unprocessed
messages still in persistent input queues. Saved consumer progress alone does
not describe every queued message or its transport number. Extend source-history
and initial-loading checks to the remaining required connectors.

**Finish when:** A fresh process restores the exact outstanding work without
missing or repeating effects, inventing progress, or deleting queued messages.

<a id="cw-36"></a>

## 5. Make passing recovery checks prove completion (CW-36)

**Priority:** High. **Effort:** Medium to large.

**Work:** Prove that all required query processing and destination work finished
before comparing results. Add genuine completion evidence to Building Comfort
baseline and recovery captures. Update the requirement checklist to reference
the newer tests and distinguish proved behavior from incomplete evidence.

**Finish when:** Incomplete captures cannot pass, and each required behavior has
an exact expected result and a matching recorded run.

<a id="cw-46"></a>

## 6. Complete the Server workflow and recovery checks (CW-46)

**Priority:** High. **Effort:** Large. **Depends on:** CW-36 and the affected CW-44/45 work.

**Work:** Cover the remaining combinations of delayed or failed startup, saved
configuration, reconstruction, reconnecting clients, old readers, native providers
and interrupted delivery. Record the actual Server and plugin versions used.

**Finish when:** The selected current-version workflows have trustworthy
end-to-end results, including failure and restart, rather than only matching
intermediate captures.

<a id="cw-43"></a>
<a id="cw-43-optional-native-query-plugin"></a>

## 7. Finish loadable query support and application definitions (CW-43)

**Priority:** High for the selected query-component work. **Effort:** Large.

**Work:** Resolve the separate performance/design investigation, then make query
processing an independently loadable component using the existing evaluator.
Carry its real storage, initial loading, scheduled work and result-reading
capabilities across that boundary. Complete remaining rebuildable definitions,
including selected source-time merger and provider combinations.

**Finish when:** Applications work with query support present or absent, missing
support is reported clearly, and loaded queries preserve required results,
transactions, restart and cleanup behavior.

<a id="cw-47"></a>

## 8. Finish required examples and normal builds (CW-47)

**Priority:** High for required examples. **Effort:** Large overall.

**Work:** Implement the GPU Lab deployment using the standard Server, finish
examples for the remaining selected connectors, and verify readiness, reset and
expected effects. Fix the Learning Trading build's unused `createdReaction`
field without disabling compiler checks.

**Finish when:** Required examples build normally and run against the supported
Server with clear setup, shutdown and recovery behavior.

<a id="cw-18"></a>

## 9. Keep saved data and retries unambiguous (CW-18)

**Priority:** High. **Effort:** Medium to large.

**Work:** Audit current saved and transmitted formats. Define which fields
identify a request, result or configuration, and add fixed examples of expected
bytes or identities. Formatting or diagnostic changes must not turn a retry into
new work. Unsupported or damaged data must fail clearly.

**Finish when:** Current-format restart and retries preserve identity, and
incompatible records are rejected without silently resetting valid state.

<a id="cw-19"></a>

## 10. Reject excessively deep data safely (CW-19)

**Priority:** High where input could terminate the process. **Effort:** Large.

**Work:** Audit supported decoders and internally constructed values beyond the
already bounded paths. Apply appropriate size and nesting limits, including safe
release of deeply nested values after an error or cancellation.

**Finish when:** Supported malformed-input cases fail with an error rather than
exhausting the call stack or terminating the process.

<a id="cw-20"></a>

## 11. Verify interrupted reset across storage providers (CW-20)

**Priority:** High. **Effort:** Large, mainly failure testing.

**Work:** Exercise process failure during query reset and removal with the
remaining supported storage providers and their optional operations.

**Finish when:** Partly cleared state cannot reopen as healthy, and explicit
recovery restores correct rows, progress and output identity.

<a id="cw-22"></a>

## 12. Make errors explain what happened and what to do (CW-22)

**Priority:** High. **Effort:** Large.

**Work:** Present the same failure consistently through the library, Server, UI,
logs and monitoring. Identify the affected component, what stopped, what work is
saved or unfinished, whether cleanup failed, and what can safely be retried.

**Finish when:** A user can diagnose and recover from the selected failures
without guessing from a generic error or mistaking rejection for successful work.

<a id="cw-23"></a>

## 13. Make live status and log updates reliable (CW-23)

**Priority:** High. **Effort:** Medium to large.

**Work:** Complete operational-status and log handover, slow-reader and reconnect
behavior. Detect missed updates and provide a way to obtain current state.
Preserve instance identity and release observer state when its owner is removed.

**Finish when:** Readers either receive the required updates or are explicitly
told to refresh, and old readers cannot report a replacement as their old component.

<a id="cw-10"></a>

## 14. Finish public status and result consistency checks (CW-10)

**Priority:** High. **Effort:** Medium to large.

**Work:** Check remaining public outcomes for partial startup/shutdown, missing
or duplicate components, replaced query readers, missing result history and
initial-result/live-update races.

**Finish when:** Supported library and Server operations report the same truthful
state and results through replacement, restart and failure.

<a id="cw-03"></a>

## 15. Check remaining extension failure boundaries (CW-03)

**Priority:** High. **Effort:** Medium to large.

**Work:** Audit uncovered factory, provider, data-validation and destruction
callbacks for catchable Rust failures. Add protection only where a concrete
unprotected path is found.

**Finish when:** The selected callback failures leave useful diagnostics, prevent
reuse of damaged objects and allow unrelated healthy work to continue.

<a id="cw-05"></a>

## 16. Extend safe-stop coverage to remaining shared resources (CW-05)

**Priority:** High. **Effort:** Medium to large.

**Work:** Exercise graceful stop and explicit forced interruption with remaining
providers and component-private shared resources. Verify who owns unfinished
work when a stop times out or its caller stops waiting.

**Finish when:** No supported stop path abandons accepted work, hides incomplete
cleanup or interrupts an unreported shared owner.

<a id="cw-07"></a>

## 17. Verify overlapping application changes (CW-07)

**Priority:** High. **Effort:** Large, mainly testing.

**Work:** Combine saved configuration changes with conflicting start, stop,
replacement and removal requests. Include failed construction and cancelled
callers while unrelated operations continue.

**Finish when:** Accepted changes remain owned, conflicting changes stay ordered,
and the reported result accurately distinguishes applied changes from failures.

<a id="cw-08"></a>

## 18. Complete cleanup checks across native providers (CW-08)

**Priority:** High. **Effort:** Large, mainly testing.

**Work:** Extend combined cleanup checks to the remaining native component,
storage and plugin combinations. Verify that failed or cancelled cleanup retains
the resources and errors needed for a later retry.

**Finish when:** Each selected combination releases its actual resources or
reports unfinished ownership truthfully, including after repeated cleanup attempts.

<a id="cw-17"></a>

## 19. Make components stop their own background work (CW-17)

**Priority:** High. **Effort:** Medium to large.

**Work:** Audit remaining components and providers that create their own tasks,
threads or connections. Establish ownership before starting them and retain it
until their real work has ended, including during failed startup and cancellation.

**Finish when:** Removed or stopped components leave no unowned background work
that can affect a replacement or keep its resources unavailable.

<a id="cw-39"></a>

## 20. Test the remaining combinations of failures (CW-39)

**Priority:** High. **Effort:** Large.

**Work:** Cover lost history with storage failures, clock changes with restart,
held late input across crashes, disconnected consumers with damaged history,
and lost destination replies with failed completion writes. Add direct proof
that a failed configuration read cannot cause saved state to be deleted.

**Finish when:** Each selected combination has exact expected data, progress,
effects and cleanup outcomes rather than an inference from single-failure tests.

<a id="cw-41"></a>

## 21. Complete checks using real plugin libraries (CW-41)

**Priority:** High. **Effort:** Large.

**Work:** Extend malformed-data, version-rejection, cancellation, delayed-callback
and replacement tests using independently built libraries. Measure per-instance
resource release through repeated use; distinguish it from libraries intentionally
kept loaded for the process lifetime.

**Finish when:** The selected libraries reject invalid calls and release instance
resources without letting old callbacks act on replacements.

<a id="cw-40"></a>

## 22. Measure larger and longer-running applications (CW-40)

**Priority:** High for release claims. **Effort:** Large.

**Work:** Measure growing application sizes, busy and slow branches, large
messages, shared storage, outages and prolonged operation. Set acceptable limits
for response time, processing cost, memory and disk use before declaring success.

**Finish when:** The selected deployment profiles have repeatable measurements,
exact result checks and explicit operating limits.

<a id="cw-31"></a>

## 23. Avoid rebuilding unchanged information for status updates (CW-31)

**Priority:** High for the agreed scaling requirement. **Effort:** Large.

**Work:** Measure the cost of rebuilding whole-application read-only reports.
Use graph-owned information on request or share unchanged data instead of
recreating all lookup tables whenever one component changes status. This concerns
reporting work, not changing or restarting working components.

**Finish when:** Small status changes avoid unnecessary whole-graph rebuilding,
while current-state reads, notifications and required history remain consistent.

<a id="cw-33"></a>

## 24. Reclaim bookkeeping left by removed components (CW-33)

**Priority:** Medium. **Effort:** Medium to large.

**Work:** Measure repeated removal and addition within one long-running graph,
then reclaim or reuse entries no longer needed by pending work or cleanup.
Preserve the distinction between an old component and a replacement.

**Finish when:** Keeping a stable number of active components does not accumulate
unnecessary entries indefinitely, and delayed old requests cannot affect new ones.

<a id="cw-29"></a>

## 25. Finish memory limits for saved queues and results (CW-29)

**Priority:** Medium. **Effort:** Large.

**Work:** Add the missing byte limits for shared transactional queues, saved
output and current result rows. Reserve required capacity before processing
commits, and define what happens to an oversized input or a full buffer.

**Finish when:** The selected limits hold through slow consumers, cancellation
and restart without accepting work that cannot be retained safely.

<a id="cw-28"></a>

## 26. Reduce unnecessary disk work and growth (CW-28)

**Priority:** Medium. **Effort:** Medium to large.

**Work:** Measure full-state rewrites, repeated reads, small writes and obsolete
stored records. Improve the expensive cases while preserving exact saved progress
and retry identity. Reproduce older storage reports before treating them as bugs.

**Finish when:** Selected workloads show lower storage cost or bounded growth
without changing results, recovery or independent query retention choices.

<a id="cw-32"></a>

## 27. Remove measured repetition from data processing (CW-32)

**Priority:** Medium. **Effort:** Medium.

**Work:** Find significant repeated encoding, hashing, metadata construction and
history scans in the retained processing paths. Reuse results where safe rather
than weakening input checks.

**Finish when:** Measured processing cost falls while exact output, identity,
replay and failure behavior remain unchanged.

<a id="cw-37"></a>

## 28. Make asynchronous tests reliable (CW-37)

**Priority:** Medium. **Effort:** Medium to large.

**Work:** Replace timing guesses with actual completion signals where tests
depend on them. Isolate shared test state and reproduce the Trading
`dialogAccessibility.test.tsx` failure under parallel execution.

**Finish when:** Selected tests reliably prove the intended behavior under load
and on both supported runtime styles, without weakened assertions or inflated timeouts.

<a id="cw-61"></a>

## 29. Restore useful compiler and code-check warnings (CW-61)

**Priority:** Medium. **Effort:** Medium overall; small packages can be done separately.

**Work:** Enable unused-code warnings in manageable parts of the project, remove
genuine leftovers and keep only justified exceptions. Reproduce and resolve the
recorded test-infra code-check failures rather than hiding them with broad suppression.

**Finish when:** Selected packages pass their normal checks with useful warnings
enabled across supported configurations.

<a id="cw-63"></a>

## 30. Remove the remaining old plugin support (CW-63)

**Priority:** Medium. **Effort:** Medium to large. **Depends on:** Required consumer moves in CW-44/45.

**Work:** Remove old hosts/adapters and obsolete binary-version branches once
their required users have moved. Remove bare-row decoding and guessed-schema
fallbacks that turn malformed current input into apparent success.

**Finish when:** The selected current interfaces are sufficient, unsupported
libraries/data fail clearly, and required functionality no longer depends on the
retired code.

<a id="cw-34"></a>

## 31. Simplify the remaining management code (CW-34)

**Priority:** Medium. **Effort:** Medium to large.

**Work:** Separate component ownership, commands, connections, scheduling and
shutdown where they are still difficult to reason about. Share repeated recovery
logic through small changes alongside the work that needs them.

**Finish when:** Changed areas have clear responsibilities and one authoritative
graph, with unchanged processing, cancellation, recovery and cleanup behavior.

<a id="cw-24"></a>

## 32. Let applications control logging and monitoring (CW-24)

**Priority:** Medium. **Effort:** Large.

**Work:** Implement the selected application-owned collection, filtering,
export and shutdown controls from the
[monitoring design](https://github.com/drasi-project/design-documents/tree/77b155319fd8160496ad059f6fdcb1e6bc93e43d/drasi-lib/tracing-logging).
Resolve the remaining profiling interfaces and dependency choices.

**Finish when:** Applications can choose the required monitoring level without
Drasi replacing their logging setup or disabling existing logs.

<a id="cw-25"></a>

## 33. Follow a request through processing and delivery (CW-25)

**Priority:** Medium. **Effort:** Large.

**Work:** Carry useful request and attempt information across queues, plugins,
network calls and retries. Define how a result relates to several inputs, while
keeping diagnostic information separate from the identity of saved business work.

**Finish when:** Selected requests can be followed through actual completion,
with meaningful timing and no invented duration across a restart.

<a id="cw-26"></a>

## 34. Make measurements consistent across components (CW-26)

**Priority:** Medium. **Effort:** Large.

**Work:** Implement the agreed measurements for availability, input/output,
completed handling, queue pressure, progress lag, errors and processing time.
Keep names and units consistent, prevent double counting and exclude sensitive data.

**Finish when:** Enabled measurements report exact expected counts and timing
boundaries across normal work, retries, batching and failure.

<a id="cw-27"></a>

## 35. Export monitoring data without blocking useful work (CW-27)

**Priority:** Medium. **Effort:** Large.

**Work:** Provide an optional application-owned export path for logs, measurements
and completed traces to OpenTelemetry services, local Drasi consumers, or both.
Bound waiting data, report dropped records and prevent monitoring its own output.

**Finish when:** Slow or broken monitoring destinations cannot stall business
processing, and export/flush/shutdown have explicit ownership.

<a id="cw-12"></a>

## 36. Investigate reported errors in grouped results (CW-12)

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

<a id="cw-13"></a>

## 37. Investigate reported query-matching errors (CW-13)

**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

**Work:** Reproduce optional-match filters, outer-variable references, list
membership and relationship-end errors. Verify exact results when candidates
and relationships change or disappear.

**Finish when:** Each report has a current reproduction and fix, or evidence that
the reported problem is not present.

**Reports:** [#795](https://github.com/drasi-project/drasi-core/issues/795),
[#796](https://github.com/drasi-project/drasi-core/issues/796),
[#797](https://github.com/drasi-project/drasi-core/issues/797),
[#798](https://github.com/drasi-project/drasi-core/issues/798),
[#807](https://github.com/drasi-project/drasi-core/issues/807),
[#808](https://github.com/drasi-project/drasi-core/issues/808),
[#812](https://github.com/drasi-project/drasi-core/issues/812).

<a id="cw-21"></a>

## 38. Investigate remaining retry and completion questions (CW-21)

**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

**Work:** Check clock changes during time-based merging, retries without an
original timestamp, content checks on output confirmation, expired replay
records and incorrect uncertainty reported for writes never attempted.

**Finish when:** Each concern has a clear result distinguishing rejected, saved
and genuinely uncertain work, with a fix where a failure is reproduced.

<a id="cw-35"></a>

## 39. Resolve the remaining ownership and limit findings (CW-35)

**Priority:** Conditional on reproduction. **Effort:** Unknown until reproduced.

**Work:** Resolve the specific review concerns about shared-resource ownership,
lock ordering, numeric limits and expired references. Reproduce an actual
failure before treating a concern as an implementation defect.

**Finish when:** Each named concern has a protected fix or an evidence-backed
closure, rather than remaining an open-ended code-cleanup task.
