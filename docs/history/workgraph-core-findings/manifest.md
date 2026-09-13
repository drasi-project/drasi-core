# Publication manifest

## Durable records

- Preservation umbrella:
  [#889](https://github.com/drasi-project/drasi-core/issues/889)
- Preservation draft:
  [#890](https://github.com/drasi-project/drasi-core/pull/890)
- Machine-readable ledger:
  [`ledger.json`](ledger.json)
- Human-readable ledger:
  [`ledger.md`](ledger.md)
- Sanitized full source report:
  [`source-report-sanitized.md`](source-report-sanitized.md)
- Per-record original finding evidence:
  [`evidence.md`](evidence.md)
- Public fix inventory:
  [`fix-inventory.json`](fix-inventory.json)
- Complete artifact checksums:
  [`checksums.sha256`](checksums.sha256)

The ledger covers all 132 source finding rows and all seven narrative anchors.
There are no unassigned source IDs.

## Publication counts

| Record class | Count | Result |
|---|---:|---|
| Source finding rows | 132 | All mapped |
| Source narrative anchors | 7 | All mapped |
| New generic Core issues | 11 | #891-#900, #902 |
| New preservation umbrella issues | 1 | #889 |
| Original Core issues enriched | 20 | #774, #775, #792-#809 |
| Additional existing issues enriched | 5 | #574, #680, #721, #722, #827 |
| Existing PRs enriched | 19 | #735-#747 subset, #765, #810, #825-#835 stack |
| New preservation PRs | 1 | #890, draft |
| Local-only commit patches preserved | 20 | Indexed under `patches/index.json` |
| Reconstructible cumulative series | 7 | All 20 local-only commits covered from public base `43e7d250` |
| Interrupted dirty follow-ups preserved | 1 | Four-file router partial patch under `patches/dirty/` |
| Server handoffs | 0 | No finding was established as a current `drasi-server` defect |

All new issues and the preservation PR are assigned to `agentofreality`.
Existing records referenced by the ledger were assigned to `agentofreality`
without removing their prior assignees. Existing open non-draft PRs were not
downgraded.

## New generic issue index

| Issue | Scope | Source evidence |
|---|---|---|
| [#891](https://github.com/drasi-project/drasi-core/issues/891) | Source startup event loss | `645af080` |
| [#892](https://github.com/drasi-project/drasi-core/issues/892) | `MATCH -> WHERE -> OPTIONAL MATCH` grammar | Historical query rejection |
| [#893](https://github.com/drasi-project/drasi-core/issues/893) | Deep logical-expression stack overflow | `8a52f6f3`, historical PR #736 |
| [#894](https://github.com/drasi-project/drasi-core/issues/894) | Source dispatch ordering across plugin ABI | `f54e829c` |
| [#895](https://github.com/drasi-project/drasi-core/issues/895) | Cypher continuous-query `UNION` | Historical application workaround |
| [#896](https://github.com/drasi-project/drasi-core/issues/896) | Dynamic StateStore durability contract | `eb777fa7`, `487df491`, existing PR #742 |
| [#897](https://github.com/drasi-project/drasi-core/issues/897) | Retained multi-source aggregate divergence | `601e2a34` |
| [#898](https://github.com/drasi-project/drasi-core/issues/898) | `coll.distinct` on unmatched OPTIONAL values | Investigation |
| [#899](https://github.com/drasi-project/drasi-core/issues/899) | Disconnected MATCH/Cartesian behavior | Investigation |
| [#900](https://github.com/drasi-project/drasi-core/issues/900) | HTTP 2xx JSON/GraphQL error handling | `16f67030`, `56745f88`, `ee649b9b`, historical PR #747 |
| [#902](https://github.com/drasi-project/drasi-core/issues/902) | Loader accepts missing/null plugin metadata without ABI validation | `44342b39`; confirmed on the recorded `main` baseline |

## Existing PR reuse

- Reuse [#810](https://github.com/drasi-project/drasi-core/pull/810) for
  #792. Do not silently add `7be2e1bd`; characterize and extract its persisted
  replay change separately.
- Reuse [#742](https://github.com/drasi-project/drasi-core/pull/742) for #896
  if it can be cleanly rebased. If it cannot, document that disposition before
  opening a replacement.
- Reuse the open durability stack:
  #825/#743, #826/#821 (and #775), #830/#822, #831/#824, #832/#819,
  #834/#820, and #835/#823.
- Historical PRs #735 and #736 merged only into a prototype base.
- Historical PRs #747 and #765 closed unmerged. Their commits are evidence, not
  upstream implementation state.
- Twenty historical commits no longer resolved through a remote branch or the
  GitHub commit API. Their sanitized diffs are preserved under
  [`patches/`](patches/) with exact original commit, parent, changed-file, size,
  and SHA-256 metadata.
- Seven cumulative series patches provide a public-base reconstruction path for
  all 20 commits. Every series passed `git apply --check` and applied to a
  fresh GitHub archive of `43e7d250`.
- The interrupted router follow-up after `25bda3bb` is preserved as an exact
  four-file partial patch. The surviving record does not substantiate a prior
  nine-file count; it contains no post-edit status or validation. The
  permanent-invalid-candidate poison-row blocker remains unrecovered.

## Recommended issue-specific implementation sequence

Each item should be a separate draft PR unless the listed issues share one
inseparable root cause.

1. **Existing active stacks:** finish review of #810 and #825-#835 rather than
   creating replacements.
2. **Source lifecycle ABI:** #774 from `1bd669a3`, isolated from WorkGraph code.
3. **Query fixes with retained tests:** #793/#794 together from `c2b86f9c`,
   then #806 from `2f129f22`, #807 from `67311899`, #808 from
   `fcce3f11` plus required `c6615b45`, and #809 from `7691d769` with the
   related replacement-update coverage.
4. **Small generic historical extractions:** #893 from `8a52f6f3`; #896 by
   reusing #742; #902 from the loader-only part of `44342b39`; #900 from the
   final `16f67030`/`56745f88`/`ee649b9b` implementation.
5. **Reproduce before porting:** #897's runtime default-transition subset is
   now covered by PR #810; reproduce its remaining persisted restart/backend
   scope and the distinct replay/result-index state-version repair before any
   extraction. Test #891 from the generic `SourceBase` part of
   `645af080`; #894 from `f54e829c` after testing current `dispatch_event`
   behavior following #827/#828/#856.
6. **Unfixed correctness issues:** #795-#799 and #805, prioritized by
   reproducibility and impact.
7. **Investigations before implementation:** #898 and #899.
8. **Language capabilities:** #800-#804, #892, and #895 according to product
   priority.

Do not create a broad WorkGraph/Core mega-PR. WorkGraph-specific Source,
allocator, router, launcher, refresh, and protocol findings are preserved as
moved or historical because ownership left Core in `b1458cb1`.

## Explicit non-issues and deferred conclusions

- The proposed Route-ingestion issue was based on stale/incomplete analysis.
- The production projector intentionally lived outside Core.
- The proposed GQL inline-predicate parser failure did not reproduce.
- One derived-slot failure used colliding node/relationship fixture IDs; #805
  remains the generic collision issue.
- The broad replay patch `8033ca12` was explicitly parked. `e26ba153` bounded
  recovery to durable provenance, while `c3b5f540` documented an unsolved
  legacy case.
- Excess-slot retirement behavior and the general accumulator empty-identity
  redesign were explicitly deferred.
