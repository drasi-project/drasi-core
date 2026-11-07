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
| New generic Core issues | 15 | #891-#900, #902, #916-#919 |
| New preservation umbrella issues | 1 | #889 |
| Original Core issues enriched | 20 | #774, #775, #792-#809 |
| Additional existing issues enriched | 6 | #346, #574, #680, #721, #722, #827 |
| Existing PRs enriched | 23 | #349, #362, #422, #735-#747 subset, #765, #810, #825-#835 stack, #903 |
| Later fix PRs recorded | 4 | #901, #904, #906, drasi-project/drasi-server#198 |
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
| [#896](https://github.com/drasi-project/drasi-core/issues/896) | Dynamic StateStore durability contract | `eb777fa7`, `487df491`; #742 is historical stacked evidence only |
| [#897](https://github.com/drasi-project/drasi-core/issues/897) | In-process live-history aggregate divergence from fresh final snapshot | `601e2a34`; related PR #810 |
| [#898](https://github.com/drasi-project/drasi-core/issues/898) | `coll.distinct` on unmatched OPTIONAL values | Investigation |
| [#899](https://github.com/drasi-project/drasi-core/issues/899) | Disconnected MATCH/Cartesian behavior | Investigation |
| [#900](https://github.com/drasi-project/drasi-core/issues/900) | HTTP 2xx JSON/GraphQL error handling | `16f67030`, `56745f88`, `ee649b9b`, historical PR #747 |
| [#902](https://github.com/drasi-project/drasi-core/issues/902) | Loader accepts missing/null plugin metadata without ABI validation | `44342b39`; active draft PR #903 |
| [#916](https://github.com/drasi-project/drasi-core/issues/916) | Deterministic Cypher `sha256(text)` scalar function | `c1910236` prototype evidence |
| [#917](https://github.com/drasi-project/drasi-core/issues/917) | Cypher string-literal escape decoding | Historical query limitation; prototype test did not change the parser |
| [#918](https://github.com/drasi-project/drasi-core/issues/918) | Atomic StateStore compare-and-swap contract | `e7190b1d`, `37d0fe35` design evidence |
| [#919](https://github.com/drasi-project/drasi-core/issues/919) | Concurrent Source resume-filter registration race | Current-main `len() - 1` association |

## Existing PR reuse

- Reuse [#810](https://github.com/drasi-project/drasi-core/pull/810) for
  #792. Do not silently add `7be2e1bd`; characterize and extract its persisted
  replay change separately. Its curated draft head is `929dc80f`.
- Do **not** reuse, unstack, retarget, or rebase
  [#742](https://github.com/drasi-project/drasi-core/pull/742) for #896. It is
  native WorkGraph stack `741` (`#737 -> #740 -> #742 -> #744`), targets
  `agentofreality-github-workgraph-bootstrapper`, and remains historical
  evidence at `6cbb9e28`. The clean generic replacement is draft
  [#906](https://github.com/drasi-project/drasi-core/pull/906), based above #903
  at `fb891f1e`; it advances the ABI from `0.15.0` to `0.16.0`.
- Reuse the open durability stack:
  #825/#743, #826/#821 (and #775), #830/#822, #831/#824, #832/#819,
  #834/#820, and #835/#823.
- Reuse [#903](https://github.com/drasi-project/drasi-core/pull/903) for both
  #774 and #902. Its loader tests cover missing, null, malformed,
  version/target-incompatible, and physically smaller legacy metadata/layouts.
- [#901](https://github.com/drasi-project/drasi-core/pull/901) fixes #893.
- [#904](https://github.com/drasi-project/drasi-core/pull/904) fixes #793 and
  #794 and intentionally targets #810.
- [drasi-project/drasi-server#198](https://github.com/drasi-project/drasi-server/pull/198)
  fixes
  [drasi-project/drasi-server#196](https://github.com/drasi-project/drasi-server/issues/196).
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

1. **Existing active stacks:** finish review of #810 at `929dc80f`, #825-#835,
   #901, #903 at `fb891f1e`, #904, and #906 at `a4fcfd84` rather than creating
   replacements.
2. **Source lifecycle and loader ABI:** reuse #903 for #774 and #902; do not
   create a duplicate loader implementation.
3. **Query fixes with retained tests:** reuse #904 for #793/#794 together,
   then #806 from `2f129f22`, #807 from `67311899`, #808 from
   `fcce3f11` plus required `c6615b45`, and #809 from `7691d769` with the
   related replacement-update coverage.
4. **Small generic historical extractions:** reuse #901 for #893 and #906 for
   #896 while leaving #742/stack 741 untouched; extract #900 from the final
   `16f67030`/`56745f88`/`ee649b9b` implementation.
5. **Prove the retained/fresh baseline:** #897 is an in-process live-history
   versus fresh-final-snapshot defect, not a persistence restart. The historical
   matrix failed on `042559d9` and passed after `601e2a34`; PR #810 contains
   that full known runtime correction and fixes #897 as well as #792. Reuse it
   rather than extracting another fix; no direct `e759606f` run is claimed.
   Keep persistence concerns with #775/#821, #822, and #806. Test #891 from the
   generic `SourceBase` part of
   `645af080`; #894 from `f54e829c` after testing current `dispatch_event`
   behavior following #827/#828/#856.
6. **Unfixed correctness issues:** #795-#799 and #805, prioritized by
   reproducibility and impact.
7. **Source/state-store correctness:** fix #919's atomic subscriber/filter
   registration independently of the broader, causally unproven historical
   relation-loss symptom; design #918's provider-atomic CAS and ABI change as a
   separate layer above the active ABI stack.
8. **Investigations before implementation:** #898 and #899.
9. **Language capabilities:** #800-#804, #892, #895, #916, and #917 according
   to product priority. #916 may selectively extract the generic portions of
   `c1910236`; #917 needs a parser characterization first because that commit
   added an expectation without adding escape decoding.

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
- The historical Error-query stop assertion was coupled to an attempted
  Error-to-Stopping transition that did not survive. Current lifecycle code
  rejects that transition and skips runtime stop during teardown from Error.
- The historical Source/bootstrap state-store precedence split was removed:
  current SourceBase uses explicit-provider-first/runtime-fallback and
  BootstrapContext carries no competing state-store provider.
- The intermediate Redb `_3a_` filename encoding and proposed `srcid-<hex>`
  migration did not land. Current PR #362 code rejects unsafe source IDs and
  treats WAL files as ephemeral across upgrades.
