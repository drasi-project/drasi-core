# WorkGraph: sanitized drasi-core issue history

**As of September 12, 2026.**

> This is a sanitized copy of the recovered report. It preserves technical and public fix provenance while excluding private development-session metadata.

## Main finding

The early WorkGraph effort had an explicit inventory of **20 filed Core issues: #774, #775, and #792-#809**. The histories also record earlier PR-only fixes, unfiled defects, an explicitly parked recovery patch, and a deletion-resilience investigation that was stopped.

The crucial distinction is **fixed in the prototype versus merged into Core's `main`**:

- Seven query-engine tickets have implemented fixes on the retained WorkGraph branch: **#792, #793, #794, #806, #807, #808, #809**.
- Two additional tickets, **#774 and #775**, have prototype ABI/restart repairs.
- The remaining eleven original tickets are **six unfixed bugs and five deferred enhancements**. Some are avoided by WorkGraph's graph/query design, not repaired in Core.
- **All twenty original GitHub issues remain open.**
- **#792 has open draft PR #810.** The later upstream recovery stack includes **open PR #826 for the #775/#821 hydration problem**. Neither is merged.

The current retained Core branch is [`workgraph-generic-recovery`](https://github.com/drasi-project/drasi-core/tree/workgraph-generic-recovery), at [`fb5d2bda`](https://github.com/drasi-project/drasi-core/commit/fb5d2bda2cbce4284fecbce40cae2a2d74116249). Current `main` is `e759606fa065bee0ef9e60017e263f0f85dd0e44`. The branch comparison contains the seven original query-fix commits; the branch is 96 commits ahead and 12 behind `main`. Its latest commit is documentation, not another engine fix.

## Scope and source hierarchy

This sanitized publication omits private app-session identifiers, transcript locations, local paths, and hierarchy metadata. The technical findings were reconciled against current public issues, pull requests, branches, and commits. The original report checksum is preserved below so custodians can verify the private source without publishing it.

## 1. Query-engine bugs with prototype fixes

**Every issue in this table is still open upstream.** Except for #792's draft PR, these fixes remain carried by the broader WorkGraph branch rather than a separate landed upstream change.

| Issue | What went wrong | Implemented repair and disposition |
|---|---|---|
| [#792](https://github.com/drasi-project/drasi-core/issues/792) | Changing grouping values could leave an old result row, reverse an update, or make the destination/source groups overwrite each other. Whole-node grouping used reference identity in one place and full properties in another. | [`d4ad7421`](https://github.com/drasi-project/drasi-core/commit/d4ad74218754f597f50e9ef45562eb535069aed5): consistent grouping equivalence, correct migration/default flags, and distinct projected group identities. **Open draft [PR #810](https://github.com/drasi-project/drasi-core/pull/810)** targets `main`, using patch-identical isolated commit `62685e467e6f276f2ba4449ca8bfcb5b4aa63c2e`. |
| [#793](https://github.com/drasi-project/drasi-core/issues/793) | Nested optional aggregates missed the first matching child, so a parent never entered its next workflow state. | [`c2b86f9c`](https://github.com/drasi-project/drasi-core/commit/c2b86f9c537bff047c8335e86a078f39264343e2): chained optional-path propagation. **Prototype fix.** |
| [#794](https://github.com/drasi-project/drasi-core/issues/794) | Adding later optional paths could leave an earlier child count stuck at zero. | Same [`c2b86f9c`](https://github.com/drasi-project/drasi-core/commit/c2b86f9c537bff047c8335e86a078f39264343e2) repair. **Prototype fix.** This did not, by itself, fix every later multi-path or nested-aggregation failure. |
| [#806](https://github.com/drasi-project/drasi-core/issues/806) | A second aggregation discarded the insertion/default flags of a real zero-valued group. Final projection suppressed the parent row as a no-op, then later emitted an update for a row never added. | [`2f129f22`](https://github.com/drasi-project/drasi-core/commit/2f129f229f487611bebbba3c9270aca315642401): durable outer-group contributor counts preserve actual group lifecycle. Simply copying flags was insufficient because deleting a non-last zero-valued contributor could wrongly remove the outer group. **Prototype fix.** |
| [#807](https://github.com/drasi-project/drasi-core/issues/807) | Cyclic MATCH paths could reuse an inconsistent relationship binding and count it more than once. | [`67311899`](https://github.com/drasi-project/drasi-core/commit/6731189964f90c9fbd08af39fa0e76883bab71b3): validate solved path endpoints. **Prototype fix.** |
| [#808](https://github.com/drasi-project/drasi-core/issues/808) | OPTIONAL MATCH could lose or duplicate rows when partial paths could not complete; results depended on relationship arrival order and correlation across optional clauses. | [`fcce3f11`](https://github.com/drasi-project/drasi-core/commit/fcce3f11a38df8cde7355473d05fb9f90bb05baf), followed by [`c6615b45`](https://github.com/drasi-project/drasi-core/commit/c6615b450b0be85694f2e77460cf892617eb946e): parser-preserved clause identity, clause-aware fallback/correlation, and preservation of later independent candidates. **Prototype fix, including a necessary follow-up correction.** |
| [#809](https://github.com/drasi-project/drasi-core/issues/809) | Replacing an OPTIONAL relationship endpoint removed the unmatched parent's row rather than restoring its zero/default row. An equivalent explicit Delete+Insert behaved differently. | [`7691d769`](https://github.com/drasi-project/drasi-core/commit/7691d769b58393940d3e58fe0756300589d39f04), with additional coverage in `c6615b45`: reconcile replacement-update and default-row lifecycles. **Prototype fix.** |

### Important evolution of this work

On August 27, the first aggregate investigation identified two bounded fixes for #792 and initially recommended deferring the broader zero-aggregate case. The very next parent/child lifecycle matrix required that case: parents disappeared instead of moving from Fork to JoinAll. It was therefore **not permanently deferred as a whole**. Subsequent work produced #793/#794, then the distinct #806-#809 repairs. However, the general accumulator empty-identity/contributor and Sum-encoding redesign **remained explicitly deferred**; authorizing bounded outer-group cardinality did not authorize a general `is_at_identity` implementation for every aggregate.

Another correction, [`7be2e1bd`](https://github.com/drasi-project/drasi-core/commit/7be2e1bd895196c1e4fbf99a23dbbcbdb4abc8e8), addressed **persisted aggregate replay identity**: persisted group signatures needed the same grouping semantics as live evaluation. Closing a child before replaying Result/Evaluation left an extinct optional-null group behind, stranding parent Assign; `PartDefault`/`PartCurrent` used full Element values instead of grouping identity. The fix advanced result-index semantic state to version 6. **PR #810 does not include this later residual fix.** The same commit made a positionless WorkGraph subscription fail closed when complete WAL history was unavailable. This is additional recovery work, not a reason to count every replay observation as another instance of #792.

Source: sanitized development history, August 27-28.

## 2. Query-engine bugs not fixed

**All six are open.** The histories show workarounds and selective authorization of other fixes, not a separate explicit user rejection of each ticket. "Unfixed" and "explicitly deferred" should not be treated as interchangeable evidence.

| Issue | Problem | What was done instead |
|---|---|---|
| [#795](https://github.com/drasi-project/drasi-core/issues/795) | A WHERE condition attached to OPTIONAL MATCH removes the unmatched parent row. | Used explicit relationships with OPTIONAL MATCH. A required-MATCH rewrite would not preserve unmatched parents and is not an equivalent general fix. **No Core fix found.** |
| [#796](https://github.com/drasi-project/drasi-core/issues/796) | An outer variable in a node property map parses/builds but fails during processing, e.g. `UnknownIdentifier("team")`. | Used explicit relationship topology instead of the correlated property-map form. **No Core fix found.** |
| [#797](https://github.com/drasi-project/drasi-core/issues/797) | An optional graph variable introduced after aggregation becomes unknown during evaluation. | Moved graph matches before aggregation or split queries; cardinality still had to be preserved by the application design. **No Core fix found.** |
| [#798](https://github.com/drasi-project/drasi-core/issues/798) | `IN` inside a list-comprehension filter can return the wrong membership result. | Replaced membership filtering with `coll.indexOf(existing, x) >= 0`. **No Core fix found.** |
| [#799](https://github.com/drasi-project/drasi-core/issues/799) | Numerically equal integer and floating-point values compare unequal. | Replaced count-like SUM expressions with conditional COUNT to avoid Float/Integer mismatches. Direct `0.0 = 0` and the Dogfood equal-zero case establish the bug; one filed aggregate example incorrectly compares 1 with 0 and is not supporting evidence. **No Core numeric-equality fix found.** |
| [#805](https://github.com/drasi-project/drasi-core/issues/805) | Reusing an element ID for a node and a relationship silently corrupts query state. | WorkGraph uses collision-free identities. **Caller-side prevention is not Core-side rejection or a repaired identity model.** |

## 3. Deferred query-language capabilities

These were filed as **enhancements**, not all as correctness bugs.

| Issue | Missing capability | Historical disposition |
|---|---|---|
| [#800](https://github.com/drasi-project/drasi-core/issues/800) | Deterministic ordering of aggregate collections. | **Deferred; open.** Do not assume collection order supplies a stable "latest" selection. |
| [#801](https://github.com/drasi-project/drasi-core/issues/801) | String comparison in `min`/`max` aggregates. | Feasibility was investigated. A general solution affected numeric accumulator/sort-key assumptions and persistent backends; the broad redesign was not authorized. **Deferred; open.** |
| [#802](https://github.com/drasi-project/drasi-core/issues/802) | Correlated EXISTS graph subqueries. | **Deferred; open.** |
| [#803](https://github.com/drasi-project/drasi-core/issues/803) | GQL MATCH stages after NEXT. | **Deferred; open.** |
| [#804](https://github.com/drasi-project/drasi-core/issues/804) | Expanding list values into matched rows. | **Deferred; open.** |

The August 28 parent inventory explicitly said the then-current 16-query design avoided the unfixed limitations. That is a statement about the prototype's selected queries, **not a resolution of the underlying issues**.

### Additional unfiled query/function limitations

These were found in the child histories but are not accounted for by the numbered enhancement tickets above. Where the cause was not established, they remain reported limitations rather than new confirmed generic engine bugs.

| Observation | Disposition |
|---|---|
| `coll.distinct` on an unmatched optional collection raised `InvalidType { expected: "List" }`. | Internal collection use was removed from the scalar query design. **No Core fix or separate issue found; it was not established whether the underlying problem was function semantics or invalid/null input construction.** Historical investigation evidence. |
| A disconnected task-by-worker MATCH returned no rows although each side worked independently. Other staged forms failed with `UnknownIdentifier("worker")`. | The design changed to task-only queries carrying `agentId`. **No general Cartesian-MATCH fix found.** Historical investigation evidence. |
| Cypher UNION could not combine the frozen artifact-detail streams. | Canonical artifact/detail nodes supplied the existing query instead. **Application graph-model workaround; no UNION implementation or issue found.** Historical investigation evidence. |
| `MATCH -> WHERE -> OPTIONAL MATCH` was rejected by the supported grammar. | Clauses and trusted predicates were reorganized. **Query rewrite, not parser support added.** Historical investigation evidence. |
| The desired generic hash and newline-producing Cypher primitives were unavailable. | Hash-dependent run/event IDs became readable deterministic IDs; the Source supplied body digests. The proposed generic SHA-256 and parser escape additions were explicitly removed from the simplified scope. **No retained generic capability fix.** |
| Simple root-only OPTIONAL cases were initially suspected to fail. | The initial Core correctness gate did not reproduce that hypothesis. Its final exact CASE/SUM follow-up was interrupted before a result, so the earlier result cannot disprove the later mixed-number problem. |

## 4. Restart, delivery, and plugin-boundary defects

### The two filed reliability tickets

| Issue | Failure | Repair and current status |
|---|---|---|
| [#774](https://github.com/drasi-project/drasi-core/issues/774) | `on_subscriptions_complete` did not cross the dynamic Source ABI. A Source could prune WAL after early subscribers caught up, before later persistent subscribers restored older positions. A 60-second fallback was not a correctness guarantee. | The actual forwarding implementation was introduced in [`1bd669a3`](https://github.com/drasi-project/drasi-core/commit/1bd669a389924cc264ef071f5f97624b5618ef08): Source vtable slot, typed/boxed adapters, and SourceProxy forwarding. **Present on the retained prototype branch; not on current main; issue open.** |
| [#775](https://github.com/drasi-project/drasi-core/issues/775) | After restart, in-memory query output restarted at sequence 0 despite durable rows/outbox/checkpoints. A new result could reuse sequence 1, overwrite an old outbox entry, regress the sequence, and be skipped by reactions. | Prototype repairs include [`54d02054`](https://github.com/drasi-project/drasi-core/commit/54d02054698bf34bcf3b4afa32ce6bc0480f8055) and [`e4f2af57`](https://github.com/drasi-project/drasi-core/commit/e4f2af57f8c8b1b8ac8ec24b20909e05fe7cdae3), including hydration and legacy outbox recovery. **Issue open. Later upstream [PR #826](https://github.com/drasi-project/drasi-core/pull/826) is open and unmerged**, in the recovery stack described below. |

**Two corrections to the old summaries:** the August 21 parent reply swapped the titles of #774 and #775. Also, the August 28 inventory named `f54e829c` only as an unverified candidate for #774. That commit changes SourceBase dispatch ordering, not subscription-complete forwarding. The correct forwarding provenance is `1bd669a3`.

The original issue-creation events place both filings at **August 21, 19:48:11 UTC**; a secondary local index dated August 22 was not the actual filing time.

### Earlier PR-only and unfiled repairs

| Defect | Evidence and implemented change | Disposition |
|---|---|---|
| Durable reaction sequence baseline lost across restart. | [PR #735](https://github.com/drasi-project/drasi-core/pull/735), merged at `48f2e6ce3fc85dc58dfcf68b3a6545847d8ff695`. | **Merged into the old `agentofreality-symmetrical-telegram` prototype branch, not main.** A predecessor to the later, more complete #775 recovery problem, not proof that all restart paths were solved. |
| Deep left-nested AND/OR expressions exhausted a Tokio worker's stack. | [PR #736](https://github.com/drasi-project/drasi-core/pull/736), flattening logical-expression evaluation; prototype merge `9bbaa29245d9eb7d31c96c38037b38bb5289aef1`. | **Historical prototype-only merge.** |
| Durable host state appeared non-durable to dynamic plugins. | `StateStoreVtable` did not expose durability and the proxy inherited `false`. Original repair `efa01868`; retained lineage includes [`eb777fa7`](https://github.com/drasi-project/drasi-core/commit/eb777fa7966d04d93a4dabe6b22fbab6f75c1f3e) and ABI versioning `487df491`. | **Prototype repair. [PR #742](https://github.com/drasi-project/drasi-core/pull/742) remains an open draft on the old stack.** |
| Retained multi-source queries diverged from freshly constructed queries. | The live Issue/Comment/Project-state workflow exposed stale initializer/launcher/router results; historical fix `601e2a34b5bce2ec37e26f26818df35f2d27c830`. | **Repair on the old prototype lineage.** Preserve this observation separately from the later minimized #792-#809 issues. |
| Startup Source changes could be lost before downstream dispatch was ready. | [`645af080`](https://github.com/drasi-project/drasi-core/commit/645af0801f268af61d44f58480e44c3e3a3544be), "Preserve WorkGraph startup events." | **Retained prototype repair.** |
| Dispatch order changed across the plugin boundary. | [`f54e829c`](https://github.com/drasi-project/drasi-core/commit/f54e829c81d6dde581001d8c20338afa6818ddd6), "Preserve dispatch order across plugin ABI." | **Retained prototype repair.** This is not #774's lifecycle callback fix. |
| Query output did not reconcile a changed result-row signature correctly. | [`01dbaf20`](https://github.com/drasi-project/drasi-core/commit/01dbaf200e2705d86604b819d443df9b93574635), changing query manager/output-state reconciliation. | **Retained prototype repair.** |
| Dynamic reaction delivery could be acknowledged before the plugin callback finished, and callback failures did not propagate reliably. | [`cc8d3f71`](https://github.com/drasi-project/drasi-core/commit/cc8d3f712b69fc730b91ce04edf4556e8d2b7899): await callbacks across FFI, propagate errors, fail pending deliveries on shutdown, stop forwarding after rejection; acknowledgement ABI 0.16. Follow-up `eb224595` preserves error chains. | **Retained prototype repair.** Does not by itself establish every generic checkpoint-after-side-effect guarantee. |
| HTTP Reaction treated GraphQL HTTP 2xx error responses as success. | [PR #747](https://github.com/drasi-project/drasi-core/pull/747), declarative response JSON validation. Retained lineage contains [`16f67030`](https://github.com/drasi-project/drasi-core/commit/16f670304b5956a41f316a0ba670d8e0aa7eab9b) and exact JSON-number classification follow-ups. | **Implementation retained on the prototype branch, but PR #747 was closed unmerged when that earlier stack was abandoned.** |

The HTTP work had two important qualifications: the numeric guard initially misclassified nonzero `1e-400` as zero and was corrected; an old plugin could accept but ignore the unknown nested response-guard configuration, so configuration acceptance alone did not establish that the guard was active. **No generic unknown-field rejection repair was demonstrated.** Partial serial GraphQL mutation success also retained an explicitly accepted ambiguous retry/duplicate-side-effect risk.

The exact-number correction briefly enabled `serde_json/arbitrary_precision` globally, affecting unrelated dependency consumers. That intermediate approach was removed: the final HTTP implementation used local raw-value traversal and exact significand classification instead of changing numeric behavior across the dependency graph.

### What the older durable-output hardening actually covered

The original custom lineage advanced through `dc10c20083723414572472537a1b5368627784b1` and `042559d99f77dd6e071d35bf3dfe3c2453b65301`, beyond the initial #735 sequence-baseline patch:

| Additional defect | Historical outcome |
|---|---|
| Restoring the sequence alone left in-memory outbox/live state empty despite persistent payloads. A failed live-row write followed by a later successful watermark could certify an incomplete snapshot. | **Repaired in the custom lineage:** hydrate and reconstruct only from contiguous certified history; repair missing snapshot mutations rather than merely advancing the number. |
| Lower sequence writes could regress persisted watermarks. An initial Garnet get-then-set repair still raced and had full-`u64` representation concerns. | **Repaired in `042559d9`:** monotonic Memory/RocksDB paths and atomic Garnet length/lexicographic comparison. |
| Capacity-based outbox trimming could evict the exact history needed to repair a failed snapshot update. | **Repaired in `042559d9`:** retain required repair history until snapshot/outbox continuity is restored; real gaps still fail closed. |
| A fresh replay checkpoint could advance to the response's latest sequence after only part of the batch was enqueued successfully, or advance unconditionally across a gap. | **Repaired in `042559d9`:** record only the last successful sequence and honor the configured gap policy. |
| Source/index progress could still commit before output persistence. | **Not established as atomically repaired by these commits.** Monotonic watermarks and repairable history are not a transaction spanning input progress, engine state, outbox, and snapshot. See #822/PR #830. |

There were separate branch lineages, not one uninterrupted deployment: the later main-based #737/#740/#742 stack did not inherit every original custom-lineage fix. This explains why some startup/restart failures appeared again.

### Additional Source integration and persistence repairs

| Defect or blocker | Implemented change / remaining limitation |
|---|---|
| An initial projector journal retained every full-body transition and required work proportional to history; partial batches also needed recoverable checkpoint/WAL ordering. | [`a826c56e`](https://github.com/drasi-project/drasi-core/commit/a826c56edd369da55f6fc76069e9f95ed8305123) introduced a bounded opaque post-transition checkpoint, staged token, materialized task map, and pending-origin/offset recovery. **Prototype repair; the narrower append-success/offset-save crash window remained.** |
| Authenticated VNext Assignment/Dispatch evidence never reached the old native allocator, whose issue/comment/agent identity vocabulary did not match canonical task/assignment/executor identities. | [`9fa2113c`](https://github.com/drasi-project/drasi-core/commit/9fa2113c844715fb253b814bd309dbfab6da69c4) implemented the native canonical allocator. An alias-only adapter was rejected. **Prototype repair, with an explicitly breaking fresh-state schema.** |
| Delta-only allocator directives could retain an older lease after another artifact, definition conflict, or reparenting invalidated its graph Assignment. | The same `9fa2113c` change reconciled against a **complete accepted-state snapshot**, not just the latest input's directives. **Prototype repair.** |
| Dispatch/Close could destroy historical Lease facts required by subsequent lifecycle queries. Closed-task validation could discard accepted historical assignments. | `9fa2113c` separated active slot ownership from immutable historical Lease/LEASE_FOR/detail evidence and preserved closed-task history. **Prototype repair.** |
| A custom Source descriptor could not preserve unresolved SecretReference configuration; persisted properties contained `[REDACTED]` instead of reloadable references. | [`957961c0`](https://github.com/drasi-project/drasi-core/commit/957961c0e4a6137d3d89cbdc0fb38055024e17ea) exposed `.with_raw_config(original_unresolved_json)`. **Prototype repair.** |
| On a fresh empty WAL, `head == 0` opened the startup pruning fence too soon. | `7be2e1bd` made the fence begin closed until subscription completion. **Prototype repair, distinct from merely adding #774's ABI callback.** |
| Positionless replay from `oldest - 1` could silently accept incomplete retained history. | `7be2e1bd` accepts replay from zero only when the WAL starts at sequence 1; pruned nonempty history fails closed with reset instructions. **Prototype repair.** |
| One subscriber's index lacked a relation despite sharing the same later checkpoint as an intact subscriber. | Dogfood Issue #87; exact offline mapping/query replay worked, and the distinguishing WAL evidence had already been pruned. **Cause unresolved: no proof separating loss from late/out-of-order transport.** The proposed contiguous-sequence gap detection was not shown to repair this exact case. |

An excess-slot retirement projection discrepancy was also encountered repeatedly. It matched pre-existing behavior, and the user explicitly excluded changing it. **It remains a deferred contract discrepancy, not a newly demonstrated allocator capacity leak.**

### A broader recovery patch was deliberately parked

The August 14 **reaction replay gap** was real and different from merely restarting a sequence counter:

- Bootstrap populated process-local retained results without necessarily creating sequenced, durable, replayable query output.
- A reaction could report Running but have no consumable startup candidate.
- Manager-owned enqueue checkpoints could acknowledge before side effects.
- Reaction-owned replay needed a separate forwarding watermark to avoid replay/buffered-live duplicates.

The child implemented and pushed [`8033ca12`](https://github.com/drasi-project/drasi-core/commit/8033ca122ad83cbca7427e7f3b3a83b50775832b) on `agentofreality-fix-reaction-replay-gap`.

**The parent explicitly parked it:** "I will not merge, deploy, or include it in the simplification work unless the new architecture independently requires it." It must not be counted as an adopted fix simply because the child reported completion. Source: GitHub Workflow, August 14.

Follow-up `e26ba153` added narrowly bounded reconciliation for components with durable per-candidate provenance. `c3b5f54078065372f9b75b4560e3abfb8db354c8` documented the remaining legacy checkpoint-equals-clock/no-payload problem; **that documentation was not another fix**. Automatic exactly-once reconstruction was not possible when the needed historical execution evidence was absent. An offline audited recovery manifest was proposed, not implemented. Clearing a reaction checkpoint or replaying every current row into a generic HTTP/gRPC Reaction was explicitly insufficient/unsafe.

An in-process Redb reopen also encountered a short-lived shutdown handle owner. The historical scenario switched to fresh processes; no separate fix or filed issue for that specific observation was recovered. Treat it as a bounded unresolved observation, not proof that every Redb shutdown path remains broken.

## 5. Explicitly stopped deletion-resilience work

The August 28-29 audits found that ordinary unrelated issues were generally isolated, but unrestricted mutation/deletion of WorkGraph-managed issues and artifacts was not safe. The parent then **stopped the deletion-resilience work without a complete repair** and resumed live ingress/admission work.

| Identified gap | What the history establishes |
|---|---|
| Canonical task Issue admission checked transport/object shape without a corresponding creator/editor/sender provenance gate. | **Diagnosis only at the audit checkpoint.** Whether manual canonical tasks should be admitted is a policy decision; the proposed actor/provenance restriction was not implemented in that audit. |
| Lifecycle deletion checked the original comment author rather than the deleting sender. | **Diagnosed and not repaired by that audit.** Do not confuse author trust with authorization of a later destructive action. |
| Removing a marker or changing Issue Type could bypass the old ownership path and leave stale workflow evidence. | **Confirmed and deferred at the audit checkpoint.** Later Source rewrites and lifecycle-revision handling repaired portions; do not assume the entire old mutation matrix was closed. |
| Ordinary-to-canonical and canonical-to-ordinary reclassification could leave evidence in both graph namespaces. | **Diagnosed, no atomic dual-namespace cleanup implemented at that checkpoint.** Later removal of the legacy architecture changes applicability; it is not evidence that the original patch was made. |
| A trusted artifact rewritten into another role by an untrusted editor could leave its previously accepted evidence active. | **Confirmed in the audit.** Later `c3813ee1` adds prior-artifact retraction for untrusted cross-role edits. |
| Late edits/removals under new delivery IDs could resurrect deleted state or remove a newer incarnation. | Delivery-ID dedupe was not entity-version/tombstone ordering. **Deferred initially; later task-generation and comment-revision fencing addressed specific paths.** |
| DeleteTask could retain task-bound lifecycle documents, allowing the old chain to return on same-source restoration. | **Diagnosed lifecycle-generation gap.** A genuinely recreated GitHub Issue with a new node ID did not inherit the old chain. |
| Deleting Dispatch evidence could make an old Assignment eligible again and launch duplicate work. Deleting Result/Evaluation could strand or regress lifecycle state. | **Confirmed; no complete cancellation, repair, and retirement closure was found in the original audit.** Do not equate graph retraction with cancellation of an already launched Agent Task. |
| Sparse `sub_issue_removed` payloads containing only a numeric child ID did not retract canonical parent linkage. | **Later repaired** by [`e1b56a29`](https://github.com/drasi-project/drasi-core/commit/e1b56a2902e3e30904a0f67dff2383968a84ed6d): durable database-ID lookup and authoritative hierarchy convergence. |
| Deleting a completed child after its parent was assigned could leave the parent authorized for downstream execution with incomplete child evidence. | A **WorkGraph projector/query/reaction correctness gap**, not a generic Core parser bug. The audit reported it; its complete repair was not established before that work was stopped. |
| WAL append and pending-offset persistence were separate; the classic delivery marker was also written after state/WAL. | **Residual at-least-once duplication window.** A crash after append but before recording progress can append the same raw changes again. Stable IDs are not a general exactly-once side-effect guarantee. |
| `last_dispatched` advanced while constructing a batch, before a successful downstream send. | **Unfixed at the audit checkpoint.** A send failure could wedge the running process until resubscription/restart despite retained WAL entries. |
| WAL deletion and Source state clearing were separate deprovision operations. | **Unfixed at the audit checkpoint.** Partial failure could restore an old projector/allocator checkpoint into a fresh WAL; merely reversing operation order was not a full solution. |
| Already-dispatched external work and append-only journals lacked cancellation/retirement. | **Residual orphaning/GC work.** Local deletion released native allocator capacity correctly; this was not a slot leak. |

Later source commits include `16cc7d3c` (equal-revision task fencing), `95fa3647`/`2a6fa78e` (authorization-generation ordering), and `c3813ee1` (lifecycle comment revisions/tombstones). These are evidence of **partial subsequent remediation**, not grounds to silently mark the entire deferred audit complete.

WorkGraph-specific Source, allocator, and protocol ownership was subsequently removed from Core in [`b1458cb1`](https://github.com/drasi-project/drasi-core/commit/b1458cb1c25cd39457361a1be010f2fba7c6d5ff). Those concerns now belong to the WorkGraph implementation rather than an assumption that generic `drasi-core` still owns them.

Source: sanitized development history through August 30.

## 6. Later Source corrections and misleading leads

The recovery sessions exposed additional **WorkGraph-specific Source defects that lived in the Core repository at the time**:

| Finding | Outcome |
|---|---|
| Generated typed tasks carrying the admission label could be misclassified as ordinary Root Issues. | Fixed with durable hierarchy handling in `e1b56a29`; generated tasks are excluded from Root Issue admission. |
| Stale/equal-revision Issue events could reopen or reauthorize state incorrectly. | Task-revision and authorization-generation fences were added through `16cc7d3c`, `95fa3647`, and `2a6fa78e`. |
| The lease validation consumer inferred the attempt instead of receiving the authoritative active-lease attempt. | The contract was made explicit in `22c8a52b`, followed by `509a3bf3` and `726c2e27`. |
| Lifecycle producers and the Source disagreed on exact Assignment/Evaluation marker names, and Error artifacts needed ingestion. | [`c3813ee1`](https://github.com/drasi-project/drasi-core/commit/c3813ee178d6dffe4d2dff5afc5f7b6d1cd1dead) changes `WorkGraphTaskAssign/v1` to `WorkGraphTaskAssignment/v1`, `WorkGraphTaskEvaluate/v1` to `WorkGraphTaskEvaluation/v1`, adds `WorkGraphTaskError/v1`, and introduces lifecycle-comment revision handling. |

Three historical assertions should **not** become additional unresolved Core tickets:

1. **"Core cannot ingest Route artifacts."** This was reported during the August 31 loopback handoff, but `WorkGraphTaskRoute/v1` was already in the preceding Source protocol. The later change corrected other marker names and lifecycle fencing. The initial Route-only diagnosis was stale/incomplete.
2. **"The production projector is missing from Core."** The first August 30 audit proposed adding it. The reconciled report corrected that: the concrete projector intentionally lived in Dogfooding's dynamic wrapper. It was an ownership misunderstanding, not a missing generic Core implementation.
3. **A proposed GQL inline-predicate parser issue.** The claimed failure could not be reproduced, so it was explicitly not filed.

The alleged failure to maintain a derived `(run, task-definition)` identity also needs correction: one major reproduction used **colliding node/relationship fixture IDs**. Correcting those IDs disproved that diagnosis. The real subsequent solver and nested-group failures were separately minimized as #806-#809.

Related older tickets [#384](https://github.com/drasi-project/drasi-core/issues/384), [#411](https://github.com/drasi-project/drasi-core/issues/411), and [#680](https://github.com/drasi-project/drasi-core/issues/680) were considered but not established as duplicates of the WorkGraph discoveries. Later [#811](https://github.com/drasi-project/drasi-core/issues/811)/[#812](https://github.com/drasi-project/drasi-core/issues/812) describe lower-level optional before-image and nested grouping-switch failures, but no filing/provenance for them was found in the owned WorkGraph aggregate-session history. **[#791](https://github.com/drasi-project/drasi-core/issues/791) appeared only incidentally in duplicate-search results** there; its later [#814](https://github.com/drasi-project/drasi-core/issues/814) performance/ordering diagnosis should not be relabeled as a WorkGraph discovery without further provenance.

## 7. Later upstream durability follow-up

The early repairs did not finish the complete process-crash durability contract. The later tracking structure is:

- [#743](https://github.com/drasi-project/drasi-core/issues/743): recovery conformance umbrella.
- [#818](https://github.com/drasi-project/drasi-core/issues/818): remaining implementation work.
- Original [PR #745](https://github.com/drasi-project/drasi-core/pull/745) remains an open draft. The later recovery stack starts with open [PR #825](https://github.com/drasi-project/drasi-core/pull/825).

**Every issue and corresponding PR below is still open; none of these PRs is merged.**

| Follow-up | Purpose | Current implementation PR |
|---|---|---|
| [#821](https://github.com/drasi-project/drasi-core/issues/821) | Hydrate durable QueryOutputState before processing; directly continues #775. | [#826](https://github.com/drasi-project/drasi-core/pull/826) |
| [#822](https://github.com/drasi-project/drasi-core/issues/822) | Commit query output, index changes, and source checkpoints atomically. | [#830](https://github.com/drasi-project/drasi-core/pull/830) |
| [#824](https://github.com/drasi-project/drasi-core/issues/824) | Reject durable reactions attached to volatile queries. | [#831](https://github.com/drasi-project/drasi-core/pull/831) |
| [#819](https://github.com/drasi-project/drasi-core/issues/819) | Prevent fresh trigger reactions from replaying retained history. | [#832](https://github.com/drasi-project/drasi-core/pull/832) |
| [#820](https://github.com/drasi-project/drasi-core/issues/820) | Advance reaction checkpoints only after side effects complete. | [#834](https://github.com/drasi-project/drasi-core/pull/834) |
| [#823](https://github.com/drasi-project/drasi-core/issues/823) | Clear prior query output on reconfigure/delete-and-recreate. | [#835](https://github.com/drasi-project/drasi-core/pull/835) |

These are later follow-ups to the reliability problem family, **not six additional independent discoveries to add blindly to the original twenty**.

The separate September composable-pipeline stack also attempted atomic output and recovery work. PRs **#869, #870, #871, and #872 are now closed unmerged**. Older app labels showing these as drafts are not evidence that their code landed.

## 8. Earlier feature PRs are not issue-resolution evidence

| PR | Current state | Interpretation |
|---|---|---|
| [#737](https://github.com/drasi-project/drasi-core/pull/737), payload-only GitHub Source | Open draft | Historical feature branch, not a landed upstream solution. |
| [#739](https://github.com/drasi-project/drasi-core/pull/739), simplified event workflow | Open draft | Mixed prototype work; does not close the twenty filed issues. |
| [#740](https://github.com/drasi-project/drasi-core/pull/740), bootstrapper | Open draft | The bootstrapper was later removed from the current WorkGraph design. |
| [#742](https://github.com/drasi-project/drasi-core/pull/742), durability bridge | Open draft | Genuine repair, still on the historical stack. |
| [#744](https://github.com/drasi-project/drasi-core/pull/744), canonical comment parser | Open draft | Historical protocol work, later superseded in part. |
| [#746](https://github.com/drasi-project/drasi-core/pull/746), task/queue contracts | Closed unmerged | Abandoned PR stack is not a merged fix. |
| [#747](https://github.com/drasi-project/drasi-core/pull/747), HTTP response validation | Closed unmerged | Useful implementation was retained in the later prototype lineage. |
| [#765](https://github.com/drasi-project/drasi-core/pull/765), dispatcher Reaction | Closed unmerged | Superseded by Source-owned allocation. |

## 9. Earlier WorkGraph component defects in the Core repository

These are included for completeness because they were discovered in `drasi-core` sessions. They are **not additional generic query-engine tickets**. Several components and entire protocol generations were later replaced, so "fixed on that historical branch" does not imply the old component remains part of today's product.

### Middleware, launcher, router, and Project refresh

| Component / problem | Recorded outcome |
|---|---|
| Source-wide middleware could drop unrelated graph elements or fail to reconcile prior derived output after an update. | `924d01c33158220898a3f2bb6f86686949ad4205` added preserving/reconciling middleware behavior. `a50fff73` corrected documentation that wrongly mapped Update as Insert. **Historical implementation, later integrated by the coordinator.** |
| Malformed JSON edits left a previously derived WorkGraph event alive because parsing failed before reconciliation. | **Configuration repair:** parse in place with `on_error: skip`, then type-gate the reconciliation stage so malformed input produces an empty derived set and retracts old output. This was not a new JSON parser implementation. |
| Project refresh mishandled secondary-rate-limit 403s and assumed the field was literally named Status. | `e63c14e9` and `266f187f` added bounded rate-limit handling and configurable field names while preserving permanent handling for other 403s. **Historical fix.** |
| Project refresh used the wrong deterministic status-node identity and insufficient Project/field/destination authority checks; retry could bypass current-row constraints. | `41c2297d`, `403be11d`, `6602e16e`, and `650533bd` corrected identity and authority checks. **Historical fix.** |
| Refresh scanned the full Redb state on every add; cancellation could strand the pruning permit. | `23a06f98` throttled scans and `4a1d058e` made the permit cancellation-safe. **Historical fix.** |
| Launcher/refresh examples used config shapes that the actual descriptor/server could not load. | Flat descriptor-compatible configuration and secret-reference examples were corrected. Final refresh equivalents included `e56e5493`/`4a1ee908`; launcher equivalents included `a373d466`/`5160cf53`. **Documentation/config integration repair, not an engine grammar bug.** |
| A duplicate launcher request after allowlist narrowing could overwrite a durable Started/task identity with Failed. | `926919b41ba2509904d911e7b6841c3143dd11d8` loaded existing durable state before mutable allowlists and removed the blind overwrite. **Historical fix.** |
| Launcher wire fields, prompt key, authoritative subject/Project/content/profile binding, and token-owner verification were inconsistent or incomplete. | Corrections culminated in `a373d4662042ec1853145f2533be741c49429c78`; the integrated subtree-identical equivalent was `5160cf53aba477322688758371349e2ac65f793d`. **Parallel integration lineages, not proof that one SHA is the other's ancestor.** |
| Router reservation relied on instance-local get/set locking, which was not an atomic cross-instance primitive. A fallback `create_if_absent` also falsely promised atomicity. | `e7190b1d` and `37d0fe35` introduced real provider CAS or explicit Unsupported, plus reservation fencing; `90403176` fenced individual effects. **Original custom-lineage repair.** The later Source-owned allocator deliberately retained a single-active-instance contract, not equivalent HA guarantees. |
| Extending a vtable while keeping its ABI version could read beyond an older physically smaller table. | Router CAS and later durability-port work corrected ABI versions and rejected incompatible hosts/plugins. **A nullable trailing slot was not a valid compatibility strategy.** |
| Router used a nonexistent mutation, omitted real bearer auth, or could write against stale/closed/incorrectly correlated items. | The real `updateProjectV2ItemFieldValue` path, fresh per-effect checks, and auth were corrected through `e7190b1d`, `90403176`, and `ff0dce92`. **Historical fixes.** |
| Router could silently acknowledge an unresolved foreign-policy reservation, accept invalid output ownership/responsibilities, or advance a non-strict checkpoint past unresolved work. | Persisted-policy resumption/error handling and unresolved-sequence barriers were added through `e7190b1d`/`ff0dce92`. **Historical fixes.** |
| Explicit empty router allowlists were treated as omitted and regained permissive defaults; persisted old-policy decisions could bypass current transition restrictions. | `26d48cde88d7a49684846d46d8f3df407930804b` distinguished omission from explicit emptiness and validated persisted as well as new decisions. **Historical custom-lineage fixes.** |
| The manager acknowledged router enqueue rather than completed side effects. | The same `26d48cde` introduced `ManagerCheckpointOwnership`/`Reaction::checkpoint_ownership()` on **August 13**, before the later HTTP recurrence. **A reusable capability was implemented, but that did not establish adoption by every asynchronous Reaction.** |
| Dynamic ReactionProxy lost that checkpoint-ownership capability because its vtable omitted the hook. | `44342b390c501032a24519c087cb4df4cd1e539f` forwarded `checkpoint_ownership_fn` across the Reaction ABI. **Distinct from #774's Source subscription-complete callback and from the later callback-completion acknowledgement protocol.** |
| The plugin loader could skip ABI compatibility checks when metadata was absent. | The same `44342b39` made metadata required. **Historical custom-lineage repair.** |
| A reservation key could resume a different candidate after partial effects; fresh reaction-owned replay could race its initial config-hash seed. | `25bda3bb2ef6eebef60dd6babdf69138466babae` introduced immutable-context fingerprints and seeded configuration before enqueue. **Partial historical repair:** the next review still required rejecting legacy empty fingerprints with prior progress and including `outcome` in the fingerprint. No completed child follow-up for those two requirements was recovered. |
| A permanently invalid router candidate could poison the entire strict stream. | **Completion unresolved in the retrieved history.** Repeated requests demanded durable terminal rejection while allowing later candidates and preserving transient retries; the child did not provide a final completion SHA for that exact blocker. Do not treat unrelated fencing/auth repairs as its resolution. |
| PR #739 review exposed unsafe partial-side-effect recovery, ambiguous recreation, assignment conflicts, and canonical byte differences. | Final `d860611bd75c3790fcc75d3f03ae04c76b52fec0` reported the bounded corrections: durable intent first, no blind recreation after ambiguous sends, trusted-assignment coalescing, ownership serialization, exact JSON/summary bytes, and adaptive HTTP policy. **Pushed to an open draft, then frozen.** |

Primary evidence came from the sanitized middleware, refresh, launcher, router, and simplified-Core development histories.

The additional router provenance comes from sanitized transcript-derived evidence

### Payload Source, comment-backed leases, and the early allocator

| Problem | Recorded outcome |
|---|---|
| Collapsed Assignment/Result parsing disagreed with producer bytes, final newline, summary, and validation-profile contract. | The accepted #744-era pin was `59bc6dd31f7106737337ff25faa0586ff92569f4`, not the earlier held candidates. **Historical parser fix; the comment protocol was later superseded by task Issues.** |
| Typing/untyping an Issue did not reclassify its existing comments. | **Explicitly not fixed in the payload-only Source.** The webhook lacks those comment IDs/bodies; an API read or bootstrap-seeded cache was needed for safe reconciliation. Historical investigation evidence. |
| The supported request-info task could not select `issue-info-requester`. | `4c0d037d` corrected the whitelist in the #746 lineage. **Historical fix.** |
| Labels/state had inconsistent or absent shapes for the queries. | `5ec967a2` and `a575af4f` normalized state, reason, ordered label arrays, and derived convenience fields through the shared mapper. **Source contract improvement, not an engine fix.** |
| Task Source/bootstrap handling diverged across type changes, filtered repositories, pagination, parent-repository shapes, reverse-order sub-Issue links, malformed-body repair, and database-ID tombstones. | Multiple corrections were carried by #746's `409790b0` checkpoint and consumed by the HTTP-validation stack. **Historical convergence fixes; later architecture changes replaced portions.** |
| Comment-backed Lease events could forge/cross-bind endings, corrupt shared identities, overcount active leases, resurrect ended leases on replay, or fail to restore state after an end edit/delete. | `43f28285` used role-specific author/editor trust, exact anchors, and deterministic current-artifact folding. **Historical fixes, later replaced by Source-owned allocation.** |
| Worker configuration missed force/create/delete push cases; remote fetches blocked a shared webhook critical section. | `43f28285` corrected push matching and moved network work out of the shared gate. **Historical fix.** |
| Worker-ledger persistence preceded WAL, and bootstrap's temporary ledger did not align with live lifecycle state. | `43f28285` reordered graph/ledger/delivery recording and reconciled historical comments. **Historical repair, not a claim of cross-provider transactionality.** |
| The minimal dispatcher only suppressed pending work within one scope and lacked some row-identity/worker/repository guards. | `198ecb48` added global pending suppression, scoped exact cleanup, consistency checks, and token redaction; `3064be2d` corrected sorting. **Implemented, then the dispatcher architecture was superseded; PR #765 closed unmerged.** |
| A trusted Assignment posted after task closure could allocate new work. | Locally repaired in the August 22 Source-owned allocation checkpoint. |
| Assignment edit/retract/recreate reused lease identity, allowing a stale Result to release a new allocation. | Locally repaired with monotonic assignment attempts and incompatible old-state rejection in the same checkpoint. |
| TaskCancelled deauthorization depended on artifact map ordering. | Locally repaired so deauthorization no longer depended on whether artifact IDs sorted before or after Result. |
| The webhook signing secret could equal the lease-validation bearer secret. | Locally repaired by rejecting equality of resolved values without exposing them. |

The last four repairs and the early runtime/ABI work were recorded by their owning child as **local-only commit `2dc5d87833960cffc7a2335215e3d93fdd09089c`**, with **no push or PR at the August 22 checkpoint**, then frozen. **It was subsequently published:** the August 28 audit's live remote listing records that exact SHA at `refs/heads/agentofreality-source-owned-workgraph-allocator`; an August 27 remote-tracking observation already contains it. The exact push time/actor was not established. Thus "local-only" describes the original handoff, not its final historical disposition. Publication still was not an upstream merge. The later retained branch contains the consolidated allocation implementation at `1bd669a3`.

### The abandoned general-purpose GitHub Source implementation

Before the payload-only Source, the original `components/sources/github` design had a richer API hydrator, snapshots, reconciliation, and durable inventory. Its defects were repaired/reworked in the original #737 development sequence, then the implementation was deliberately replaced. These are **historical discoveries, not automatically still-open defects in the current WorkGraph Source**:

| Problem family | Historical disposition |
|---|---|
| Invalid GraphQL selections, unsupported App attribution, and incorrect Project-item locator shapes. | Corrected query fragments/shapes; unavailable attribution was removed rather than fabricated. Same-user credential attribution remained a documented limit. |
| Secret exposure from properties, followed by an overcorrection that stripped required configuration and broke snapshot/recreate; a masked literal authorization header. | Config round-trip and actual bearer-header behavior were corrected. Silently dropping required secret references was explicitly rejected as a solution. |
| Fatal hydrator failure left the Source reporting Running/accepting work; stop/restart could detach or revive old tasks/listeners; timeouts were missing. | Source supervision, bounded requests, and stop-before-restart were hardened. Parts of that machinery were then removed with the hydrator architecture. |
| Dedupe check/append/mark races and indefinitely growing markers. | Admission durability and serialization were corrected, with bounded retention rather than a claim of eternal dedupe. |
| Missing/nested pagination and partial GraphQL errors could appear to be a complete inventory and delete real graph objects; reconciliation could race deletion or stale scope. | Corrected in the original inventory/hydration passes. That inventory architecture was later removed. |
| Bootstrap discarded durable adjacency, making early deletion ineffective; incorrect ownership fallback deleted the wrong Project Item; archived replay could remove restored items. | Bootstrap/authoritative-state coordination was corrected in the historical design, then superseded. |
| Concurrent query bootstrap could deliver an older snapshot after a newer one or deadlock on a bounded waiting queue. | Per-query ready gating and an atomic prepared reconciliation record were introduced, then removed/reworked with the simplified Source. |
| A missing/deleted/unsupported object at the FIFO head could retry forever. | Bounded absence/tombstone/terminal handling was added; later payload-only deletes used signed locators without hydration, eliminating that specific failure class. |

The historical correction spine includes `c2a8396c` (contracts), `7f00f325` (failed hydration), `393c3cbc` (bootstrap coordination), and `0d1cfe6b` (handoff), followed by two-WAL simplification and payload-only conversion. DCO rewriting changed commit IDs without changing the final reviewed tree. This is another reason identical-looking work can appear under several session/commit identities.

#### Additional intermediate Source/shared-runtime findings

The sanitized development history contains the following more specific review findings in that same superseded implementation. Several broad shared-runtime/WAL modifications were later excluded from the minimal Source scope. **Their individual final repair commits were not established; do not convert them into either current open defects or completed upstream fixes without a separate survival trace.**

| Finding | Historical evidence and disposition |
|---|---|
| Dedupe compaction rescanned and individually read all markers after every admission, producing quadratic work over a backlog. | Historical investigation evidence. **Intermediate performance defect; different from the refresh Reaction's pruning hot path.** |
| Newly stopping Error queries could hit `DrasiQuery::stop`'s debug assertion, which did not accept Error. | sanitized transcript location. **Coupled regression in the attempted global lifecycle cleanup; subsequent correction was requested.** |
| A new query's bootstrap overwrote the shared reconciliation baseline without applying that snapshot to older queries. | sanitized transcript location. **Existing queries could remain stale while future reconciliation saw no difference.** The historical bootstrap redesign addressed the problem family before that architecture was removed. |
| Source and bootstrap selected different state stores because their precedence rules differed. | sanitized transcript location. **A shared effective provider was required; not a query-language defect.** |
| Broadcast subscriptions bypassed bootstrapping-query exclusion, allowing delta plus full-snapshot duplication. | sanitized transcript location. **Broadcast rejection or proper per-receiver targeting was required in the obsolete bootstrap architecture.** |
| A pending committed delta could be cleared after a successful dispatch with no ready subscribers. | sanitized transcript location. **The requested repair retained pending work until an eligible subscriber existed.** A success return without a receiver was not delivery proof. |
| Redb WAL filename escaping could map `a:b` and `a_3a_b` to the same file. | sanitized transcript location. **Introduced shared-WAL identity collision in the intermediate change.** Broad WAL changes were later excluded; no current surviving collision is asserted here. |
| The proposed switch to `srcid-<hex>.redb` could open a new empty WAL instead of the existing legacy file. | sanitized transcript location. **Follow-on migration/access regression**, not a safe repair merely because the new names were collision-free. |
| Concurrent subscriptions could associate a resume watermark with the wrong dispatcher index by reading `len() - 1` after releasing the registration lock. | sanitized transcript location. **Atomic creation/registration of the actual subscriber identity was required.** |
| A Source set `source_position` but left wrapper `sequence` to a reset process-local counter, allowing query dedupe to discard new events after restart. | sanitized transcript location. **Source-event sequence defect, distinct from #775's query-output sequence restoration.** A WAL-derived/restored wrapper sequence was proposed; no separate final fix association was established here. |
| Permanent GraphQL authentication, permission, or invalid-document failures became endless transient hydration retries. | sanitized transcript location. **Misclassification could leave the Source Running and accepting work until full.** The later payload-only design eliminated this hydrator path. |
| Project union queries used the same response field `state` for incompatible Issue/PR enum types. | sanitized transcript location. **Additional GraphQL schema error**, distinct from the invalid Actor/Project-owner selections above. |
| An object leaving scope was ignored without retracting its prior in-scope graph. | sanitized transcript location. **Stale projection in the historical inventory model.** Later scope/cache removal changed the contract. |
| Cascade deletion iterated a randomized map, while sorted assertions concealed nondeterministic output order. | sanitized transcript location. **Historical ordering defect; the user-directed direct-delete design later removed cascade machinery.** |

These findings came from the August 14-15 intermediate Source review. Intermediate "all blockers fixed" messages were followed by further findings, so they are not used as unconditional final-resolution evidence.

### Other deliberately bounded contracts

- **Single active allocator:** no distributed CAS/leader-election or active-active allocation guarantee in the simplified design.
- **Point-in-time lease validation:** validation does not atomically cover a subsequent GitHub write.
- **Result actor attribution:** the old Result schema did not independently identify a worker; trusted reporter plus exact lease possession was the accepted boundary.
- **Bootstrap/activation:** no invented restoration of preexisting open Assignments; fresh-state activation and full Source-state/WAL reset requirements were explicit.
- **Missed webhook reconciliation and atomic GitHub snapshots:** payload-only ingress did not implement polling/catch-up, and successful pagination was not a transactional upstream snapshot.
- **Historical publication dependency:** automatic standalone plugin installation was gated on loader-compatible published Core/SDK dependencies, referenced as [#574](https://github.com/drasi-project/drasi-core/issues/574); pinned local plugin builds were used instead. This is a historical packaging constraint, not a newly asserted current engine defect.

## 10. Provenance and limits

The unsanitized source report has SHA-256 `683a7406e1edd6778f892bd3e849d2440ac8da3fa023aac79c6ac93b37eebbda`. Private session navigation, transcript coordinates, and local paths are intentionally omitted. `provenance.json` records the sanitization boundary and checksums for this publication.

The report does not claim to recover permanently deleted history, and "no fix found" means no implementation or landing evidence was found in the searched history and relevant public refs. It does not mean every current query/backend combination was re-executed.

Product workflow changes, agent-profile/MCP problems, tunnel failures, and app session failures are not counted as generic Core defects. WorkGraph-specific Source issues remain documented because they were encountered in `drasi-core`, even though ownership later moved.
