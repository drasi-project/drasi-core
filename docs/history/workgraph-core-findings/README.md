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
The final ledger preserves each stable source ID with enough generic detail to
understand its disposition without those materials.

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
- [#810](https://github.com/drasi-project/drasi-core/pull/810) contains the
  isolated #792 candidate but not the later persisted aggregate-replay repair
  in `7be2e1bd`.
- `8033ca12`, `e26ba153`, and `c3b5f540` form a deliberately parked replay
  investigation. They are not an adopted generic recovery fix.
- `2dc5d878` is remotely preserved on
  `agentofreality-source-owned-workgraph-allocator`; it is not permanently
  local-only.
- No broad empty-aggregate redesign, deletion/cancellation resilience repair,
  or excess-slot retirement implementation is inferred from narrower fixes.

## Files

- [`fix-inventory.json`](fix-inventory.json) records immutable public commits
  and existing PR reuse requirements.
- `ledger.json` will contain the complete stable-ID disposition map.
- `ledger.md` will provide the corresponding human-readable issue and ownership
  summary.

