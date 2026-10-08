# ComputationGraph: opt-in resilience and recovery plan

**Date: 2026-10-04. Status: native-only stages A-D complete; the broader nine-package plan is not complete.**

### Active native-only completion scope (user decision, 2026-10-07)

Complete the native framework, SDK, Host and Server work. Plugin migrations are
deferred, not prerequisites for framework completion. Do not remove existing
legacy support or spend this phase extending its recovery services. Production
database-plugin packaging, native SQLite REST and GPU plugin migration remain
separate work. Use test-only native fixtures to prove the framework contracts.

Close these stages in order; no stage is complete merely because work has begun
in a later one:

| Stage | Remaining native work | Exit evidence |
|---|---|---|
| A: contracts and ownership | Native coordinated bootstrap/initialization and consumer-completion services, capability validation and safe lifecycle boundaries. Reuse existing admission, progress and transaction services. | Real native-library negotiation, errors, cancellation, cleanup and revocation cases pass; unbound fast paths retain their existing behavior. |
| B: processing guarantees | Finish native use of the shared admission, transaction, output-handoff and consumer-progress contracts. | Shared-domain atomicity and separate-store replay preserve required obligations across interrupted commits, partial delivery and reconstruction. |
| C: Server operation | Expose the completed native services through existing resource recipes and managed reconstruction; finish standalone-provider transitions and explicit loss-authorized retirement. | Actual Server configuration, acceptance/refusal, restart and transition cases pass without silent loss or hidden ownership. |
| D: final qualification | Complete native whole-path and source-transaction evidence, plus the disabled-recovery cost gate. | Separately built native fixtures, real process exits/faults and repeatable ordinary/native performance evidence meet the declared contracts. |

The reference set closes the framework; migrating every existing plugin does
not. No additional executor, dynamic query engine or general-purpose resource
injection system is required by this scope.

**Authorized follow-on order (2026-10-07):** finish Stage D first; preserve each
Core/Server pre-commit HEAD on a milestone branch, then commit and push the
completed work on the existing branches. Only after that publication, begin
native migration of Move-a-Wall and GPU Cluster Lab, including the plugin work
those examples require. This is not authorization for a general plugin migration
sweep or a reason to broaden Stage D.

**Example follow-on complete (2026-10-08).** Both examples use native SSE with
direct query edges and a shared browser binding that validates the real graph.
GPU Cluster Lab also uses the native PostgreSQL transaction source, coordinated
snapshot and one persistent atomic query owner; downstream native queries receive
the policy component's authoritative table projections. Isolated live/browser,
writer recovery, crash/restart/reset, missing-slot refusal, SQL mutation and
scenario checks pass. The running deployment is unchanged. Atomic completion of
all downstream effects remains unsupported and explicitly asserted as such;
general plugin migration and diagnostic replay/performance work remain separate.
These example changes are not part of the published native-framework commits.

**Stage A complete (native-only scope).** Bootstrap-v1 supplies query-owned
preparation, bounded initialization state, streamed snapshots and final handover.
Consumer-v1 supplies external and transactional completion through the existing
Host-owned `DeliveryRunner`, stable batch/operation identity and graph-owned
delivery resources. Both reuse the revocable state mailbox and existing runtime.
No production plugin was migrated.

Exit evidence: 46 native Host cases pass, including 10 bootstrap and 15 consumer
cases; actual separately built libraries and RocksDB exercise reconstruction.
Coverage includes malformed negotiation, actual progress ownership, forged
content/stale generations, partial completion, ignored state errors, cancellation,
retained cleanup, rejected/weakened storage, panic fencing and graph factory
binding. The six ABI and 19 SDK cases pass without changing base ABI 1.0/wire 2.
All 22 shared delivery regressions pass, including their invoked process-crash
children; two child-only entry tests remain intentionally ignored in the parent.
Strict native ABI/SDK/Host and library lint gates pass. Server resource recipes
remain Stage C, and the new changes' performance gate remains Stage D.

**Stage B complete (native-only scope).** The new consumers now run behind real
native transaction participants and both shared-domain and separate-store QoS
fan-out. Four mode/storage combinations preserve the failed branch's obligation,
resume partial batches under replacement generations and avoid duplicate journal
acceptance. Transactional business counts commit exactly with operation progress;
external destinations use stable keys and may repeat one interrupted effect.

Eight real-library completion-fault combinations cover external/transactional
mode, pre/post-commit failure and cancellation. Uncertain owners reject cached
progress and further delivery; reconstruction observes the actual committed
prefix and completes every remaining operation. Sixteen native-consumer cases,
three admission cases and 14 native-pipeline cases pass, including the existing
shared-transaction process-exit parent/child checks. Strict Host lint passes.
No second recovery implementation or production plugin migration was added.

**Stage C complete (native-only scope).** Native bootstrap/consumer resource recipes now use actual
declared dependencies in Host and Server. Bootstrap recipes retain secret
references and bind one query/progress owner; consumer recipes isolate persisted
progress by instance and graph. The Server's live resolver now forwards both
dependency construction and transition guards. Real-library cases cover managed
bootstrap reconstruction without resnapshot, instance isolation, partial consumer
completion with retained upstream obligations, metadata discovery and retired
bootstrap revocation. Provider transitions and explicit loss-authorized retirement
are qualified below. Final whole-path/process/performance qualification remains
Stage D.

Current recipe evidence: 49 native Host cases and six Host management cases pass.
Four new Server cases cover managed and imperative/YAML reconstruction, metadata
discovery and consumer progress; eight existing managed Server cases still pass.
No production plugin migration or silent pending-domain reconfiguration is included.

Standalone persistent QoS retirement now holds the actual transaction owner
through configuration resolution, with required subscriber drain checked under
the journal-state and transaction locks. Confirmed rejection resumes it; accepted
or abandoned retirement fences it. Terminal cleanup revokes an uncertain lease
without reopening the owner. Actual Server cases cover pending-delivery refusal,
drained reconstruction, cancelled callers, confirmed/uncertain rejection and
acceptance, and shutdown/restart with unavailable commit confirmation.
Initialized built-in continuous queries now weakly identify their actual provider
and transaction in the existing output contract. Retirement holds that owner,
checks output and scheduled work, and rejects unused or foreign-provider evidence.
Separate-store query cases cover quiesce/stop, same-storage reconstruction, path
redirection, persisted removal, and uncertain acceptance/rejection resolved by
reconciliation or shutdown/restart. Ordinary stop remains interruptible; the
qualification quiesces first rather than weakening the interrupted-owner refusal.

Current retirement evidence: 25 Core ownership cases, the new provider-identity
and journal-waiter cases, 16 existing QoS cases, 88 shared query conformance cases,
and all 38 native Server cases pass (one child-only entry remains ignored by its
parent run). Native and discovery fixtures were rebuilt separately; stale legacy
discovery binaries were not treated as current-code failures.
Native linear transaction sequences now bind their actual standalone owner and
recognize their immutable transformer registry. A real native counter/participant/
sink case retires and reconstructs that owner without replaying completed input.
Enabled transaction-transformer, middleware and source-transaction regressions
and strict Core/library/Server lint pass.

Native consumers now retain positive completion evidence from the existing
delivery runner only after healthy ledger validation and successful terminal
storage cleanup. The service holds no indexes or replacement runner; a lifecycle
lease and the graph freeze exclude reopening through configuration resolution.
Partial, uncertain and uninitialized ledgers cannot prove lossless drain.
Native bootstrap retirement holds the existing call gate after positive stop;
acceptance, abandonment and terminal cleanup revoke the old provider. Associated
source-progress resources must match the actual processing owner. Known catalog,
configuration and factory registry values are recognized as non-storage services,
not independent proof of drain.

Current focused evidence: 23 delivery cases pass (two process-child entry points
remain intentionally ignored in the parent); 50 native Host cases and nine native
Server service cases pass. Server coverage includes actual partial consumer
completion, drained replacement and a third reconstruction, plus six
acceptance/rejection/uncertainty/terminal-close schedules. A native bootstrap
domain reconstructs its actual query, progress, catalog and secret-reference
resources without resnapshotting.

Explicit loss authorization now permits **complete domain removal only**, as
chosen by the user. `desired.retirement` records `from_revision`,
`allow_data_loss: true`, and exact resource/component membership with the existing
durable configuration and idempotency receipt. It cannot authorize in-place reuse,
omit connected live users, or bypass actual stop/cleanup and storage ownership.
Pending obligations and a stopped component's earlier processing failure may be
abandoned; failed cleanup and unhealthy transaction owners still refuse.
No processing data is deleted, reset or marked handled. Restoring the original
definition later can replay the intact pending work.

Stage C exit evidence: 10 retirement unit cases, all 23 delivery cases, 50 native
Host cases, 14 native pipeline cases, and all 45 Server native cases pass.
The parent runs intentionally ignore two delivery child entry points, one native
pipeline child entry and one Server process child; their parents invoke them.
The loss cases cover revision/scope/durable-store refusal, JSON/YAML acceptance,
receipt retry/conflict, six journal and six native-consumer uncertainty schedules,
and both shared and standalone query-domain removal. Direct reopening verifies
retained input, unadvanced cursors, partial business state and replay identity.
Native fixtures were rebuilt separately; strict Core/Lib/Host and Server lint
passes.

**Stage D complete (native-only scope).**
Real native consumer processes exit immediately before/after operation-completion
commit in external and transactional modes. Reconstruction observes the actual
committed prefix, repeats only the permitted external attempt, and completes all
three operations without changing their identities. Actual Server processes exit
with partial work and before/after durable retirement acceptance; restart follows
the accepted configuration/receipt and preserves pending data for replay.

Final-source qualification passes 289 computation library units, 18 controller
cases, seven query cases, 16 query-recovery cases, five enabled source-transaction
cases and 22 enabled transaction-transformer cases. It also passes 51 native Host
cases, three admission cases, 14 native pipeline cases, 57 QoS cases, three
transaction-query cases, six runtime-flavor cases and 46 native Server cases.
The unchanged ownership/SDK/contracts surfaces retain 36 Core ownership, six ABI,
19 SDK, 18 reconciliation, 19 recovery-contract, nine resource-dependency and
12 consumer-recovery passing cases. Process-child entries are invoked and checked
by their parents, not counted as standalone passes or skipped evidence. Native
fixtures were rebuilt separately; strict Core/Lib/ABI/SDK/Host and Server lint and
the runtime-parity runner self-check pass.

The initial native measurement exposed a repeatable 50% low-window p99 penalty
and 6% window-32 throughput loss. Crossover runs attributed the large tail
penalty to the Host. Node budgets now reset after actual suspension and requeue
through the node's own waker instead of deferring the controller's Tokio task.
Each input or continuation includes its first forward in one unit of work;
additional forwards are charged separately. The bound remains 64, with ready
sources and a 4,096-output burst explicitly checked for consumer progress within
64 inputs/forwards on both Tokio flavors. Unexhausted checkpoints construct no
yield future. Ready construction/regular command queues alternate without a
nested select or async wrapper on each data-plane wake; either queue can close
without closing the other.

Opt-in admission, shared-storage and durable-completion futures are isolated from
ordinary work. The native SDK selects the consumer wrapper before exporting the
operation future, and ordinary query evaluation specializes its already-known
source-transaction mode without duplicating the evaluator. Profiling identified
out-of-line bounded MessagePack writes; inline all-or-error bulk writes remove
that cost while preserving exact byte limits, fallible allocation and the wire
format. Regression cases protect the disabled completion-future size, no-partial
oversized writes and exact per-poll scheduling behavior.

Latest cost evidence uses identical benchmark sources, release settings and
common external dependency versions against Core baseline
`e46f6130b29d9268603585a5912e92d6839fa01f`: seven interleaved 300,000-event
runs per binary/window, after 2,048 warmup events, on macOS arm64 current-thread.
Ordinary projection and native arithmetic are different workloads, each compared
only with its own baseline. Native allocation counts cover the Host only;
CPU/RSS cover the whole process, including startup and the plugin.

| Workload / window | Events/s, baseline -> current | p50 ns, baseline -> current | p99 ns, baseline -> current | Process CPU seconds, baseline -> current |
|---|---|---|---|---|
| Ordinary / 1 | 23,262 -> 23,097 | 41,833 -> 42,250 | 49,250 -> 49,416 | 9.712 -> 9.777 |
| Ordinary / 32 | 34,059 -> 34,013 | 928,042 -> 930,500 | 969,458 -> 968,875 | 8.374 -> 8.386 |
| Native / 1 | 36,394 -> 36,804 | 26,666 -> 26,625 | 32,583 -> 32,125 | 4.991 -> 4.889 |
| Native / 32 | 70,423 -> 71,744 | 454,083 -> 443,708 | 473,458 -> 466,041 | 4.099 -> 4.018 |

Native Host allocations remain exactly 163/event and 21,594.730 bytes/event.
Ordinary counts remain 751.001/event at window 1 and 748.439 at window 32;
the difference is a few allocations over the entire run, not per input.
Allocation traffic falls by approximately 320 bytes/event:
124,141 -> 123,821 and 123,940 -> 123,620 bytes/event.

Native throughput improves 1.13%/1.88%, CPU falls 2.06%/1.97%, and p99 falls
1.41%/1.57% at windows 1/32. Ordinary throughput medians differ by -0.71%/-0.14%
and CPU by +0.67%/+0.15%. Ordinary throughput ranges overlap:
23,016-23,352 versus 22,516-23,267 at window 1, and 33,901-34,167 versus
33,829-34,360 at window 32. Overlap alone is not proof of no regression.

A further seven-pair, 300,000-event comparison uses the original ordinary source
with only mandatory node/controller fairness corrections as a diagnostic control.
Against that control, final-source throughput differs by -0.04%/+0.55%, CPU by
-0.12%/-0.54%, and p99 by +0.34%/+1.13%. Individual paired timing differences go
in both directions; window-32 p99 ranges from -4.50% to +6.83%. This does not show
a repeatable disabled-recovery timing penalty. Dependency-feature-matched native
controls and same-plugin crossovers also ruled out dependency changes as the
principal cause of the corrected native penalty. None of these controls replaces
the original acceptance binaries; the archived source/manifests were restored.

**Memory is not unchanged.** Peak RSS is 25.26 -> 26.02 MB and 25.41 -> 26.10 MB
for ordinary, and 18.96 -> 20.07 MB and 18.96 -> 20.10 MB for native. Executable
text grows by 0.92 MB for ordinary and 1.46 MB for the native Host, plus 0.61 MB
for its plugin. Separate active-process memory maps show roughly 0.3/0.9 MiB more
resident executable text and 0.3/0.2 MiB more private physical footprint for
ordinary/native. These are sampled process footprints, not a precise attribution
of every RSS byte or a claim that the entire increase is code. The larger loaded
framework has a real fixed footprint; allocation traffic is not retained memory.
No recovery store, runner, queue or service is activated by these workloads.

The Stage D disabled-recovery gate is met for the declared ordinary/native
workloads: the identified disabled-feature per-input/future and serialization
costs are corrected, native timing improves, and the fairness-controlled ordinary
runs show no repeatable adverse timing direction. This is not a percentage
allowance, a claim of identical binary/process size, a universal latency bound,
or qualification of every plugin/workload. The fixed footprint above remains an
explicit cost rather than being hidden behind a "zero overhead" claim.

Raw runs, binary/plugin hashes, ranges and logs are retained in the session
artifacts as `native-completion-{ordinary,native,fairness}-performance.json`,
`native-completion-*.log`, `native-completion-memory-*.txt` and the unchanged-module
`native-qualified-*.log` records. Reproduce with the existing release benchmark
examples and `lib/tests/measure-fast-path.py`; keep the original baseline binaries
separate from diagnostic builds. Publication and then the two example migrations
are the authorized next work, not prerequisites omitted from Stage D.

### Previous bounded milestone (completed 2026-10-07)

Feature expansion is deferred. Complete the existing supported configurations
in this order:

1. Fix defects and qualify volatile processing, native HTTP/gRPC admission,
   managed configuration, and built-in query/shared-QoS recovery. Retain
   fail-closed rejection of unsupported configurations and transitions.
2. Measure disabled-recovery throughput, latency, allocations, CPU and memory
   against the pre-reliability baseline, using identical workloads and repeated
   runs. Resolve attributable regressions rather than inferring zero overhead.
3. Finish focused regression checks and accurate supported/unsupported guidance.
4. Rebuild and run GPU Cluster Lab and Move-a-Wall against the current sources.
   Examples are last; a running older image is not current-source evidence.

Deferred: standalone-provider transition support, explicit loss-authorized
retirement, additional legacy recovery adoption, production native database-plugin
packaging, native SQLite REST, and broader connector hardening/qualification.
The Rust database reference implementations and existing recovery services remain;
they must not be described as fully packaged Server capabilities.

This is a bounded completion milestone, not completion of all original W1-W9
criteria. The sections below retain the original plan and historical evidence.

**Bounded milestone completed on 2026-10-07.** Existing supported-path
qualification covers volatile processing, native HTTP/gRPC admission, managed
configuration and built-in shared query/QoS recovery, including actual
process-exit reconstruction. The performance correction and measurements are
below. Move-a-Wall was rebuilt and passed its real Server/SSE/browser checks;
GPU Cluster Lab was rebuilt and passed isolated functional/restart/writer
recovery checks without touching its older live deployment. GPU's explicitly
unsupported whole-transaction policy-context guarantee remains deferred; its
full acceptance assertion was not weakened. These are exercised configurations,
not a claim that every plugin, provider or deployment is qualified.

### Earlier disabled-recovery measurement (2026-10-07)

This bounded-milestone measurement is historical. The completed native Stage D
measurement above supersedes its current-code costs and acceptance evidence.

The `lib/examples/fast_path.rs` release workload uses an ordinary
ApplicationSource, one in-memory projection query and ApplicationReaction on
Tokio current-thread. Every result is checked. It enables no durable source,
journal, checkpoint provider or consumer recovery. The comparison uses Core
`e46f6130b29d9268603585a5912e92d6839fa01f` and the current working sources,
identical benchmark source, Rust 1.95, release LTO and matching versions of common
external dependencies. A source archive, not another checkout, holds the baseline;
seed its lockfile from the current workspace before resolving its older manifests.

The first run exposed a genuine disabled-feature cost: the query transform
allocation grew from 19,192 to 32,928 bytes because its future contained large
opt-in transaction branches. Boxing only those enabled branches reduces it to
19,472 bytes without adding an ordinary-path allocation. Source-transaction
deadline/cancellation and shared commit semantics are unchanged.

Final results below are medians of seven interleaved runs per binary and window,
300,000 events per run after 2,048 warmup events, on this shared macOS arm64 host:

| Metric | Window 1 baseline / current | Window 32 baseline / current |
|---|---|---|
| Events/second | 22,387 / 22,154 | 31,230 / 32,494 |
| p50 latency (microseconds) | 43.88 / 45.21 | 959.33 / 932.88 |
| p99 latency (microseconds) | 60.38 / 68.08 | 1,892.13 / 1,567.42 |
| Allocations/event, including reallocations | 751.00 / 750.95 | 748.44 / 748.34 |
| Allocated bytes/event | 124,141 / 124,498 | 123,940 / 124,290 |
| Whole-process CPU seconds | 10.08 / 10.25 | 9.11 / 9.06 |
| Peak RSS bytes | 25,296,896 / 25,985,024 | 25,427,968 / 26,050,560 |

Allocation traffic increased by 0.28-0.29%, not zero; peak process RSS increased
by 0.62-0.69 MB. Timing was noisy: window-1 throughput ranged from 19,028-22,732
for baseline and 16,253-22,771 for current; window-32 ranges were 27,606-33,179
and 27,571-33,086. These overlapping observations do not establish a precise
throughput or tail-latency regression bound. This qualifies one simple query
workload, not all query shapes, native plugins, network paths or sustained load.
Allocation bytes are traffic, not retained heap; CPU/RSS include process startup,
warmup and shutdown. The measured allocator delegates to System in both binaries.

Reproduce the repeated measurements with
`python3 lib/tests/measure-fast-path.py BASELINE_BINARY CURRENT_BINARY OUTPUT.json --events 300000`.
Build each binary using `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build --offline -p drasi-lib --example fast_path --release`
and retain distinct copies. The runner captures hashes, individual observations,
medians and ranges. For allocation diagnosis only, set
`DRASI_FAST_PATH_ALLOCATION_SIZES=1`; histogram bucket 65,536 also counts larger
allocations. Leave it unset for the timing/RSS comparison.

### Implementation status

The first changes repair specific W7/W8 boundaries and add W1 storage declarations.
They do **not** establish new end-to-end recovery guarantees or complete Stage A.

| Package | Current implementation status |
|---|---|
| W1 | Partial: storage declarations and explicit whole-path assertions now use constructed components/resources. Construction, activation, replacement, native batches, managed-definition composition and live inspection enforce the assertions. Wider plugin adoption, service negotiation, transition policies and final qualification remain. |
| W2 | Partial: QoS-backed producer sessions, atomic input/receipt acceptance, bounded duplicate tracking, graph-owned binding and native HTTP/gRPC durable protocols are implemented. Real-library reconstruction, commit interruption and client receipt-loss cases are covered. Legacy adoption and remaining whole-plan qualification remain. |
| W3 | Partial: opt-in Rust `DeliveryRunner` persists ordered per-operation completion and stable scoped IDs. Transactional consumer state shares the completion commit; separate PostgreSQL and HTTP services qualify transactional destination effects, bounded confirmation and reconstruction. All bundled reaction boundaries are inventoried; legacy adapters remain explicitly acceptance-only. Stronger factory/plugin exposure remains. |
| W4 | Partial: query-owned initialization state, awaited snapshot cleanup, native PostgreSQL exported snapshots, native SQLite initial-read/live handover, and opt-in native MySQL coordinated snapshots are implemented. Legacy/plugin exposure, broader connectors and full qualification remain. |
| W5 | Partial: bounded retained access, stable output receipts and durable destination membership are implemented. `SharedStorageGroup` now integrates one actual storage transaction with query, scheduled-query, middleware and linear-transaction output journals, selected through provider/pipe resources. Host and Server expose shared-storage recipes. Real graph cancellation, crash and subscriber-progress cases pass. Plugin adoption and broader recovery/transition qualification remain; separate-store retries remain bounded by receipt retention. |
| W6 | Partial: complete-transaction framing/query execution, native PostgreSQL replay/initial loading, native SQLite isolated scopes and atomic replay records, and native MySQL owned streaming with verified snapshot/live handover. Native ABI exposure, SQLite REST and the full matrix remain. |
| W7 | Partial: explicit storage-list errors, bootstrap settings/properties, provider-proxy release, bounded lifecycle catch-up and typed public/Server errors. Legacy ABI 0.17 retains ABI 0.16 fast-mode loading. Optional native service-v1 provides scoped admission while preserving genuine ABI 1.0 fast-mode binaries. Legacy shared services, other recovery capabilities and remaining boundary qualification are still required. |
| W8 | Partial: fair/bounded control, nested shutdown, coherent generations, ownership-before-spawn and cancellation-safe joining. Shared bases and selected source/reaction connectors are qualified below, including a native application sink with retained callback futures. Wider connector adoption, submitted-storage fencing and full shutdown/replacement qualification remain. |
| W9 | Partial: encrypted Server-managed desired state, authoritative restart, revision/receipt/status APIs, scoped admission/replay and graph-owned shared-storage recipes. Protected changes reject before acceptance; stopped shared domains support held drain proof and authoritative retirement/reconstruction. Explicit loss authorization, standalone-provider transitions and whole-path qualification remain. |

Controller changes add no recovery journals, checkpoints or data-plane workers.
There are two bounded control lanes (64 commands each): ordinary mutations wait
without being drained into an unbounded deferred queue, while independent
additions and generation-fenced handle stops remain serviceable during component
construction. This is lifecycle correctness, not a new delivery guarantee.

Nested stop now uses the existing owned stop operation: it cancels unfinished
startup and waits for failure cleanup already in progress instead of interrupting
or duplicating it. Ordinary removal validates the selected generation and
applies its mutation in one controller command, so unrelated resource
observations cannot invalidate a separate preview. Record-token and generation
checks read the same registry publication.

Shared worker helpers now reserve a vacant slot before spawning and retain each
handle until joining observes its exit. Ordinary join timeout requests abort and
reports incomplete cleanup; a retry can finish the join. The separate graceful
join retains ownership without aborting a worker that must drain its children.
Panics remain typed errors.
Source/query/reaction base cleanup follows this rule, and reaction forwarders
are joined rather than merely aborted. Source cleanup also clears old subscriber
position filters with their dispatchers.

HTTP, gRPC, Application and OTel listeners/processors and WAL pruners use owned
registration and/or in-place cleanup. PostgreSQL retains failed-start workers
until cleanup, rejects replacement of an unjoined replication worker, and stops
workers even when initialization or startup failed. OTel's two listener futures
are scoped to its receiver; an unexpected exit cannot cause a second poll of a
completed task. OTel stop joins the receiver before saving lifecycle state and
surfaces failed saves.

Dashboard owns processing, heartbeat and server workers before spawning.
Cancelled cleanup and remaining upgraded connections block restart even after
the HTTP server handle is gone. Scoped WebSocket loops and graceful server joins
keep the connection-drain boundary intact. Four lifecycle cases cover cancelled
worker/connection cleanup, typed server errors/panics and three actual same-port
HTTP/WebSocket restarts. All 63 Dashboard units, two integration cases, 1,124
no-default library units and six shared worker integration cases pass. The two
ignored delivery crash helpers are explicitly driven by their ordinary parents.
The parity runner includes the Dashboard lifecycle profile. This does not
upgrade browser notifications beyond the existing acceptance-only boundary.

SSE and MCP now bind before reporting Running, retain timed-out/cancelled server
ownership, close live streams and drain HTTP requests before Stopped. Actual
five-second timeout cases keep a slow HTTP handler or incomplete request body
owned until a later stop; three same-port restart cycles release connections.
MCP stream drop closes its channel synchronously and signals the existing
processing worker, rather than spawning unowned cleanup tasks. Closed sessions
cannot recreate subscriptions, and cancelled registry cleanup blocks restart
even after worker exit. SSE's 28 units, ten loopback integrations and existing
AutoSkipGap recovery case pass; MCP's 28 units and newly unignored end-to-end
integration pass. Both packages pass strict all-target Clippy and have dedicated
parity profiles. Their acceptance-only delivery and explicit skip/fallback
policies are unchanged.

Cloudflare Radar, Open511 and HERE Traffic now own polling work before spawn,
retain in-flight HTTP/state operations across cancelled or five-second timed-out
stops, and block restart until worker and base cleanup finish. HERE resets its
shutdown signal for each run. Thirty-four source units and four actual protocol
integrations pass, including Docker-backed Radar, local Open511/HERE change
delivery and HERE rate limiting. Seven new lifecycle cases include repeated
polls, typed worker failures and an intentionally held state-store write. Local
Open511/HERE integrations now run by default; Open511 explicitly joins its mock
server. All three packages pass strict all-target Clippy and have parity
profiles. Existing polling/checkpoint fallback behavior is unchanged; this is
not coordinated replay or transactional downstream delivery.

File, Log and Profiler now serialize lifecycle calls and own processors before
spawn. The new `ReactionBase::stop_common_gracefully` preserves the existing
forwarder/queue cleanup ordering without aborting the processor on timeout.
A current-thread case retains an actual submitted blocking operation; component
cases retain pending file/statistics work, reject replacement after cancelled
startup or panic, and verify three restart cycles. Profiler now has cooperative
shutdown instead of relying on forced cancellation, while sample/report
selection remains fair. All 51 component units, seven File integrations and
Log/Profiler recovery cases pass, as do 1,125 no-default library units and six
base-worker integrations. Component and library production lint pass. File
integrations and the shared AutoSkipGap harness now use permanent shutdown; the
SSE consumer of that harness also passes. Dedicated parity profiles retain this
evidence. Queued-work draining and acceptance-only output policies are unchanged;
Log's synchronous stdout can still block its runtime thread.

SQLite now owns its blocking database worker, HTTP server and change dispatcher.
Startup waits for real connection/hook readiness and retains typed database/bind
failures. Stop closes public admission, drains accepted HTTP work, joins the
database and then queued change dispatch before base cleanup. Cancelled/timed-out
cleanup blocks replacement. Generation-bound transaction/HTTP handles cannot
switch to a replacement connection. Nineteen units and five now-default local
integrations pass, including actual five-second held-write/partial-HTTP cases,
`100 Continue` request admission, rejected late pipelined work and three restart
cycles. Shared blocking registration is also qualified and SQLite passes strict
all-target lint. These changes do not resolve SQLite's existing concurrent
transaction scoping, cancellation or pre-commit-hook publication gaps; those
remain W6 work.

HTTP and gRPC reactions now serialize lifecycle and register processors before
spawn. Standard/fixed and adaptive delivery retain actual requests and checkpoint
writes beyond the two-second stop bound. Adaptive batching is a scoped concurrent
future, not a child that can be detached or abandoned after 1.5 seconds. Fixed
gRPC final batches use the ordinary retry policy, and status transitions cannot
discard an already-dequeued result. Eight lifecycle cases cover both modes,
cancelled startup/cleanup, typed checkpoint panics, held replies/writes, final
flush and repeated exact-output restarts. All 266 units and 74 integration/schema/
recovery cases pass, with strict all-target lint. Existing explicit skip/poison
policies and acceptance-only graph boundaries remain unchanged.

A separate `NativeApplicationReaction` now delivers native envelopes directly to
a bounded application channel (`Accepted`) or an awaited callback (`Handled`).
There is no legacy reaction adapter, processor or subscription task. Cancelled
callback work remains owned through actual graph cleanup deadlines; its original
future completes on cleanup retry, and panics permanently poison that binding.
Real native-query integrations cover callback completion and three reaction-only
restarts. All 12 package units, eight integrations and nine documentation cases
pass, including the compiled native registration example; 23 pre-existing legacy
documentation examples remain ignored. Strict all-target lint and parity
enforcement pass. Ordinary query wrappers have no public root data ports: the
native example uses `computation_pipeline` and node-first connection before
waiting for creation. External application bindings cannot be reconstructed from
configuration or passed through FFI. The legacy application implementation was
intentionally left unchanged at the user's direction. This addition does not
establish durable callback effects or whole-source-transaction atomicity.

SQS now serializes lifecycle calls, registers its processor before spawning and
publishes Running only after client setup and registration. Actual SDK requests
survive cancelled/two-second timed-out stops without abort; incomplete cleanup
blocks replacement. Seventeen units and four integration cases pass, including
Standard/FIFO held-reply and three-restart cases plus both ElasticMQ graph tests.
Readiness replaces fixed startup sleeps and integration teardown awaits permanent
shutdown. Strict all-target lint passes. Render/send error policy and volatile
queue behavior remain unchanged: FIFO options and lifecycle ownership do not
upgrade the acceptance-only legacy boundary to durable or once-only effects.

Loki now builds its HTTP client before Running, owns the processor before spawn,
and serializes cleanup/restart. Held pushes and partial error responses survive
cancelled/two-second timed-out stop; an intermediate Stopping observation cannot
discard admitted input. Nine units and the real Loki insert/update/delete
integration pass. Fourteen combined HTTP/gRPC/SQS/Loki lifecycle cases and strict
all-target lint pass after extracting their shared bounded HTTP request fixture.
Test container cleanup retains its owner and reports stop errors. Loki's existing
skip/fallback behavior and acceptance-only graph contract remain unchanged.

These fixes do not cover every connector's manual workers or make an aborted
storage operation reversible. Legacy raw task-handle setters remain compatibility
APIs; new code should use ownership-before-spawn. Broader adoption and fencing of
already-submitted storage work remain W8 requirements.

Legacy lifecycle observation now retains readiness, the first unobserved failure,
and the latest status in at most three pending observations. Status bursts cannot
fill a queue and hide a later failure, even when the plugin has already moved to
Stopped. Async reporting yields cooperatively; synchronous FFI callbacks do not
block or spawn a forwarding task. Closing/replacing an observer revokes old sender
clones. Restart of the same plugin instance still requires its old workers to stop
and join; the mailbox cannot identify an unjoined worker impersonating a new run.
Ordinary MPSC senders remain usable through `.into()` and context constructors,
retaining their original queue semantics. This Rust API change adds no FFI fields.
Failed-initialization retries can rewire a closed status observer, while retaining
the first live observer. Explicit start requests join an activation already in
progress instead of reporting that component as its own blocked dependency;
readiness still has to be confirmed separately.

The ABI 0.17 SDK now contains source/reaction initialization panics and preserves
the original message after the initializer finishes unwinding. All four concrete
and boxed wrappers permanently reject reuse and further bootstrap/delivery work.
Stop/deprovision remain available, and failed cleanup retains the instance.
Graph-owned factories construct a replacement only after cleanup succeeds;
successful cleanup does not clear the damaged instance's failure. Rejected
initialization releases newly transferred stores and rejected bootstrap providers
are released without invoking the damaged source.

The current legacy ABI is 0.17. New hosts retain genuine ABI 0.16 plugins for
existing behavior; only new capabilities require rebuilding those plugins.
ABI 0.15, malformed/missing metadata, unknown future versions and wrong targets
are rejected before initialization. Native computation ABI 1.0.0 and existing
data-envelope formats are unchanged. Bootstrap
sequence counters remain provider-local across FFI; forwarding properties does
not establish a shared allocator or snapshot/log boundary.
Server's mixed-library discovery/install assertions and documentation now match
both legacy versions, with exact expected-version assertions in each fixture run;
this does not implement W9 deployment restoration.

Public operation, inspection and configuration mappings now retain typed causes
using the existing `DrasiError::Internal` carrier. Callers match `classification()`
for the public category and use `downcast_ref()` for graph/provider/worker causes.
Server uses that classification to preserve its HTTP status, code and message
mapping; its creation-health wait retains timeout and construction causes without
cancelling the accepted node. Existing direct variant matches still compile but
must use the accessor to recognize wrapped errors.

Startup/shutdown aggregates retain individual failures, lifecycle-report errors
retain member outcomes, and rollback errors retain both the initial failure and
failed cleanup. Driver watch state holds the error itself; only the informational
snapshot formats it. These changes do not implement a retry policy, guarantee
rollback, or add recovery fields to either plugin ABI.

W1 now distinguishes process restart, power loss, and permanent loss of local
storage. Each can be guaranteed, not guaranteed, or unknown. Unknown declarations
cannot satisfy a requirement, and combining required storage boundaries cannot
strengthen either declaration. These are small values, not new recovery workers
or per-message metadata.

Memory indexes, journals and consumer progress explicitly promise no crash
survival. RocksDB's current commits do not request disk sync and declare process
restart survival, not power-loss survival. Redb state writes explicitly use
`Immediate` commit durability and declare process/power-loss survival, assuming
the filesystem and device honor sync. Neither local provider promises survival
of storage loss. Undeclared/custom providers and ABI 0.16 FFI proxies remain
unknown; a persistent-looking name or the old persistence flag is insufficient.
The actual query bundle retains its declaration; independent fallback checkpoint
storage cannot inherit it. Storage declarations do not bypass the existing
session-participation checks or prove consumer effects committed once.

The declarations are available through Rust provider APIs and a fixed-size,
versioned ABI 0.17 state-store callback. The ABI 0.16 prefix offsets are unchanged.
Failed or unsupported descriptions remain explicit errors; the infallible provider
trait logs the error and returns unknown, never a stronger guarantee. The final
0.17 proxy releases its transferred table/provider, including after repeated
initialization and cleanup. Old 0.16 proxies retain their existing persistent
pointer lifetime; accepting those binaries does not promise the new cleanup or
recovery behavior. Native source admission now negotiates its own scoped service;
other native and legacy shared recovery capabilities remain unfinished.
Data-envelope formats remain unchanged.

**Whole-path assertions:** `ComputationGraphBuilder::require_recovery` and
`ComponentBatchBuilder::require_recovery` accept a consumer, failure scope and
requested guarantees. The graph follows every contributing input, including
ordinary subscriptions, instead of checking only the final pipe. Unrelated
branches remain independent. `recovery_report` on a graph or its running control
handle returns the graph ID, revision, participants and typed reasons for rejection.

These assertions check implementations; they do **not** create admission,
replay, retries, or destination idempotency. A component's declaration is captured
at lifecycle boundaries, not rediscovered for each envelope. Query and
transactional middleware declarations validate their actual atomic resource
bundle. A source using downstream progress must hold the owner's actual progress
resource: matching names are insufficient. A proven source-retention boundary
can cover a single path through stateless volatile intermediates. Hidden wakeups,
unproven state, reconverging replay paths, lossy pipes and skipped history cannot
be treated as that path. Publication also checks the selected publisher's outgoing
delivery boundaries. External-effect guarantees remain unsupported.

Factory-created components are checked after construction and before activation.
An assertion-only live change pauses its participating path; replacement checks
the newly constructed resources before processing resumes. Ordinary subscription
metadata cannot introduce an unchecked dependency. Live assertion removal or
weakening is rejected; an explicit loss-transition API remains W9 work.

Desired topology has an optional `recovery_requirements` field, omitted when
empty. Previous strict readers reject a nonempty new assertion rather than
silently discard it. Default data frames gain no fields. Existing FFI components
and ordinary subscription transports remain unknown to this analysis until their
shared recovery services are implemented; a persistent backend alone cannot
upgrade them.

W2 now has a shared Rust admission service on the existing persistent QoS channel.
It commits input, subscriber obligations and the client's retry receipt in one
existing transaction, without a second journal or worker. Persistent producer
sessions use increasing client sequence numbers; retained duplicates return the
original receipt and changed payloads are rejected. Receipt retention is bounded
independently of handled payload history. Expired numbers and retired sessions
cannot become new inputs, and retirement cannot discard subscriber obligations.
Actual provider durability is checked when enabling and reopening the service.

The opt-in service preserves timestamps and upstream context/positions in lineage,
and assigns its own persistent logical identity. Busy/full channels reject new
admission rather than build a hidden queue. Unknown commits fence the owner until
cleanup and reconstruction. Existing fast paths do not enable receipt storage or
hashing; unchanged QoS metadata omits the optional admission field. The
[QoS guide](computation-graph-qos.md#opt-in-producer-admission-rust-service)
documents limits, recovery and the current API.

`SourceAdmission` now connects Rust sources directly to their outgoing QoS channel.
The graph validates schema/stream/sequence before the single commit and does not
call the source's `next` hook or send a second copy. It derives the recovery
contract from the service and rejects mismatched identities, mixed/foreign
channels, untracked subscriptions and live transport weakening. All registration,
retirement, status, receipt and input requests share 16 waiting slots plus one
executing request, with no additional worker. Stop closes pending requests;
cancelled writes fence storage until cleanup and reconstruction.
An independent bounded failure notification preserves the first listener failure,
including during startup, without needing an admission slot or polling `next`.

The selected native compatibility policy preserves genuine ABI 1.0 fast-mode
binaries. New services require a separately versioned optional interface, not
fields appended to frozen tables or silently added to strict old metadata.
The optional service-v1 interface now binds a native source to that actual outgoing
QoS channel, not a second source journal feeding memory pipes. It exposes immutable
identity, bounded requests and failure reporting through retained C callbacks.
Only negotiated single-output factories can use the resource; revoked callbacks
cannot resurrect a replaced source, and plugin-runtime polling never performs
the graph's storage work.

Native HTTP/gRPC now offer separate versioned durable protocols for registration,
status, receipts, retirement and input. Fast submissions reject on a durable source.
Durable mode has no volatile publication queue or local sequence allocator.
Input normalization is deterministic; HTTP requires an explicit event timestamp.
Batches/streams acknowledge individual committed inputs, not atomic whole batches.
Timeouts and lost responses preserve uncertainty and must be resolved using the
original identity. Native network source configuration must match the bound stream.
The [network protocol guide](../../components/computation-plugins/network/README.md#optional-durable-admission)
defines wire shapes and errors. Legacy ingestion endpoints do not yet use this
service, and this is not a claim of once-only downstream effects or completed W2.

W5's retained-storage work caches the committed serialized window and its head
and consumer progress after the first successful load. Individual reads decode
only their selected record; cache updates follow confirmed storage commits.
Rebinding reuses the same committed cache. Cancellation/read failure cannot
publish partial reconstructed state, and revoked or uncertain writers cannot use
stale cached progress. Reconstruction rejects inconsistent head/progress, missing
interior/tail records and invalid envelopes before new acceptance. Existing
smaller-capacity reopen behavior preserves old unhandled obligations. This is
bounded steady-state journal access, not disk-only paging or completed W5.

W5 also adds opt-in `QosChannel::enable_replay` on the existing durable, lossless
channel. It stores a bounded receipt with the original output and subscriber
obligations, using the immediate persistent graph/query producer's logical
identity rather than a replay attempt's transport ID. Retries return the old
position; changed content, expired receipts and producer/generation replacement
reject. Only immediate-query post-commit timings are excluded from comparison;
inherited timings and other provenance/context remain significant. Ordinary
channels do not enable hashing or receipt storage. This does not join separate
stores into one transaction.

Built-in durable middleware, linear transactions and tracked persistent queries
now save the required replay-enabled output destinations before processing.
Membership identifies ports, component/subscriber IDs and the actual journal
UUID. It is bounded to 256 destinations and cannot change while output is pending.
**The selected replacement policy lets pending output use new destination
configuration under the same component ID**, without pinning the old generation
or URL. Changing the actual journal or removing a required destination rejects.

Membership and a cached fingerprint in existing progress commit through the
producer's existing transaction owner. Recovery rejects missing/mismatched
partners, disabled tracking and automatic query reset that could discard these
obligations. Reconciliation validates before mutation and after quiescence.
Resume requeues saved output without recomputing it; retrying all branches uses
their bounded receipts instead of a separate per-branch completion journal.
Queries confirm the whole batch only after forwarding finishes.

Replay configuration must precede the first publication or pipe binding.
Old version-1 receipt records upgrade transactionally to version 2 with a
persistent journal UUID. The default transformer hook does not claim support;
these producer services still need plugin SDK/ABI adoption under W7.

The selected same-store ownership model keeps storage at graph lifetime, not
producer lifetime: stopping/replacing a producer must not prevent its pipe from
finishing deliveries. The new Core `ComputationTransactionGroup` supplies the
underlying ownership and commit mechanism. One processor and up to 256 active
journals use one proven storage session and one lock. Up to 256 journal mutations
can be staged with processing; committed cache updates finish before another
member can start a transaction. Rejected staging rolls back; interruption or
failed cache publication blocks the entire group until reconstruction.

Healthy members stop independently. Dropping the group owner also blocks new
work, but members retain the ability to await storage work and finish cleanup.
The mechanism uses actual group membership, not matching paths or names.
It supports both live and scheduled query hooks without another evaluator.
Each member's outbox now has a separate persisted namespace, including clears
and retention operations. Journals can keep metadata there without losing it
when the processor clears its own outputs or checkpoints. Groups require
dedicated storage; this does not silently migrate existing standalone output.
Library `SharedStorageGroup` now supplies graph-owned index resources and QoS
journals. Participating pipes declare the same storage resource through
`shared_storage`, with actual group membership checked before binding. Built-in
query, scheduled-query, durable middleware and linear-transaction paths reserve
capacity before processing and append to participating journals in their
existing commit. Multicast deduplicates reservations as well as publication.
Journal reconstruction shares the storage gate; subscriber progress/retirement
leases prevent generation changes from racing unfinished writes. Group failure
wakes blocked publishers and readers. Quiesced producer stop keeps the journals
available for acknowledgement. Plugin-facing services and packaged Server
resource construction remain W7/W9 work.
There is one processor per group, not arbitrary transactions spanning queries.

Existing native transaction participants can already run inside that host-owned
linear container, using their existing revocable state mailbox. No shared-storage
handle or new ABI interface crosses FFI. Separate-library tests now exercise two
native arithmetic steps and two shared journals, including cancellation and
required fresh-process exits before/after commit. Genuine preserved ABI 1.0
participants retain this behavior. This is host-owned transactional participation,
not a new recovery service for arbitrary standalone native transformers.

**Evidence for this implementation segment:**

| Check | Result |
|---|---|
| Storage-declaration unit suites | 827 Core, 1,050 `drasi-lib` and 38 Redb state-store cases passed before the whole-path changes. The library cases also passed without default features. |
| Storage-declaration recovery suites | 16 RocksDB transaction, 12 consumer recovery, 15 retained-pipe and 14 QoS integration cases passed. Core and RocksDB also compiled without the computation feature. |
| Whole-path regression suites | The pre-worker run passed 1,067 library units, 19 assertion cases, 20 transactional-transformer cases, 21 enabled persistent-middleware cases and the affected runtime suites. It includes overlap-closure ordering, independent-path isolation, poisoned unrelated metadata and rejection of unbound asserted publication before mutation. |
| Worker and restart regression suites | The final no-default library run passed 1,078 cases. The RocksDB/middleware run passed 1,079 library cases, six base lifecycle cases and the affected addition, controller, deployment, reconciliation, assertion and transactional/middleware recovery suites. Nine worker-helper cases cover registration, cancellation, timeout, panic and group cleanup. Five selected source unit suites passed 308 cases; 16 enabled WAL integration cases passed and nine existing cases stayed ignored. These are not database crash or whole-plugin qualification results. |
| Actual transactional replay | A retained RocksDB source, stateless intermediate and real transactional transformer reopen without repeating committed processing. Saved output may be delivered again; this is not an external exactly-once claim or a fresh-process crash test. |
| Earlier W7/W8 boundary suites | 129 Host SDK and 68 plugin SDK unit cases passed; 90 selected no-default controller, construction, reconciliation, resource, lifecycle and pipe-matrix cases passed. |
| Separately rebuilt plugin libraries | 82 Host SDK integration cases passed; one existing case remained ignored. Tests run serially because global plugin log callbacks are shared. |
| Worker-era rebuilt plugin libraries | Rebuilt the existing test cdylibs. 129 Host SDK units, 12 bootstrap-provider, 56 library integration, two actual C callback and 11 native-pipeline cases passed; one existing integration case stayed ignored. |
| Typed public/Server errors | 1,086 no-default library units, 16 controller, seven deployment, nine instance, 18 reconciliation and 19 recovery-contract cases passed. Server passed 360 units and nine real-Router creation/catalog cases. Coverage includes retained provider/worker causes, multiple failures, retryable cleanup ownership, exact creation timeout and unchanged HTTP classifications. |
| Legacy ABI 0.17 boundary | 133 Host SDK and 72 plugin SDK units passed. Includes all 27 storage declarations, fixed prefix offsets, provider panic/invalid output, real Redb/unknown/volatile evidence and last-owner release. The loader rejects old/future/malformed/oversized versions, missing metadata and wrong targets before an aborting initialization fixture can execute. |
| Genuine old/current plugin compatibility | The new host passed 59 integration cases against each of preserved ABI 0.16 and rebuilt ABI 0.17 binaries; one existing stress case stayed ignored in each run. The rebuilt run also passed three snapshot/bootstrap, 12 bootstrap-provider, two C callback and 11 native-pipeline cases. Both versions reject a durable-admission assertion despite an injected Redb store. Repeated real source/reaction initialization releases shared stores under 0.17 and preserves the documented old 0.16 lifetime. Server passed all 17 native/mixed-library cases against each version with exact metadata assertions. |
| Initialization isolation | 74 SDK unit cases and four new Host SDK regression cases passed. In-process probes cover all four concrete/boxed source/reaction wrappers, original panic/lifecycle errors, completed unwind, rejection of further work, transferred-resource cleanup, failed-stop ownership and exactly one fresh instance after successful cleanup. A separately rebuilt `snapshot-test` reaction cdylib also verifies panic containment, partial-state ownership through cleanup and rejection of reuse. The fixture is required, not silently skipped. Rebuilt ordinary plugins passed the existing Host SDK lifecycle/compatibility suites, and the three streaming-bootstrap cases still pass. |
| Shared producer admission | The QoS integration binary passed 23 test functions, including one child-process fixture. New cases cover bounded sessions/receipt expiry, payload conflicts, lineage, actual storage requirements, twelve persisted admission corruptions, cancellation and lost commit responses. A parent case requires three abrupt child-process exits: before commit, after commit and after successful acceptance. Each reconstruction resolves input and receipt together without a second acceptance. Two library unit cases check exact configuration limits and serialized maximum receipt storage. This is process-loss evidence, not power-loss or endpoint qualification. |
| Graph-owned producer service | The expanded QoS binary passed all 30 functions. Seven graph cases cover pre-commit schema validation, derived recovery evidence, single-append fan-out, registration/status/receipt/retirement, stop/restart, 16-slot saturation, abandoned callers, invalid bindings and live weakening, scoped activation failure, and reconstruction with stopped consumers. Eight interruption combinations cover input/registration writes before/after real commit during source stop or graph cancellation. The channel-exclusive binding unit passed under its exact selector. These results preceded the native endpoint cases below and do not complete whole-plan qualification. |
| Native admission and compatibility | Three ABI units, 14 native-SDK units, 134 Host units, three C-interface admission integration cases and 11 rebuilt native pipeline cases passed. Genuine preserved ABI 1.0 standard/network binaries separately passed their 11/5 existing cases. New callbacks retain/release correctly, enforce 16 outstanding operations, preserve unknown writes after missing/malformed responses, report worker failure and revoke before native release. |
| Native network recovery | Actual rebuilt-library HTTP/gRPC tests cover partial acceptance, stopped-consumer replay, duplicate/conflicting/expired receipts, retirement and two required abrupt child exits without saved client receipts. An eight-case matrix blocks before/after real RocksDB input commit and exercises client timeout or source stop for each protocol; reconstruction either accepts the previously uncommitted input or returns its original receipt, with one journal acceptance. This is process-restart evidence, not power-loss or external-effect qualification. |
| Native network packaging and lifecycle | The full rebuilt package passed two listener ownership/panic units, 11 loopback functions and four wire cases. A mismatched configured stream rejects before listening. The packaged protobuf generates without a sibling workspace checkout, and workspace builds reject schema-copy drift. |
| Bounded retained-store access | All 19 retained-pipe functions passed, including 512 append/read/acknowledgement cycles across two owners with exactly one journal/head/progress read per owner, six interrupted-load schedules, eight real persisted corruptions and strict/explicit-skip pruning across reopen. The existing capacity-reduction, unknown-commit and generation-revocation cases still pass. Adjacent suites passed 21 middleware and 21 transactional-transformer cases, plus the required fresh-process crash matrix using the legacy RocksDB adapter; its child helper remains explicitly invoked, not counted as a standalone passing case. |
| Stable output handoff receipts | Eight library cases passed for graph/query identities, exact retry content, query timing overlays, bounded pruning/reconstruction, exact receipt/name limits, rejection of inherited identity/sequence and eight persisted corruptions. New logical output still has to advance transport order. A real graph routes a replay attempt through persistent multicast without another append or delivery. The expanded QoS binary passed 33 cases; its additional ignored helper was explicitly invoked by a parent requiring process exits before and after the real commit. Four cancellation/revocation schedules resolve the receipt with the event. A real transactional query reopens after only one of two separate output stores accepted its result; replay adds no second entry to the first store, delivers to the second and then confirms the producer. This is bounded-receipt evidence, not completion of W5 or power-loss qualification. |
| Durable output destinations | Twelve binding/replay units and 36 QoS integration cases passed. Coverage includes journal UUID upgrade, pre-binding configuration races, a real graph rejecting a new journal without revision mutation, and pending output following a replacement's new configuration. Producer cases cover binding commit failure/cancellation, live resume without recomputation, both RocksDB adapters and two required fresh-process exits. Five persisted middleware corruption cases and seven query corruption/disabled-tracking/reset cases retain typed errors without rewriting the damaged state. This remains bounded separate-store recovery, not completed W5. |
| Destination-binding regressions | Without default features, 16 controller, 18 reconciliation, 19 recovery-contract, 11 query-fault, 13 query-recovery, 22 middleware-recovery and 23 transactional-transformer cases passed. The producer crash helper is invoked and checked by its parent, not counted as a standalone pass. |
| Shared transaction kernel | 836 Core units, 42 RocksDB units, 20 RocksDB transaction cases and 88 shared behavioral cases passed. Seven ownership/mutation units cover independent lifetimes, cancelled waiters, interrupted storage/cleanup, owner drop, exact limits and stage-versus-commit failures. Two namespace units pin disjoint stored keys and reject invalid keys on every operation; real RocksDB confirms processor clearing and journal retention cannot erase another member's records or metadata. Real storage also covers live/scheduled group commits, rollback and reconstruction; the crash parent requires actual exits before/after commit and checks indexes, input progress, producer output, live rows and journal output together. Its ignored child is explicitly invoked, not counted as a standalone pass. Scoped Clippy and the expanded parity-runner self-check passed. This does not qualify graph/QoS integration, multiple processors or power-loss survival. |
| Shared graph/QoS integration | All 49 ordinary QoS cases passed. Thirteen new functions cover actual query/middleware/linear factories, scheduled output, multicast reservation deduplication, backpressure outside storage transactions, encoding rollback, quiesced producer stop, group-wide failure wakeups, endpoint revocation, subscriber rebinding, gate-protected reconstruction and ten persisted corruption/reconfiguration faults. Eight cancelled producer commits and eight required child-process exits cover before/after live query, scheduled query, middleware and linear commits. Eight acknowledgement/retirement schedules resolve progress after interruption. Both ignored QoS crash helpers are explicitly invoked by required parents, not counted as ordinary passes. Core/library production Clippy passed with the existing two command-line allowances. This is Rust graph/RocksDB process-restart evidence, not packaged Server, plugin recovery-service or power-loss qualification. |
| Shared handoff transitions | Two additional functions passed: eight partial-publication interruptions across mixed shared/separate output stores, plus live replacement of all four producer kinds, drained component removal without storage loss, rejection of a still-referenced owner removal and explicit unused-owner cleanup. Reconstruction keeps one acceptance in each output journal. |
| Shared staged writes and progress crashes | Four producer cases fail after actual event writes reach both distinct journal namespaces, before the second mutation finishes; reconstruction preserves no partial publication and reprocessing commits once. A separate parent requires four exit-80 child crashes before/after acknowledgement and retirement commits. Exact cursors, retired identities, retained delivery and original acceptance receipts survive reconstruction. |
| Shared gate cancellation | A new Core case reproduced queued storage/member/inspection waits remaining blocked after group failure. Both gate waits now use the existing failure notification, without changing the ordinary transaction path. All eight group units pass; an additional real RocksDB case proves queued append, acknowledgement, retirement and reconstruction fail before active processing finishes, without mutating recovered journal state. |
| Shared handoff regressions | After the gate-wakeup fix, 837 Core units, 42 RocksDB units, 20 RocksDB transaction cases, 88 shared behavioral cases, 54 ordinary QoS cases, 1,102 library units and the affected controller, reconciliation, query, middleware and transaction suites passed. All three ignored QoS helpers are explicitly driven by required parents. Integration-target Clippy passed with warnings denied and no lint allowances. The architecture inventory contains 1,062 types, 2,587 stored relationships, 311 namespaces and 16 diagnostics; generation/check and the expanded parity-runner self-check passed. |
| Native shared transactions | Rebuilt current and genuine preserved ABI 1.0 standard libraries each passed 13 pipeline cases and the explicitly driven shared-commit crash helper. Two required exit-81 children per run cover before/after actual shared commit, preserving two native participants' state and two journal acceptances. Native integration-target Clippy and parity self-check passed. No new ABI or plugin-owned shared-storage service is implied. |
| Shared operation completion | Ten new library cases passed, plus one explicitly driven helper requiring exit-82 crashes before/after progress commit. Coverage includes partial batches, independent streams, real QoS acknowledgement only after all operations complete, exact retry IDs after process exit, lost commit responses, cancelled effects/delays, bounded retry/receipt settings and six persisted corruptions. All ten existing output replay cases still pass after extracting their shared identity/digest implementation; production Clippy passed. The durable effect fixture verifies preserved IDs, not a qualified external database or HTTP deduplication contract. |
| Delivery regression and destination positions | Before destination-position additions, 1,111 no-default library units and 54 ordinary QoS cases passed, with required crash helpers explicitly driven. After the additions, all 13 delivery units and the required helper pass, including independent stream-key changes, sparse ordinals versus vector indexes, strict serialized positions and full-width sequences. Production library Clippy passes with the existing evaluator allowance. |
| Transactional PostgreSQL effects | Seven new real PostgreSQL/RocksDB cases pass, covering lost destination responses and local reconstruction, concurrent initialization/duplicates, effect failure and cancellation after SQL writes, exactly 4,096 streams, unsigned maximum sequences, gaps, changed/expired batches and corrupt cursors. A required actual database SIGKILL preserves committed effects and deduplicates unconfirmed delivery after database/local reconstruction, despite asynchronous session defaults. The adapter uses explicit READ COMMITTED and synchronous commit; connection drivers are owned and joined. Production and integration-target Clippy pass with warnings denied and no allowances. This qualifies the separate Rust adapter, not the legacy reaction, HTTP, plugin ABI or power-loss survival. |
| HTTP completion reference | Six real HTTP cases extend the destination binary to 13 passes. They cover actual response timeout after SQL effects, reconstruction of both sides, missing remote progress after destination commit, partial batches, bounded admission and cancellation of active SQL. Forged content/scope and queued, forged or oversized confirmations reject; exact byte/timeout limits and explicit retry classifications are covered. A whole batch uses one request per attempt rather than per-operation full-envelope encoding. A nontransactional fixture deliberately repeats its effect, preserving the documented weaker guarantee. Network and PostgreSQL production/integration Clippy pass without allowances. Factory/ABI exposure is still pending. |
| Atomic consumer state | Nine additional library cases pass, bringing operation completion to 22 ordinary cases and two explicitly driven exit-82 helpers. Real RocksDB covers state/completion commit and rollback, independent streams, typed retry classification, cancellation, uncertain commits, failed rollback retaining both causes, mode compatibility/corruption and before/after-commit process exit. A connected QoS graph reconstructs partial state and retries only unfinished operations before acknowledging the whole envelope. The destination binary passes all 14 cases, including HTTP construction rejecting a transactional-state runner before external handling; strict component/integration Clippy passes. The initial five failures were test inspection reads outside an active storage session, fixed without weakening provider requirements. Per-operation state atomicity is not W6 whole-source-transaction visibility or plugin-service qualification. |
| Consumer-state regressions and reaction inventory | All 1,123 no-default library units pass, with both required delivery crash helpers explicitly driven. Production Clippy, architecture generation/check and parity self-check pass. The refreshed inventory has 1,080 types, 2,626 stored relationships, 313 namespaces and 16 diagnostics. The [bundled reaction inventory](../../components/reactions/README.md#bundled-completion-boundaries) records all 19 legacy and three native sink boundaries, including existing skips, fallbacks and checkpoint limitations. No legacy adapter is upgraded and no existing failure policy is changed; inspection is not destination crash qualification. |
| Server mixed/native ABI and single-runtime suites | 27 cases passed; one existing case remained ignored. |
| Review and qualification tooling | Formatting, whitespace, architecture regeneration/check and runtime-parity runner self-check passed. |

The latest Core/library production Clippy check passes with dependency linting
disabled and only `clippy::question_mark` allowed for unchanged evaluator code;
the integration target passes with warnings denied and no allowances.
Earlier scoped runs below used their recorded toolchain-specific allowances.
No source or configuration exemptions were added. Ignored cases are not counted
as qualified; required crash helpers have explicit parent drivers.
The worker-era scoped check covers the library, both SDKs and all targets of the
five changed source plugins. Earlier checks cover Core, RocksDB, Redb and the QoS
integration target. The regenerated architecture snapshot contains 1,017 types,
2,459 stored relationships, 303 namespaces and 16 explicit extraction diagnostics;
the new diagnostic is the intentionally generic `WorkerCompletion<T>` payload.
After shared admission, the refreshed snapshot contains 1,025 types, 2,475 stored
relationships, 304 namespaces and the same 16 diagnostics. The no-default
regression run passed 1,086 existing library cases, two foundation-matrix cases,
six pipe-profile cases and 19 recovery-contract cases; the two new admission
unit cases also passed under an exact-name selector.

The graph-admission update passed 1,088 existing no-default library cases,
16 controller, 18 reconciliation and 19 recovery-contract cases. Its new
channel-exclusive binding unit also executed separately. Scoped all-target
library/native-SDK Clippy passed with the same two existing allowances.
The regenerated snapshot now contains 1,029 types, 2,499 stored relationships,
305 namespaces and 16 extraction diagnostics; the parity runner self-check passes.

After bounded journal access, the snapshot reached 1,030 types and 2,503 stored
relationships. The output-receipt update contains 1,035 types, 2,512 stored
relationships, 306 namespaces and the same 16 diagnostics. Its eight replay
units, five query-codec compatibility cases, 33 QoS cases, scoped Clippy,
formatting and parity self-check pass. The two required output-receipt crash
children are executed by their parent, not counted as ordinary passing cases.

The destination-binding update passes 1,101 no-default library cases, the
122 affected runtime/producer cases above and 36 QoS integration cases. Scoped
Clippy passes with the same two existing command-line allowances. The refreshed
architecture inventory has 1,042 types, 2,535 stored relationships, 307 namespaces
and 16 diagnostics. Formatting, generated-artifact checks and the parity runner
self-check pass; both output crash helpers are explicitly tied to required
parent drivers in the ignored-case ledger.

New regression cases are protected by the existing runtime-parity ledger.
Compatibility reruns can select preserved binaries with
`DRASI_HOST_TEST_PLUGINS_DIR` and `DRASI_HOST_TEST_EXPECTED_LEGACY_ABI=0.16.0`;
Server uses `DRASI_SERVER_TEST_PLUGINS_DIR` and
`DRASI_SERVER_TEST_EXPECTED_LEGACY_ABI=0.16.0`. Without an expected-version
override, assertions require the current ABI exactly. These local compatibility
runs do not replace a published old/new release-artifact CI matrix.
The whole-plan crash matrix, complete plugin/provider qualification and fast-path
performance gate remain outstanding. No work package is marked complete merely
because these individual fixes pass.

This plan addresses the nine resilience issues summarized in the recent review.
It complements the [backlog](computation-graph-backlog.md) and the
[schemas, ports, and pipes guide](computation-graph-schemas-ports-pipes.md);
it does not replace their descriptions of what works today.

## 1. Recommended direction

**Keep a simple, fast in-memory path. Add stronger guarantees by selecting pipe
policies and capable storage providers, with ComputationGraph supplying the
necessary shared services to participating plugins.**

The work should extend the existing graph, transaction owner, retained/QoS
pipes, source/reaction helpers and provider interfaces. It should not introduce
another graph executor, another query engine, or a separate reliability
implementation inside every plugin.

The main deliverables are:

- A validated description of what each configured path actually guarantees.
- Shared durable admission, transactional processing, completion and recovery.
- Correct snapshot-to-live handover and optional source-transaction grouping.
- Reliable lifecycle ownership and explicit errors across plugin boundaries.
- Reconstructable Server deployments and an executable guarantee matrix.

**Fast mode means no added recovery machinery, not disabled correctness.**
Schema checks, bounded-resource rules, truthful errors, ordering promised by the
selected pipe, and safe ownership still apply. Existing configurations retain
their current meaning; adding this feature must not silently enable persistence,
retries, additional queues, or a different execution mode.

### User choices

These are explanatory recipes, **not proposed literal YAML values or a new
mandatory global mode switch**. They compose per path rather than forcing one
reliability level on an entire instance.

| User intent | Principal choices | Result and cost |
|---|---|---|
| **Fast, best effort** | Existing volatile pipe; memory provider; no recovery services | No crash-recovery promise. No added durable recording, recovery checkpoints, duplicate tracking or retry workers. Backpressure or deliberate dropping remains the user's pipe choice. |
| **Recover accepted work** | Persistent retained/QoS delivery, lossless retention, suitable source and consumer recovery bindings | Accepted work remains recoverable within the declared failure and retention contract. Delivery attempts and external actions may repeat. Storage, acknowledgements and replay have a cost. |
| **Atomic internal processing** | Recoverable transport plus participating components and an atomic state/progress/output provider | Each input's participating committed effects happen once. An interrupted attempt can run again. This does not make all branches visible simultaneously. |
| **Preserve a source transaction** | Explicit transaction-grouping policy; a source exposing boundaries; compatible processing/storage | All changes in that source transaction become visible at the promised publication boundary together. Buffering and increased latency are explicit costs. |
| **Prevent duplicate external effects** | Recoverable delivery plus a destination-specific idempotency or transaction contract | Stable delivery identities prevent repeated effects only within the destination's documented scope and retention window. |

Saving desired configuration is an independent option. A user may persist the
topology while choosing memory-only processing, or rebuild a durable processing
graph programmatically without using the configuration store.

### Execution sequence

Work-package numbers below match the nine reviewed issues, not execution order.
Implementation and failure tests travel together in each package.

| Stage | Deliverables | Prerequisites |
|---|---|---|
| **A. Establish the contracts and fix unsafe boundaries** | W1 guarantee/settings model; W7 explicit errors and versioned services; W8 lifecycle ownership | W1 and W8 can start independently. W7 error/status fixes can start immediately; its new service capabilities require W1. |
| **B. Make shared recovery usable** | W2 durable admission; W5 transactional handoff; then W3 consumer completion | W2 and W5 require W1, W7 and W8. W3 uses W5's shared processing contract and the completed plugin/lifecycle foundations. |
| **C. Complete handover and operation** | W4 snapshot/replay handover; W9 Server restoration | W4 uses W2's source progress/admission services. W9's first supported end-to-end configurations require W2, W3, W4 and W5. |
| **D. Complete transaction visibility** | W6 whole-source-transaction handling; expose that option through W9 | W6 requires W4 and W5. It is optional for users, but is a required deliverable of this plan. |
| **Release gates throughout** | Correctness, compatibility, fast-path cost and real restart evidence | Each package has its own exit criteria. Final end-to-end qualification depends on all applicable packages; it is not postponed until the end. |

Start with one reference implementation of each contract, then qualify plugin
adoption. Do not require all connectors to be rewritten before any feature can
ship. Conversely, do not describe all plugins as recoverable because one
reference connector works.

## 2. Settings should select services, not duplicate plugin logic

At graph construction or reconciliation, resolve the selected settings into a
fixed execution path and scoped services. Do not repeatedly discover capabilities
or interpret a large policy object on every envelope.

Extend the existing pipe families with optional policies and provider-backed
transaction participation. Do not create a separate pipe implementation for
every combination of durability, retention and processing guarantees.

| Setting belongs primarily to | What the user controls |
|---|---|
| **Pipe/connection policy** | Volatile versus recoverable delivery; capacity and byte budgets; backpressure versus deliberate loss; retention; acknowledgement boundary; retry/gap policy; required subscribers; requested transaction grouping. |
| **Storage provider/binding** | Persistence and failure-survival guarantee; atomic participation; storage identity and ownership; state, journal and progress resources; retention/storage budgets. |
| **Component binding, only where necessary** | Ingress acceptance boundary; upstream snapshot/replay contract; transaction participation; external idempotency binding. The graph should derive these bindings from compatible pipe/provider selections where unambiguous. |
| **Graph or selected path requirement** | An optional assertion of the intended guarantee, checked against all relevant producers, stateful stages, branches and consumers. |
| **Management storage** | Whether desired definitions and configuration-request receipts survive restart. This must not change data-plane durability implicitly. |

A persistent provider's presence must not automatically activate every feature.
It describes available capability; the selected policy describes requested
behavior. Explicit plugin settings that conflict with that policy must fail
validation, not be silently overridden.

For example, selecting a recoverable first connection for a source supporting
host admission should bind that source to durable admission. A native HTTP
plugin then awaits the host's acceptance receipt before responding. If a plugin
acknowledges its own volatile queue instead, the graph must reject the stronger
ingress promise. A durable pipe cannot retroactively protect an earlier response.

Mixed branches are valid: a source can feed a recoverable business consumer and
a best-effort monitor. Validate and report their guarantees separately. Required
subscribers constrain lossless retention; an optional monitor must not silently
become a required subscriber or release another subscriber's obligation.

### Shared runtime services

Names here describe responsibilities, not finalized Rust interfaces.

| Graph-owned service | What a correctly written plugin does |
|---|---|
| **Admission and producer progress** | Decode/validate external data, submit through the bound service, and acknowledge only the returned acceptance boundary. Supply upstream positions or client retry keys where required. |
| **Transaction-scoped processing** | Keep commit-sensitive state in the supplied context and return outputs. Do not perform hidden external effects or start independent transaction workers. |
| **Delivery and consumer progress** | Report actual handling, retryable failure, permanent failure or uncertain outcome. Provide destination-specific classification and idempotency behavior. |
| **Snapshot/replay coordination** | Establish a real source boundary, stream the snapshot, resume changes and report progress through the shared contract. |
| **Worker scope and readiness** | Register owned workers/resources, signal actual readiness, and implement protocol-specific closing within the host-owned lifecycle. |

Implement these as small modules and scoped resources accessible through the
existing runtime/SDKs, not as more provider-specific branches in the central
controller. No plugin library should implement a second replay coordinator.
Pure retry/backoff helpers can be shared without requiring a storage service.

Native and legacy adapters must use the same semantics. Cross-library access
uses versioned, bounded, revocable host capabilities, never borrowed Rust
transactions, trait objects or storage pointers. Service completion must remain
serviceable while a plugin data operation or admission call is waiting, including
on Tokio's current-thread runtime.

## 3. Work packages and completion criteria

### W1. Validate the whole recovery path

**Addresses:** issue 1; CG-01 and the capability part of CG-03/CG-11.
**Complexity: high. Disruption: moderate with opt-in requirements.**

Define separate guarantees for acceptance, replay, committed processing,
publication and external effects. Define the failure model: memory lifetime,
process restart, power loss where supported, and machine/disk loss only where a
provider genuinely supports it. Do not infer these from a backend name.

Extend provider and progress-store descriptions with the guarantees they can
actually supply. Reuse `TransactionDomain` and `AtomicResultTransaction` to prove
that resources participate in the same real transaction; equal configurations or
directory paths are not proof. Unknown or legacy durability is not affirmative
evidence.

Resolve requirements across the graph, including stateful middleware, fan-in,
fan-out, timers and recovery sources. A volatile intermediate connection can be
recoverable if a proven upstream replay/retained-output boundary covers it; a
rule that merely requires every edge to use disk would be unnecessarily costly.
Reject lossy retention or skip policies that contradict a no-loss requirement.

**Done when:** construction, ordinary APIs, native factories and reconciliation
agree on acceptance/rejection. Inspection explains the effective guarantee and
the exact incompatible participant. Strong guarantees cannot silently downgrade.
Existing fast configurations retain their path and documented behavior.

### W2. Provide shared durable admission

**Addresses:** issue 2; CG-02 and source services in CG-11.
**Complexity: high. Disruption: high at the acknowledgement boundary.**

Build a producer-scoped admission service using existing WAL/journal and progress
mechanisms. Persist the accepted input, stable identity and receipt information
before durable success. Keep bounded admission, ordering, replay and pruning in
one owner. Do not append the same event independently through both a plugin WAL
and a new host journal without a documented need.

Define stable client retry keys and payload-conflict checks. Keep logical event
identity distinct from replay transport sequence. Bound duplicate tracking:
document its replay/retry window and reject expired or ambiguous retries rather
than assigning them an apparently safe new identity. An uncertain commit remains
explicitly uncertain until receipt lookup or recovery resolves it.

**Client retry decision:** durable HTTP/gRPC clients register a producer session
and submit increasing event numbers within it. Retrying a retained number with
the same payload returns its original receipt; a changed payload is a conflict.
Expired numbers and retired sessions are rejected, never interpreted as new
input. Fast clients retain their existing protocol without session setup.

Adopt the service in native HTTP/gRPC first. Then migrate supported legacy
application/HTTP/gRPC/OTEL durable paths, preserving their wire formats and
configured response meanings. Existing database-log sources should retain their
upstream replay mechanism rather than incur an unnecessary second full journal.

**Done when:** crashes on either side of append and response leave every durably
accepted input discoverable; retries preserve identity; capacity exhaustion does
not accept unsaved input. Batch responses identify any accepted prefix. Fast
admission creates no journal, receipt index or pruning worker.

### W3. Centralize actual completion and consumer progress

**Addresses:** issue 3; CG-03, consumer services in CG-11 and reaction review.
**Complexity: high. Disruption: high for existing failure policies.**

Build on `ReactionBase`, `CheckpointState` and `CheckpointedSink` to provide one
optional delivery runner and progress contract usable beyond legacy query-result
queues. Persist progress only after the promised handling boundary. For
transactional consumers, commit state and handled-input identity together.

Make queued acceptance distinct from handling. An acceptance-only legacy plugin
must gain an explicit completion interface or remain ineligible for a handled
guarantee. A wrapper cannot manufacture that completion.

Handle partial batches and multiple streams without advancing a checkpoint past
unfinished work. Retry transient failures with bounded, cancellation-aware
policy; leave strict failures pending or stop explicitly. Skipping must be an
explicit weaker policy. If a future quarantine facility transfers responsibility
to another durable destination, report that outcome, not successful delivery to
the original destination.

For external effects, provide stable per-operation idempotency keys and a
documented destination adapter contract. Start with an idempotency-aware HTTP
reference endpoint and a database sink that atomically records the delivery ID
with the effect. Ordinary destinations remain explicitly at-least-once.

**Done when:** the same identity is used after restart and partial delivery;
progress-write failure cannot skip unfinished actions; destination deduplication
is verified where claimed. Inspect every bundled reaction's declared completion
and failure behavior; either qualify it for the stronger contract or report its
unsupported capability. Do not silently change existing skip behavior.

### W4. Make snapshot, live input and restart one coordinated handover

**Addresses:** issue 4; CG-07 and CG-24.
**Complexity: high across connectors. Disruption: high for recovery formats.**

Reuse `SourceBase` bootstrap boundaries, query bootstrap watermarks and source
progress. Specify explicit snapshot-in-progress, completed-boundary and live
states, with recoverable progress where requested. Preserve per-source positions
and partition vectors; do not replace them with a single invented graph cursor.

Require a connector to establish a valid snapshot/change-log boundary or a
documented equivalent. Buffer or replay concurrent changes with bounded storage.
Partial or failed snapshots must not count as successful completion or as
evidence that missing records were deleted. Define how repeat initial loads
replace state without duplicating completed effects.

Forward all bootstrap settings and context through W7's versioned interfaces.
Prove the contract first with PostgreSQL, then extend the connector matrix.
Sources without sufficient history remain explicitly unsupported for gap-free
handover; a polling connector can offer a current-state refresh without claiming
that it captures every intervening event.

**Implemented PostgreSQL reference:** an optional borrowed `BootstrapState`
service persists at most 64 KiB of connector initialization metadata in the
query's actual transaction domain. Intent survives explicit reset; final metadata
commits with source watermarks and the completed-bootstrap marker. Ordinary
providers retain their existing methods and do not perform these writes.

The native source's `coordinated` constructor returns a paired snapshot provider.
At the user's direction, this mode creates dedicated UUID-suffixed slots after
persisting their ownership. An exported PostgreSQL snapshot and its consistent
WAL boundary replace independently sampled snapshot/LSN values. A directly owned
connection imports the snapshot and reads one bounded row at a time; concurrent
changes stay in the slot's WAL until live processing commits them. Snapshot
rows use the same native key/value conversion and do not pretend to constitute
one upstream transaction.

Incomplete loads fail under strict recovery. Explicit `AutoReset` can restart an
unfinished snapshot and replace only its recorded unfinished slot. Completed
slots are reused, not replaced. Missing history, changed publication/schema
bindings and absent ownership state fail explicitly; completed-slot retirement
is still an operator action before query deprovisioning. No administrative
failover/storage-loss guarantee is inferred.

Real PostgreSQL/RocksDB cases cover concurrent updates/deletes/inserts during a
slow snapshot, cancellation, bounded-row rejection and limit increase, unchanged
external slots, strict versus reset recovery, missing completed history, changed
publication/column definitions and process exits during/after initial loading.
Direct and factory graph pipelines exercise both replay-only and coordinated
modes. This reference currently requires PostgreSQL 15+, ordinary
non-partitioned tables and unfiltered complete publications. Legacy bootstrap,
native plugin exposure, other connectors and the broader failure matrix remain.

**Implemented native SQLite reference:** its paired `SqliteSnapshot` persists
query-owned intent before creating a coordinated journal in the database.
A retained blocking worker streams a consistent initial read with bounded rows,
queueing and duration, using the live source's exact key/value conversion.
Completion and a real epoch-qualified sequence-zero watermark commit together.
Native SQL admission begins only after the query is ready; external writes are
unsupported, rather than assumed captured. Strict recovery rejects partial loads;
explicit reset can retry unfinished initialization after increasing limits, but
cannot recreate completed ownership or change an existing journal's mode.

The shared bootstrap interface now has a defaulted asynchronous cleanup hook.
Query stop and deprovision await it before releasing storage or clearing state.
SQLite's worker and client lease survive cancelled/timed-out cleanup; stream drop
requests cancellation without detached cleanup tasks. Actual SQLite/RocksDB cases
also exposed a query checkpoint-view handle retaining the old RocksDB lock after
failed startup. Retirement now releases that view only after awaited cleanup and
publishes replacement handles only after successful construction. Same-object
restart/reset and old-snapshot generation fencing have explicit regressions.
The SQLite/RocksDB cases
cover empty/populated initial reads, composite typed keys, live updates, restart,
limits/reset, missing identities, blocked cleanup and direct/factory graph paths.
Real process exits after persisted intent, partial row loading and atomic
completion qualify those three initialization boundaries.

**Done when:** changes around snapshot start/end, crashes, slow sources, missing
history and resumed subscriptions cannot create an unexplained gap. Missing
history either stops strict recovery or invokes an explicitly selected reset
whose weaker semantics are visible.

### W5. Complete transactional handoff and reliable fan-out

**Addresses:** issue 5; pipe/transaction review and CG-21 through CG-23.
**Complexity: high. Disruption: high for storage and acknowledgement changes.**

Extend the existing `ComputationTransaction` and `TransactionTransformer`
state/progress/output commit, rather than introducing another transaction owner.
Preserve the existing query evaluator and its atomic schedule/result handling.

Support two explicit arrangements:

- **Same transaction domain:** a transaction-aware journal append can join the
  producer's state/input-progress commit if the provider proves participation.
- **Separate stores:** commit producer state and an outgoing-message record
  together; publish that record to the durable pipe using stable identity; retire
  it only after confirmed durable acceptance. The receiver commits its own state
  and handled-input record together, then acknowledges transport separately.

The outgoing-message record is the **outbox**; the handled-input record is the
**inbox**. These are not extra stores for fast mode. Do not describe separate
commits as atomic or require every graph component to share one database.

Persist required branch obligations with output. A partial fan-out resumes only
unfinished obligations or safely deduplicates repeats; it must not recompute a
committed effect. Preserve QoS's single append and independent subscriber
progress. Disconnection is not retirement. Cancellation, unknown acceptance,
identity changes and expired history must have explicit recovery outcomes.

Keep storage access bounded: avoid rereading the entire retained journal for
each append/delivery. Capacity protects both unhandled pipe history and
unconfirmed component output; do not hide additional unbounded queues.

Expose the supported handoff through pipe/provider settings. Do not enable the
existing broad `Transactions`/`ExactlyOnce` capability names until their precise
scope is specified and met. Prefer a narrowly described guarantee to an
unqualified "exactly once" flag.

**Done when:** every commit/publication/acknowledgement interruption preserves
pending obligations and once-only committed participating effects across
reconstruction. Different-store configurations work without pretending to use
distributed transactions. Arbitrary global atomic visibility remains outside
this contract.

### W6. Preserve complete upstream transactions when requested

**Addresses:** issue 6; CG-18.
**Complexity: high. Disruption: high, therefore opt-in.**

Carry source transaction identity and boundaries through decoding, envelopes,
middleware, plugin marshaling and durable replay. Begin with PostgreSQL's
committed multi-row transactions. Keep existing per-change processing unchanged
when grouping is not selected.

The legacy SQLite source still has transaction-isolation and pre-commit
publication gaps; worker-generation fencing alone does not fix them. The user
explicitly directed a **new native SQLite component**, preserving the legacy
API's behavior rather than repairing its transaction semantics in place.

Process the group under a compatible transaction and delay publication until its
final derived state is available. Multiple changes to the same row, deletes,
relations, aggregates and scheduled changes must respect that publication
boundary; simply concatenating current per-row notifications is insufficient.

Use explicit size/time limits. If a group exceeds one envelope's negotiated
limit, reject it explicitly without acknowledging successful atomic processing.
The user selected this bounded rejection policy rather than disk-backed staging:
retain the last completed source position and allow processing to resume after
the configured limit is increased. Never silently split an atomic group into
independently visible updates. Durable recovery must distinguish complete groups
from interrupted assembly.

**Implemented so far (not W6 completion):** Core has opt-in complete-transaction
result hooks, including shared-journal staging. They keep the original before
image and final after image per row/group, remove cancelling row changes, retain
aggregate baseline/default semantics, and leave ordinary batch methods unchanged.
Real RocksDB cases cover repeated rows, relations, aggregates, scheduled work,
rollback, cancelled staging, reconstruction and shared-journal commits.

The native library now has a distinct `drasi.source-transaction` schema and a
bounded `SourceTransactionBuilder`. A completed group occupies one record;
incomplete, oversized, timed-out, mixed-source or malformed groups cannot become
valid input. Its commit method must be called only after an upstream commit is known,
and the source must select on its exposed deadline while waiting for more data.
Both the existing JSON storage codec and native binary codec preserve the whole
frame, source identity, commit position and transaction context. This is a codec
check, not qualification of a separately built plugin.

Native queries opt in through `QueryExecutionSettings::source_transactions`,
with explicit positive `max_changes`, `max_bytes` (binary transaction payload)
and `max_duration_ms` limits. Use `descriptor_with_execution` when declaring a
factory-created query. Ordinary graph-change ports and non-atomic storage or
publication are rejected. Input progress, final results and retained output
commit together. Processing deadlines fence interrupted ownership for cleanup.
Increasing admission limits preserves committed state; changing between grouped
and ordinary semantics requires explicit recovery/reset. These settings also
round-trip in the native factory's existing execution configuration.

The user-directed native Rust PostgreSQL source now reuses the connection,
protocol, decoder and common value/key services. Its strict path publishes
complete groups, uses actual persistent owner progress for feedback, preserves
heartbeats through bounded-output backpressure and retains worker ownership
through interrupted cleanup. It rejects malformed/incomplete input, unavailable
cursors, missing keys and unsafe automatic slot retention. Stable, binding-checked
commit positions are separate from replay transport sequences; increasing numeric
limits does not reset progress. The shared transaction codec's optional replay
context is validated against the complete group and leaves ordinary framing
unchanged.

Real PostgreSQL/RocksDB cases cover repeated changes, transient rows, key changes,
deletion, rejected-group reconstruction, duplicates, three source restarts,
cancelled stop, heartbeat backpressure beyond the server timeout, direct/factory
graph pipes, and abrupt process exits before/after query commit. A query restart
retains final state; native input replay does not publish intermediate rows.
These are scoped qualification results, not completion of W6.

The latest run passes 104 PostgreSQL units, six substantive native integrations
and their explicitly driven process-exit helper, one real legacy multi-row
regression, 31 shared PostgreSQL conversion units, 1,136 library units and five
source-transaction integrations. Two existing library crash helpers remain
ignored as standalone cases and are driven by their parents. Source/common and
legacy dynamic-plugin all-target lint pass; selected library lint passes with
the existing `question_mark` allowance. The parity runner and its self-check now
include the PostgreSQL transaction profile and protect the exact new cases.
The regenerated architecture artifacts contain 1,091 types, 2,647 stored
relationships, 315 namespaces and 16 extraction diagnostics.

W4 now supplies opt-in exported-snapshot handover for the native source, preserving
replay-only operation. Its additional reference cases and limits are described
above.

The new native SQLite Rust component and factory now supply private bounded
transaction lanes, reserved cancellation rollback, revoked scoped handles and
post-commit publication. The user selected aborting the whole scope on **any**
statement failure, even when the callback catches it. SQLite's authorizer rejects
raw transaction boundaries and query-based write/savepoint escapes; actual parsed
savepoints preserve capture count/byte accounting. Idle scopes and SQLite VM work
have execution deadlines. Worker ownership and explicit client-resource ownership
survive interrupted cleanup and a blocked submitted commit.

Its ordinary graph-change mode has no replay journal or committed-progress
service. Complete-transaction output is separate. Optional durable mode commits
the replay record with the SQLite business changes and retires it only from the
actual persistent query's progress. It verifies source/stream/consumer/table and
schema bindings, journal continuity and cursor identity; missing history and
weakened configurations fail closed. The shared transaction builder now supports
inert pre-commit staging without exposing an uncommitted envelope.

Current-thread SQLite and real RocksDB cases cover isolated scopes, bounded
admission, cancellation, caught errors, failed foreign-key commits, quoted
savepoints, key changes, deadlines, full journals, source reconstruction and
direct/factory graph pipes. Three real exit-84 child processes cover before
SQLite commit, after SQLite commit and after query commit. The legacy unit,
REST/bootstrap/query integrations and ABI export are retained separately.
These are scoped reference guarantees, not completion of W6.

Native PostgreSQL additionally supports an optional bounded PEM trust anchor
without modifying system trust or disabling hostname verification. Real
certificate-backed cases cover replay-only and coordinated-snapshot connections,
persistent restart, server-observed encryption/session retirement, untrusted and
expired certificates, wrong hostnames, malformed CA settings and preservation of
query progress on rejection. `Prefer` never downgrades after a TLS handshake fails.
The updated gate passes 104 PostgreSQL library cases and 16 native integration
cases, including all 29 required PostgreSQL parity contracts. Strict
dynamic-plugin lint, workspace formatting, architecture checks and the
parity-runner self-check pass.

**Still pending:** separately built native plugin negotiation/exposure and qualification,
native SQLite REST/Server exposure, and the full
provider/crash/transition matrix. Replay-only operation requires an explicit
initial LSN and pre-existing slot; coordinated mode manages its own. Legacy PostgreSQL still emits
individual changes; neither its fast contract nor its ABI is upgraded. Native
SQLite replay-only mode rejects pre-existing populated databases; opt-in
coordinated mode loads them before live admission. The legacy SQLite plugin is
not upgraded.

**Done when:** observers never see part of a committed source transaction at the
declared boundary, including after restart. Unsupported sources/providers fail
configuration validation. This does not promise one transaction spanning
independent databases or all external sinks.

### W7. Preserve errors and expose shared services through plugins

**Addresses:** issue 7; CG-05, CG-06, CG-07 and recovery portions of CG-11.
**Complexity: high overall. Disruption: high at the ABI boundary.**

First fix storage-list errors becoming empty success and dropped lifecycle
updates leaving stale state. Use explicit results and authoritative
generation-aware status/readiness with a catch-up path, not indefinitely blocking
callbacks. Carry typed causes through public/Server error classification without
adding incompatible `DrasiError` variants.

Version and forward durability descriptions, bootstrap settings/context,
admission receipts, completion and the scoped recovery services needed by W2-W5.
Reuse the native transaction capability's bounded request and revocation
patterns. Keep native and legacy ABI versions independently managed.

**Read-only progress prerequisite:** native PostgreSQL, MySQL and SQLite now
consume `SourceProgressReader`, with independent, cancellation-safe
`SourceProgressUpdates` subscriptions. Existing local construction retains the
actual `Arc<QuerySourceProgress>` and pointer-identity recovery checks.
An external `SourceProgressProvider` can supply bounded reads and wakeups, but
cannot supply local transaction-owner proof, even with identical names.
Read, subscription and revocation failures propagate instead of becoming empty
progress; no forwarding worker or writable progress mirror is introduced.
Source/bootstrap pairing still requires the same actual reader handle. Fast
MySQL/SQLite configurations do not construct a reader or subscription.
Five new Core cases cover complete state, independent observation, cancellation,
closure and typed read failures. Three SQLite/RocksDB cases prove actual graph
ownership rejection, initial loading/reconstruction through an external reader,
and revoked reads retaining uncommitted input. Native database replay and
snapshot regressions pass; the external-reader cases are in-process Rust tests.
The independent native **recovery-v1** service now carries that read-only view
without changing ABI 1.0, wire 2 or admission service-v1. Factory negotiation,
typed host-resource binding, bounded snapshot serialization, independent
subscriptions and revocation are wired end to end. The SDK verifies the actual
supplied reader and retention consumer; the host retains the real owner for
unchanged graph pointer-identity assertions. Activation cannot silently change
the negotiated declaration. Fast sources have no progress handles/subscriptions
or new per-envelope work.

A separately built, test-only plugin wraps the existing SQLite replay source.
Its host-owned RocksDB query exercises uncommitted whole-transaction replay,
committed journal retirement, reconstruction, cancellation before database open
and stale callback rejection. It deliberately does not alter the volatile standard
counter or advertise native bootstrap support. General bootstrap/other recovery
services, production database plugin registration and Server integration remain
unfinished; this read-only service does not complete W7.

Publish an old/new host/plugin compatibility matrix. Old libraries can retain
supported behavior, but cannot claim new guarantees. Where an old contract
cannot represent a required error or setting, require an upgrade or reject the
unsupported operation explicitly. Compatibility is not permission to preserve a
false success result.

**Compatibility decision:** newer hosts must retain ABI 0.16 legacy plugins for
their existing fast-mode behavior. Recovery settings requiring newer capabilities
must fail explicitly for those plugins; upgrading the host must not require every
legacy plugin to be rebuilt. Earlier incompatible ABIs remain rejected.

**Initialization-panic decision:** contain the panic rather than aborting the
host, but never initialize or activate that same damaged plugin instance again.
Cleanup must be confirmed before constructing its replacement. Failed or
unfinished cleanup retains ownership and blocks replacement; a plugin without a
reconstruction factory requires explicit replacement by its owner.

**Done when:** real separately built libraries preserve the same outcomes as
in-process components. Errors, late callbacks, revocation, size limits and
incompatible capabilities fail explicitly. Fast plugins make no new recovery
service calls or carry additional recovery-only fields in their data frames.
New grouping/recovery representations are negotiated only where used.

### W8. Own work through shutdown and replacement

**Addresses:** issue 8; lifecycle review, CG-21 and CG-25.
**Complexity: medium-high. Disruption: moderate-high.**

Consolidate worker cancellation, readiness and awaited joining around existing
graph resource/instance ownership. Register listeners, polling, pruning and
heartbeat workers; retain cleanup ownership if stop is cancelled or times out.
A timeout reports incomplete cleanup, not successful release.

For recoverable paths, stop new admission, quiesce or preserve in-flight work,
finish/fence submitted storage operations, and transfer ownership only after the
old owner cannot make conflicting progress. Preserve replayable obligations and
reject late acknowledgements from retired generations. Fast paths may discard
volatile pending work under their documented cancellation policy, but still
must close resources correctly.

Fix controller fairness and unbounded deferred-command admission as lifecycle
prerequisites: control/stop must remain serviceable under ready-source traffic,
full data queues and slow construction. Preserve one root graph and independent
query scheduling; a larger queue or a rate limiter is not the fairness fix.

**Done when:** interrupted shutdown/replacement has a discoverable owner and
recoverable pending state where promised. Repeated cycles release workers,
sockets and resource handles. Uncooperative trusted plugin code produces an
explicit failure/escalation outcome, not a fabricated safe-stop guarantee.

**MySQL remains incomplete:** its Drasi-owned replication worker now uses
serialized lifecycle calls, ownership-before-spawn, retained graceful joins,
shutdown-aware subscriber/bootstrap/binlog/reconnect waits and observable worker
errors/panics. All 48 package units and eight existing real-MySQL integrations
pass, including current-thread cancellation, a blocked TCP handshake, bootstrap,
restart and TLS cases; dynamic-plugin lint passes. The dependency audit found
that `mysql_async` 0.37.1 can detach cleanup during connection/registration
failure and hides errors in `BinlogStream::close`. These paths prevent a complete
driver-ownership guarantee and need a driver-level fix or a different native
connector implementation. The user selected a **separate native MySQL component
with owned replication I/O**, not a vendored driver patch. Reuse protocol/data
types without the driver's connection/task ownership, preserve the legacy ABI,
and keep no-recovery operation independent of persistent progress services.
No legacy transaction or checkpoint semantics have been upgraded by the
worker-lifecycle changes.

The user approved **both non-purging policies**: ordinary bounded retention with
explicit gap failures and no guaranteed-replay declaration, plus opt-in retention
until processing with automatic binlog expiry disabled. Neither mode changes
retention settings or purges files. Operators must retain the last checkpoint's
transaction file (used as a content-verification anchor) and all subsequent
history. A file position or GTID alone is not evidence of retained history.
Strict validation includes the older `expire_logs_days` setting as well as
seconds-based expiry and the newer automatic-purge switch.

The native Rust source now implements those policies with owned packet/auth/TLS,
catalog and binlog I/O; bounded strict decoding; stable typed/binary/composite
keys; complete transaction output; actual query-owned replay checkpoints; and
direct/factory graph construction. Its fast changes mode has no transaction
assembly, replay hashing or persistent progress. Native tests cover real graph
pipes, atomic query visibility, source/query reconstruction, actual process exits
before/after commit, oversized rejection/retry, missing/reused history, schema
changes, live binlog rotation, cancelled startup and authenticated TLS. Real
column-type coverage also caught and repaired native signed MEDIUMINT decoding,
YEAR signedness, BIT bytes and fixed BINARY padding. Legacy semantics remain
unchanged. Pre-bootstrap qualification passed all 74 source library cases (including
the parent-invoked crash helper), eight existing real-MySQL integrations and
23 shared MySQL cases; strict dynamic-plugin lint, workspace formatting,
architecture checks and the parity-runner self-check also pass. The helper's
standalone no-op is not additional crash evidence; the parent verifies actual
process exits before and after durable query commit.

The user also approved an **opt-in native MySQL bootstrap provider**, separate
from the legacy provider. `MySqlSource::coordinated` pairs it with the actual
query progress owner; the native factory accepts the same paired resource.
Durable intent precedes external locking. A deadline-limited global read lock
establishes the consistent view and binlog boundary, then releases before bounded
binary-cursor scanning. Selected-table metadata locks exclude DDL through the
read transaction. Initial/live identity and value mapping match. Completion,
handover state and watermark commit in the query's transaction; snapshot-prefix
content verification rejects reused binlog history before live admission.

Partial snapshots require explicit reset, while completed snapshots resume
without rescan. The provider owns its sessions through server-observed retirement,
retains cleanup through cancellation/timeouts, blocks replacement and preserves
cleanup failures. MySQL has an important limitation: cancelling a pending table
flush does not necessarily release subsequent same-table access until an older
reader finishes. Drasi does not kill unrelated readers, and hard process loss
still depends on server session cleanup, not an independent lock lease.
The configured deadline must not be advertised as an unconditional pause bound.
The completed bootstrap gate passes 86 source library cases, eight legacy
real-MySQL integrations and 23 shared-type cases. All 49 required MySQL parity
cases appear in actual passing output; strict dynamic-plugin lint, workspace
formatting, architecture checks and the parity-runner self-check pass. Coverage
includes direct/factory initialization, least-privileged access, initial/live
type parity, concurrent writes and DDL exclusion, partial/completed process exits,
rotation/reused history, expanded-row rejection and cancelled/timed-out cleanup
against a paused server. Native fixtures await Docker's published IPv4 mapping
as well as MySQL's internal readiness.
Native ABI/Server wiring remains outstanding; these Rust reference capabilities
do not complete all MySQL work or the full reliability plan.

### W9. Restore accepted deployments and compatible processing state

**Addresses:** issue 9; CG-04, CG-15, CG-20 and CG-26.
**Complexity: high across Core/Server. Disruption: high, kept opt-in.**

Wire Server into Core's existing optional managed configuration store and
reconciliation. Persist desired definitions and idempotent request receipts
before effects. Expose accepted, initializing, ready, failed and cleanup-pending
outcomes, including receipt lookup after a lost response.

Persist selected pipe/provider policies, stable resource identities and
reconstruction recipes. Restore only state whose component, schema,
configuration and generation identities are compatible. Missing secrets,
plugins, storage or versions leave a visible unresolved deployment; do not fall
back to memory or an empty graph.

Treat changes to durability, storage, subscriber membership and grouping as
explicit transitions. Preserve old obligations until drained or deliberately
retired. Refuse unsafe downgrade/removal while required work is pending unless
the user explicitly authorizes the loss. Fast-to-durable activation establishes a
new acceptance boundary; it cannot protect work lost before that boundary.

**Done when:** requests and desired definitions survive crashes during creation,
replacement and response delivery; retrying a request does not duplicate
ownership. Both fast and recoverable recipes round-trip through supported Server
configuration. W6 adds its grouping settings before the whole plan is complete.

**Implementation progress:** Host managed resources and Server native
configuration now reconstruct `sourceProgress` resources for an explicitly named
consumer; Host also supports the Server's `queryCatalog` recipe. Sources and
consumers share the same resource ID, with actual-owner validation retained.
Recipes cannot supply live checkpoints, readiness or durability, and separate
instances receive distinct owners. Server roundtrip and actual two-query
dataflow coverage includes these resources.

Server now uses Core's managed instance and encrypted redb configuration provider.
The selected policy initializes from YAML once; stored desired state controls later
restarts, including after the seed becomes semantically obsolete. Canonical empty
state can remain revision zero without being mistaken for an uninitialized store.
Provider acquisition is shared by actual database path and key, while ownership
and receipts stay instance-scoped. No store/key failure falls back to memory.

Revisioned desired, receipt, status and reconciliation endpoints distinguish
acceptance from readiness. Failed deployments remain inspectable; imperative
configuration changes cannot bypass managed acceptance. YAML persistence retains
store settings without exporting accepted credential-bearing definitions.
Current Router evidence covers genuine native execution/reconstruction, JSON/YAML,
idempotency, response loss, cancellation after durable commit, unavailable commit
and read confirmation, conflicts, instance isolation and secret-safe responses.
Actual Server preparation/startup covers obsolete seeds, stable IDs, missing/wrong
keys and unavailable factories. Runtime-loaded missing factories require an
instance restart; live factory refresh is not claimed.

Host and Server QoS recipes now select bounded admission or output replay using
the same host configuration type. Actual instance/graph scope is injected by the
owner. The strict Core constructor commits initial mode metadata atomically and
rejects omitted/changed persisted modes before retirement or trimming; exact
metadata preservation and actual receipts are checked across reopen. A plain
empty journal can establish a new tracked boundary, but accepted older messages
cannot retroactively acquire receipts. Omitted volatile settings still do not
create recovery machinery.

Host and Server now expose `sharedStorage` and `sharedQos`. Explicit graph-owned
resource dependencies supply actual parent handles, validate cycles/roles and
ownership, propagate replacement, and release users before providers. Failed or
cancelled cleanup retains prerequisite owners. Snapshots and inspection retain
the dependency graph; old independent constructors cannot silently ignore it.
No hidden host cache or independently opened matching group stands in for the
actual transaction owner.

Host evidence covers group identity, instance isolation and persistent journal
identity. Server evidence covers typed configuration, deferred imperative
construction, actual query output and accepted-state reconstruction. A managed
pipeline reopens with no new source input and finishes the previously unhandled
shared-journal output. This is in-process reconstruction, not a Server
process-crash or external-destination exactly-once claim.

The chosen transition policy is rejection before acceptance until drained or
explicitly retired, not an accepted pending handover containing old/new definitions.
Standard resolvers now protect declared persistent domains, including unavailable
ones, while unrelated domains and lifecycle-only settings remain editable.
Definite refusal is HTTP 409 without a receipt/revision, not ambiguous HTTP 503.

The permitting path currently covers stopped shared-storage domains. A graph
lease prevents restart/configuration mutation; actual storage gates exclude new
obligations through producer-output, live-journal cursor and future-work checks,
configuration commit and authoritative readback. Confirmed rejection resumes
the original owners; acceptance fences and reconstructs exact old generations.
Uncertain confirmation holds the leases for later authoritative reconciliation.
Abandoned proof cannot authorize replacement. Resource reconstruction preserves
the recipe rather than using imperative rebinding's recipe-discard semantics.
Plain fast paths acquire none of these leases or journal checks.

Server cases exercise pending refusal while running and stopped, drained
same-path replacement, fresh-path redirection, accepted removal/reopen, lost
responses, rejected writes and unavailable confirmation on either side of commit.
Concurrent direct restart and storage transactions remain blocked until the
decision is resolved. Core cases cover scheduled-work detection, in-flight
exclusion, unresolved fencing and initialization evidence. This does not migrate
stored query state. Standalone/unsupported provider transitions, explicit
loss-authorized retirement and supported whole-path Server qualification remain;
W9 is not complete.

## 4. Qualification and the fast-path gate

Reuse the existing foundation, runtime-parity, native-library and cross-crate
integration harnesses. Extend their requirement ledger rather than introducing
another testing framework or counting coverage as a recovery guarantee.

| Required scenario | Required evidence |
|---|---|
| **Disabled reliability** | No extra per-envelope recovery allocations, storage calls, serialization, acknowledgement/duplicate lookups or service RPCs. No additional recovery queues/workers/resources. Preserve the direct native memory provider, including no-default-feature builds. |
| **Fast-path performance** | Compare identical workloads before/after with fixed configuration, warmup and repeated runs; record throughput, tail latency, allocations, CPU and memory. Establish run-to-run noise first. Any repeatable regression attributable to disabled reliability blocks acceptance rather than being excused by an arbitrary percentage allowance. |
| **Crash boundaries** | Interrupt before/after admission commit, state/output commit, each branch acceptance, external effect and progress commit. Reconstruct in a new process and account for every accepted identity and required consumer obligation. |
| **Storage faults and uncertainty** | Inject read/write/commit failures, corruption, disk/capacity exhaustion and interrupted cleanup. No fabricated empty history, false success or blind retry of an uncertain commit. Process-kill evidence alone must not be called power-loss qualification. |
| **Concurrency and transitions** | Exercise slow/disconnected consumers, membership changes, partial batches, simultaneous sends, full queues, replacement and late old-generation callbacks on both Tokio runtime flavors. |
| **Snapshot and transaction grouping** | Concurrent source changes, multi-source delays, unavailable positions, repeated row changes, incomplete groups and crashes around publication produce the declared exact state and visible results. |
| **Real plugin boundaries** | Run the same contract through ordinary in-process components, native factories and separately built native/legacy libraries. Include unsupported/old versions and scoped-service cancellation. |
| **External delivery** | A non-idempotent endpoint demonstrates permitted repeats; an idempotent endpoint and transactional database destination demonstrate once-only effects within their declared deduplication scope. Neither may silently skip failed operations. |
| **Managed restart** | Recover accepted configuration receipts, resource recipes and pending delivery with missing/unavailable dependencies. Fast data paths remain fast when only configuration persistence is enabled. |

The reference end-to-end matrix should include native HTTP/gRPC admission,
PostgreSQL replay/bootstrap, a stateful transformer, a real continuous query,
retained and QoS branches, and handled consumers. Include an optional lossy
monitor beside a required durable consumer and custom-schema components so the
services are not accidentally limited to the legacy source/query/reaction shape.

Guarantee-critical bookkeeping is present only when required by that guarantee.
Baseline error reporting and ownership are always required. Detailed tracing and
profiling remain separately optional; they must not be introduced as an
unavoidable per-envelope cost.

## 5. Delivery boundaries and existing code to reuse

| Area | Existing foundation and implementation location |
|---|---|
| Contracts and graph resolution | [`pipe.rs`](../src/computation/v1/pipe.rs), [`graph/specification.rs`](../src/computation/v1/graph/specification.rs), graph preflight/reconciliation and inspection |
| Atomic participation and cancellation | [`core::computation`](../../core/src/computation/mod.rs), [`transaction.rs`](../../core/src/computation/transaction.rs), existing provider index bundles and `ComputationTransaction` |
| Recoverable processing/handoff | [`TransactionTransformer`](../src/computation/v1/transaction_transformer.rs), retained/QoS pipes, producer/source progress and provider outbox implementations |
| Source and consumer services | [`SourceBase`](../src/sources/base.rs), [`ReactionBase`](../src/reactions/common/base.rs), [`consumer_recovery.rs`](../src/computation/v1/consumer_recovery.rs), query bootstrap and WAL replay |
| Plugin integration | `components/computation-plugin-{abi,sdk}`, `host-sdk`, `plugin-sdk`, native network components and selected legacy plugins |
| Configuration/restart | [`managed configuration`](managed-configuration.md); future integration in `drasi-server/src/computation.rs` and its computation API handlers |
| Qualification | `lib/tests`, `lib-integration-tests`, Host SDK separate-library tests; Server tests and `test-infra/e2e-test-framework` for matching embedded/Server workloads |

Preserve the dependency direction: plugins depend on shared runtime contracts;
`drasi-lib` must not depend on plugin implementations or concrete storage
backends. End-to-end tests needing component crates belong in
`lib-integration-tests`.

All new guarantees must be explicit additions with compatible configuration and
stored-format rules. Do not silently migrate old checkpoints or upgrade their
meaning. Specify upgrade, rollback and unsupported-version behavior before
writing new persistent formats.

**The plan is complete only when all nine packages have their stated outcomes,
the supported plugin/provider matrix is explicit, and fast mode passes the same
cost gate throughout.** Unsupported external capabilities remain honest
limitations, not features claimed through configuration. General distributed
transactions, automatic cross-database snapshots and unconditional exactly-once
external effects are not promised.
