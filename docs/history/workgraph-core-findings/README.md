# WorkGraph-era drasi-core findings

This directory preserves the generic engineering findings discovered while
developing WorkGraph against `drasi-core`. It is a historical evidence and
disposition record, not a claim that WorkGraph-specific components still belong
in Core.

Tracked by [#889](https://github.com/drasi-project/drasi-core/issues/889).

## Preservation baseline

- Core `main`: `e759606fa065bee0ef9e60017e263f0f85dd0e44`
- Retained prototype branch: `workgraph-generic-recovery`
- Retained prototype tip: `fb5d2bda2cbce4284fecbce40cae2a2d74116249`
- Source report SHA-256:
  `683a7406e1edd6778f892bd3e849d2440ac8da3fa023aac79c6ac93b37eebbda`
- Mechanical source index: 132 finding rows and seven narrative anchors

The source report and index were recovered from local development history on
September 12, 2026. Raw transcripts, private session metadata, credentials,
operator configuration, and runtime databases are intentionally not published.
A sanitized full report and exact per-record finding details are published so
the final ledger does not discard the richer technical evidence.

## Interpretation rules

1. A fix on a prototype branch is not described as merged into `main`.
2. A PR merged only into a historical prototype base is not described as an
   upstream merge.
3. Existing issues and PRs are reused rather than replaced.
4. Historical WorkGraph-specific behavior removed from Core in
   `b1458cb1c25cd39457361a1be010f2fba7c6d5ff` is marked moved/out of scope unless
   a surviving generic Core contract is demonstrated.
5. Incomplete historical evidence is recorded as an investigation, not as a
   confirmed current-main defect.
6. Explicitly parked, deferred, or withdrawn work remains identified as such.

## Existing primary records

The original Core issue set is:

- Reliability: [#774](https://github.com/drasi-project/drasi-core/issues/774),
  [#775](https://github.com/drasi-project/drasi-core/issues/775)
- Query correctness:
  [#792](https://github.com/drasi-project/drasi-core/issues/792) through
  [#799](https://github.com/drasi-project/drasi-core/issues/799), and
  [#805](https://github.com/drasi-project/drasi-core/issues/805) through
  [#809](https://github.com/drasi-project/drasi-core/issues/809)
- Query capabilities:
  [#800](https://github.com/drasi-project/drasi-core/issues/800) through
  [#804](https://github.com/drasi-project/drasi-core/issues/804)

The later durability implementation is already decomposed under
[#818](https://github.com/drasi-project/drasi-core/issues/818), with children
[#819](https://github.com/drasi-project/drasi-core/issues/819) through
[#824](https://github.com/drasi-project/drasi-core/issues/824). The associated
open PR stack starts at
[#825](https://github.com/drasi-project/drasi-core/pull/825). This preservation
work does not duplicate that stack.

## Critical corrections retained

- #774 is the missing Source `on_subscriptions_complete` dynamic-ABI callback.
  Its implementation provenance begins at `1bd669a3`; `f54e829c` is a separate
  dispatch-order repair.
- #775 is durable query-output restoration. Existing
  [#826](https://github.com/drasi-project/drasi-core/pull/826) must be reused.
- Dynamic Reaction checkpoint-ownership forwarding belongs to
  [#820](https://github.com/drasi-project/drasi-core/issues/820). Missing or
  null plugin metadata bypassing ABI checks is a distinct loader defect tracked
  by [#902](https://github.com/drasi-project/drasi-core/issues/902) and
  implemented together with #774 in draft
  [#903](https://github.com/drasi-project/drasi-core/pull/903).
- [#810](https://github.com/drasi-project/drasi-core/pull/810) contains the
  isolated #792 candidate but not the later persisted aggregate-replay repair
  in `7be2e1bd`. Its curated draft head is `929dc80f`.
- [#901](https://github.com/drasi-project/drasi-core/pull/901) is the clean
  current-main fix for #893, and
  [#904](https://github.com/drasi-project/drasi-core/pull/904) is the shared
  #793/#794 fix stacked above #810. Both issues remain open while reviewed.
- [#742](https://github.com/drasi-project/drasi-core/pull/742) remains
  historical evidence inside native WorkGraph stack `741`
  (`#737 -> #740 -> #742 -> #744`). It must not be unstacked, retargeted, or
  rebased; the clean #896 implementation is instead based above #903's ABI
  `0.15.0` layer.
- The related Server publication is
  [drasi-project/drasi-server#198](https://github.com/drasi-project/drasi-server/pull/198)
  for [drasi-project/drasi-server#196](https://github.com/drasi-project/drasi-server/issues/196).
- `8033ca12`, `e26ba153`, and `c3b5f540` form a deliberately parked replay
  investigation. They are not an adopted generic recovery fix.
- `2dc5d878` is remotely preserved on
  `agentofreality-source-owned-workgraph-allocator`; it is not permanently
  local-only.
- No broad empty-aggregate redesign, deletion/cancellation resilience repair,
  or excess-slot retirement implementation is inferred from narrower fixes.

## Files

- [`source-report-sanitized.md`](source-report-sanitized.md) preserves the full
  technical report without private development-session provenance.
- [`evidence.md`](evidence.md) preserves original detail for each of the 139
  stable source records.
- [`fix-inventory.json`](fix-inventory.json) records immutable public commits
  and existing PR reuse requirements.
- [`ledger.json`](ledger.json) contains the complete 139-entry stable-ID
  disposition map.
- [`ledger.md`](ledger.md) provides the corresponding human-readable mapping.
- [`manifest.md`](manifest.md) records publication counts, ownership, and the
  recommended issue-specific implementation sequence.
- [`provenance.json`](provenance.json) records source checksums and sanitization
  boundaries.
- [`checksums.sha256`](checksums.sha256) records SHA-256 checksums for every
  other published artifact in this directory tree.
- [`patches/index.json`](patches/index.json) inventories 20 sanitized portable
  patches for historical commits that were still available locally but no
  longer resolved through a remote branch or GitHub commit endpoint.
- [`patches/series/index.json`](patches/series/index.json) supplies verified
  cumulative reconstruction paths from public commit `43e7d250` for all 20
  local-only patches.
- [`patches/dirty/index.json`](patches/dirty/index.json) records the exact
  interrupted four-file router follow-up and its unvalidated limitations.
