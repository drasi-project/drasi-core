# Sanitized per-record source evidence

This document preserves the original technical detail for every stable finding
row and narrative anchor represented in [`ledger.json`](ledger.json). Private
session identifiers, transcript locations, and local paths are removed; public
issue, pull request, commit, and branch provenance is retained.

The original source report SHA-256 is
`683a7406e1edd6778f892bd3e849d2440ac8da3fa023aac79c6ac93b37eebbda`.

<a id="report-l42"></a>
## `report-L42` - #792

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 42
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#792](https://github.com/drasi-project/drasi-core/issues/792) | Changing grouping values could leave an old result row, reverse an update, or make the destination/source groups overwrite each other. Whole-node grouping used reference identity in one place and full properties in another. | [`d4ad7421`](https://github.com/drasi-project/drasi-core/commit/d4ad74218754f597f50e9ef45562eb535069aed5): consistent grouping equivalence, correct migration/default flags, and distinct projected group identities. **Open draft [PR #810](https://github.com/drasi-project/drasi-core/pull/810)** targets `main`, using patch-identical isolated commit `62685e467e6f276f2ba4449ca8bfcb5b4aa63c2e`. |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/792
- https://github.com/drasi-project/drasi-core/pull/810

<a id="report-l43"></a>
## `report-L43` - #793

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 43
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#793](https://github.com/drasi-project/drasi-core/issues/793) | Nested optional aggregates missed the first matching child, so a parent never entered its next workflow state. | [`c2b86f9c`](https://github.com/drasi-project/drasi-core/commit/c2b86f9c537bff047c8335e86a078f39264343e2): chained optional-path propagation. **Prototype fix.** |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/793

<a id="report-l44"></a>
## `report-L44` - #794

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 44
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#794](https://github.com/drasi-project/drasi-core/issues/794) | Adding later optional paths could leave an earlier child count stuck at zero. | Same [`c2b86f9c`](https://github.com/drasi-project/drasi-core/commit/c2b86f9c537bff047c8335e86a078f39264343e2) repair. **Prototype fix.** This did not, by itself, fix every later multi-path or nested-aggregation failure. |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/794

<a id="report-l45"></a>
## `report-L45` - #806

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 45
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#806](https://github.com/drasi-project/drasi-core/issues/806) | A second aggregation discarded the insertion/default flags of a real zero-valued group. Final projection suppressed the parent row as a no-op, then later emitted an update for a row never added. | [`2f129f22`](https://github.com/drasi-project/drasi-core/commit/2f129f229f487611bebbba3c9270aca315642401): durable outer-group contributor counts preserve actual group lifecycle. Simply copying flags was insufficient because deleting a non-last zero-valued contributor could wrongly remove the outer group. **Prototype fix.** |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/806

<a id="report-l46"></a>
## `report-L46` - #807

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 46
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#807](https://github.com/drasi-project/drasi-core/issues/807) | Cyclic MATCH paths could reuse an inconsistent relationship binding and count it more than once. | [`67311899`](https://github.com/drasi-project/drasi-core/commit/6731189964f90c9fbd08af39fa0e76883bab71b3): validate solved path endpoints. **Prototype fix.** |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/807

<a id="report-l47"></a>
## `report-L47` - #808

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 47
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#808](https://github.com/drasi-project/drasi-core/issues/808) | OPTIONAL MATCH could lose or duplicate rows when partial paths could not complete; results depended on relationship arrival order and correlation across optional clauses. | [`fcce3f11`](https://github.com/drasi-project/drasi-core/commit/fcce3f11a38df8cde7355473d05fb9f90bb05baf), followed by [`c6615b45`](https://github.com/drasi-project/drasi-core/commit/c6615b450b0be85694f2e77460cf892617eb946e): parser-preserved clause identity, clause-aware fallback/correlation, and preservation of later independent candidates. **Prototype fix, including a necessary follow-up correction.** |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/808

<a id="report-l48"></a>
## `report-L48` - #809

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs with prototype fixes
- **Source subheading:** N/A
- **Sanitized source-report line:** 48
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#809](https://github.com/drasi-project/drasi-core/issues/809) | Replacing an OPTIONAL relationship endpoint removed the unmatched parent's row rather than restoring its zero/default row. An equivalent explicit Delete+Insert behaved differently. | [`7691d769`](https://github.com/drasi-project/drasi-core/commit/7691d769b58393940d3e58fe0756300589d39f04), with additional coverage in `c6615b45`: reconcile replacement-update and default-row lifecycles. **Prototype fix.** |

**Disposition rationale**

The issue remains open; a public prototype fix exists and requires an issue-specific upstream disposition.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/809

<a id="report-l64"></a>
## `report-L64` - #795

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs not fixed
- **Source subheading:** N/A
- **Sanitized source-report line:** 64
- **Published disposition:** `existing-issue-unfixed`

**Original finding detail**

> | [#795](https://github.com/drasi-project/drasi-core/issues/795) | A WHERE condition attached to OPTIONAL MATCH removes the unmatched parent row. | Used explicit relationships with OPTIONAL MATCH. A required-MATCH rewrite would not preserve unmatched parents and is not an equivalent general fix. **No Core fix found.** |

**Disposition rationale**

The generic defect is already tracked; only an application workaround was recovered.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/795

<a id="report-l65"></a>
## `report-L65` - #796

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs not fixed
- **Source subheading:** N/A
- **Sanitized source-report line:** 65
- **Published disposition:** `existing-issue-unfixed`

**Original finding detail**

> | [#796](https://github.com/drasi-project/drasi-core/issues/796) | An outer variable in a node property map parses/builds but fails during processing, e.g. `UnknownIdentifier("team")`. | Used explicit relationship topology instead of the correlated property-map form. **No Core fix found.** |

**Disposition rationale**

The generic defect is already tracked; only an application workaround was recovered.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/796

<a id="report-l66"></a>
## `report-L66` - #797

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs not fixed
- **Source subheading:** N/A
- **Sanitized source-report line:** 66
- **Published disposition:** `existing-issue-unfixed`

**Original finding detail**

> | [#797](https://github.com/drasi-project/drasi-core/issues/797) | An optional graph variable introduced after aggregation becomes unknown during evaluation. | Moved graph matches before aggregation or split queries; cardinality still had to be preserved by the application design. **No Core fix found.** |

**Disposition rationale**

The generic defect is already tracked; only an application workaround was recovered.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/797

<a id="report-l67"></a>
## `report-L67` - #798

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs not fixed
- **Source subheading:** N/A
- **Sanitized source-report line:** 67
- **Published disposition:** `existing-issue-unfixed`

**Original finding detail**

> | [#798](https://github.com/drasi-project/drasi-core/issues/798) | `IN` inside a list-comprehension filter can return the wrong membership result. | Replaced membership filtering with `coll.indexOf(existing, x) >= 0`. **No Core fix found.** |

**Disposition rationale**

The generic defect is already tracked; only an application workaround was recovered.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/798

<a id="report-l68"></a>
## `report-L68` - #799

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs not fixed
- **Source subheading:** N/A
- **Sanitized source-report line:** 68
- **Published disposition:** `existing-issue-unfixed`

**Original finding detail**

> | [#799](https://github.com/drasi-project/drasi-core/issues/799) | Numerically equal integer and floating-point values compare unequal. | Replaced count-like SUM expressions with conditional COUNT to avoid Float/Integer mismatches. Direct `0.0 = 0` and the Dogfood equal-zero case establish the bug; one filed aggregate example incorrectly compares 1 with 0 and is not supporting evidence. **No Core numeric-equality fix found.** |

**Disposition rationale**

The generic defect is already tracked; only an application workaround was recovered.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/799

<a id="report-l69"></a>
## `report-L69` - #805

- **Kind:** `finding-row`
- **Source section:** Query-engine bugs not fixed
- **Source subheading:** N/A
- **Sanitized source-report line:** 69
- **Published disposition:** `existing-issue-unfixed`

**Original finding detail**

> | [#805](https://github.com/drasi-project/drasi-core/issues/805) | Reusing an element ID for a node and a relationship silently corrupts query state. | WorkGraph uses collision-free identities. **Caller-side prevention is not Core-side rejection or a repaired identity model.** |

**Disposition rationale**

The generic defect is already tracked; only an application workaround was recovered.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/805

<a id="report-l77"></a>
## `report-L77` - #800

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** N/A
- **Sanitized source-report line:** 77
- **Published disposition:** `existing-enhancement-deferred`

**Original finding detail**

> | [#800](https://github.com/drasi-project/drasi-core/issues/800) | Deterministic ordering of aggregate collections. | **Deferred; open.** Do not assume collection order supplies a stable "latest" selection. |

**Disposition rationale**

The missing capability is already tracked and was explicitly deferred; the application workaround is not a Core implementation.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/800

<a id="report-l78"></a>
## `report-L78` - #801

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** N/A
- **Sanitized source-report line:** 78
- **Published disposition:** `existing-enhancement-deferred`

**Original finding detail**

> | [#801](https://github.com/drasi-project/drasi-core/issues/801) | String comparison in `min`/`max` aggregates. | Feasibility was investigated. A general solution affected numeric accumulator/sort-key assumptions and persistent backends; the broad redesign was not authorized. **Deferred; open.** |

**Disposition rationale**

The missing capability is already tracked and was explicitly deferred; the application workaround is not a Core implementation.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/801

<a id="report-l79"></a>
## `report-L79` - #802

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** N/A
- **Sanitized source-report line:** 79
- **Published disposition:** `existing-enhancement-deferred`

**Original finding detail**

> | [#802](https://github.com/drasi-project/drasi-core/issues/802) | Correlated EXISTS graph subqueries. | **Deferred; open.** |

**Disposition rationale**

The missing capability is already tracked and was explicitly deferred; the application workaround is not a Core implementation.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/802

<a id="report-l80"></a>
## `report-L80` - #803

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** N/A
- **Sanitized source-report line:** 80
- **Published disposition:** `existing-enhancement-deferred`

**Original finding detail**

> | [#803](https://github.com/drasi-project/drasi-core/issues/803) | GQL MATCH stages after NEXT. | **Deferred; open.** |

**Disposition rationale**

The missing capability is already tracked and was explicitly deferred; the application workaround is not a Core implementation.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/803

<a id="report-l81"></a>
## `report-L81` - #804

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** N/A
- **Sanitized source-report line:** 81
- **Published disposition:** `existing-enhancement-deferred`

**Original finding detail**

> | [#804](https://github.com/drasi-project/drasi-core/issues/804) | Expanding list values into matched rows. | **Deferred; open.** |

**Disposition rationale**

The missing capability is already tracked and was explicitly deferred; the application workaround is not a Core implementation.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/804

<a id="report-l91"></a>
## `report-L91` - `coll.distinct` on an unmatched optional collection raised `InvalidType { expected: "List" }`.

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** Additional unfiled query/function limitations
- **Sanitized source-report line:** 91
- **Published disposition:** `investigation-opened`

**Original finding detail**

> | `coll.distinct` on an unmatched optional collection raised `InvalidType { expected: "List" }`. | Internal collection use was removed from the scalar query design. **No Core fix or separate issue found; it was not established whether the underlying problem was function semantics or invalid/null input construction.** Historical investigation evidence. |

**Disposition rationale**

The historical failure lacked an isolated cause; #898 defines a current-main characterization matrix.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/898

<a id="report-l92"></a>
## `report-L92` - A disconnected task-by-worker MATCH returned no rows although each side worked independently. Other staged forms failed with `UnknownIdentifier("worker")`.

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** Additional unfiled query/function limitations
- **Sanitized source-report line:** 92
- **Published disposition:** `investigation-opened`

**Original finding detail**

> | A disconnected task-by-worker MATCH returned no rows although each side worked independently. Other staged forms failed with `UnknownIdentifier("worker")`. | The design changed to task-only queries carrying `agentId`. **No general Cartesian-MATCH fix found.** Historical investigation evidence. |

**Disposition rationale**

The disconnected-pattern behavior was worked around by changing the graph model; #899 defines the generic support/rejection contract.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/899

<a id="report-l93"></a>
## `report-L93` - Cypher UNION could not combine the frozen artifact-detail streams.

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** Additional unfiled query/function limitations
- **Sanitized source-report line:** 93
- **Published disposition:** `enhancement-opened`

**Original finding detail**

> | Cypher UNION could not combine the frozen artifact-detail streams. | Canonical artifact/detail nodes supplied the existing query instead. **Application graph-model workaround; no UNION implementation or issue found.** Historical investigation evidence. |

**Disposition rationale**

The application workaround did not add UNION support; #895 defines compatible continuous-stream semantics.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/895

<a id="report-l94"></a>
## `report-L94` - `MATCH -> WHERE -> OPTIONAL MATCH` was rejected by the supported grammar.

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** Additional unfiled query/function limitations
- **Sanitized source-report line:** 94
- **Published disposition:** `enhancement-opened`

**Original finding detail**

> | `MATCH -> WHERE -> OPTIONAL MATCH` was rejected by the supported grammar. | Clauses and trusted predicates were reorganized. **Query rewrite, not parser support added.** Historical investigation evidence. |

**Disposition rationale**

The query was rewritten, not fixed; #892 isolates the generic clause-order capability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/892

<a id="report-l95"></a>
## `report-L95` - The desired generic hash and newline-producing Cypher primitives were unavailable.

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** Additional unfiled query/function limitations
- **Sanitized source-report line:** 95
- **Published disposition:** `withdrawn-or-unsupported`

**Original finding detail**

> | The desired generic hash and newline-producing Cypher primitives were unavailable. | Hash-dependent run/event IDs became readable deterministic IDs; the Source supplied body digests. The proposed generic SHA-256 and parser escape additions were explicitly removed from the simplified scope. **No retained generic capability fix.** |

**Disposition rationale**

Generic hash/newline primitives were explicitly removed from the authorized scope; readable IDs and Source-provided digests were the application workaround.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l96"></a>
## `report-L96` - Simple root-only OPTIONAL cases were initially suspected to fail.

- **Kind:** `finding-row`
- **Source section:** Deferred query-language capabilities
- **Source subheading:** Additional unfiled query/function limitations
- **Sanitized source-report line:** 96
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Simple root-only OPTIONAL cases were initially suspected to fail. | The initial Core correctness gate did not reproduce that hypothesis. Its final exact CASE/SUM follow-up was interrupted before a result, so the earlier result cannot disprove the later mixed-number problem. |

**Disposition rationale**

The root-only OPTIONAL hypothesis did not reproduce; the interrupted CASE/SUM follow-up is retained only as supporting uncertainty for the confirmed mixed-number issue.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/799
- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l104"></a>
## `report-L104` - #774

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** The two filed reliability tickets
- **Sanitized source-report line:** 104
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | [#774](https://github.com/drasi-project/drasi-core/issues/774) | `on_subscriptions_complete` did not cross the dynamic Source ABI. A Source could prune WAL after early subscribers caught up, before later persistent subscribers restored older positions. A 60-second fallback was not a correctness guarantee. | The actual forwarding implementation was introduced in [`1bd669a3`](https://github.com/drasi-project/drasi-core/commit/1bd669a389924cc264ef071f5f97624b5618ef08): Source vtable slot, typed/boxed adapters, and SourceProxy forwarding. **Present on the retained prototype branch; not on current main; issue open.** |

**Disposition rationale**

The callback fix is public at 1bd669a3 and is distinct from f54e829c dispatch ordering.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/774

<a id="report-l105"></a>
## `report-L105` - #775

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** The two filed reliability tickets
- **Sanitized source-report line:** 105
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | [#775](https://github.com/drasi-project/drasi-core/issues/775) | After restart, in-memory query output restarted at sequence 0 despite durable rows/outbox/checkpoints. A new result could reuse sequence 1, overwrite an old outbox entry, regress the sequence, and be skipped by reactions. | Prototype repairs include [`54d02054`](https://github.com/drasi-project/drasi-core/commit/54d02054698bf34bcf3b4afa32ce6bc0480f8055) and [`e4f2af57`](https://github.com/drasi-project/drasi-core/commit/e4f2af57f8c8b1b8ac8ec24b20909e05fe7cdae3), including hydration and legacy outbox recovery. **Issue open. Later upstream [PR #826](https://github.com/drasi-project/drasi-core/pull/826) is open and unmerged**, in the recovery stack described below. |

**Disposition rationale**

The original report is continued by #821 and existing PR #826; no duplicate implementation record is needed.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/775
- https://github.com/drasi-project/drasi-core/issues/821
- https://github.com/drasi-project/drasi-core/pull/826

<a id="report-l115"></a>
## `report-L115` - Durable reaction sequence baseline lost across restart.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 115
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Durable reaction sequence baseline lost across restart. | [PR #735](https://github.com/drasi-project/drasi-core/pull/735), merged at `48f2e6ce3fc85dc58dfcf68b3a6545847d8ff695`. | **Merged into the old `agentofreality-symmetrical-telegram` prototype branch, not main.** A predecessor to the later, more complete #775 recovery problem, not proof that all restart paths were solved. |

**Disposition rationale**

Historical sequence-baseline work was a predecessor to the broader durable-output restoration issue and merged only into a prototype base.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/775
- https://github.com/drasi-project/drasi-core/issues/821
- https://github.com/drasi-project/drasi-core/pull/735

<a id="report-l116"></a>
## `report-L116` - Deep left-nested AND/OR expressions exhausted a Tokio worker's stack.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 116
- **Published disposition:** `issue-opened`

**Original finding detail**

> | Deep left-nested AND/OR expressions exhausted a Tokio worker's stack. | [PR #736](https://github.com/drasi-project/drasi-core/pull/736), flattening logical-expression evaluation; prototype merge `9bbaa29245d9eb7d31c96c38037b38bb5289aef1`. | **Historical prototype-only merge.** |

**Disposition rationale**

#893 tracks the generic Core stack-overflow fix, which was merged only to a historical prototype base.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/893
- https://github.com/drasi-project/drasi-core/pull/736

<a id="report-l117"></a>
## `report-L117` - Durable host state appeared non-durable to dynamic plugins.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 117
- **Published disposition:** `issue-opened-existing-pr`

**Original finding detail**

> | Durable host state appeared non-durable to dynamic plugins. | `StateStoreVtable` did not expose durability and the proxy inherited `false`. Original repair `efa01868`; retained lineage includes [`eb777fa7`](https://github.com/drasi-project/drasi-core/commit/eb777fa7966d04d93a4dabe6b22fbab6f75c1f3e) and ABI versioning `487df491`. | **Prototype repair. [PR #742](https://github.com/drasi-project/drasi-core/pull/742) remains an open draft on the old stack.** |

**Disposition rationale**

#896 tracks the generic StateStore durability contract and ABI versioning; its historical open draft must be reused or explicitly dispositioned.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/896
- https://github.com/drasi-project/drasi-core/pull/742

<a id="report-l118"></a>
## `report-L118` - Retained multi-source queries diverged from freshly constructed queries.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 118
- **Published disposition:** `issue-opened`

**Original finding detail**

> | Retained multi-source queries diverged from freshly constructed queries. | The live Issue/Comment/Project-state workflow exposed stale initializer/launcher/router results; historical fix `601e2a34b5bce2ec37e26f26818df35f2d27c830`. | **Repair on the old prototype lineage.** Preserve this observation separately from the later minimized #792-#809 issues. |

**Disposition rationale**

#897 tracks the historical retained/fresh divergence. PR #810 now covers 601e2a34's runtime default-transition and grouping-migration subset with a generic survival test; persisted restart/backend acceptance and the separate replay/result-index state-version repair remain open.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/897

<a id="report-l119"></a>
## `report-L119` - Startup Source changes could be lost before downstream dispatch was ready.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 119
- **Published disposition:** `issue-opened`

**Original finding detail**

> | Startup Source changes could be lost before downstream dispatch was ready. | [`645af080`](https://github.com/drasi-project/drasi-core/commit/645af0801f268af61d44f58480e44c3e3a3544be), "Preserve WorkGraph startup events." | **Retained prototype repair.** |

**Disposition rationale**

#891 tracks the generic SourceBase startup readiness/loss window despite the historical WorkGraph fixture.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/891

<a id="report-l120"></a>
## `report-L120` - Dispatch order changed across the plugin boundary.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 120
- **Published disposition:** `issue-opened`

**Original finding detail**

> | Dispatch order changed across the plugin boundary. | [`f54e829c`](https://github.com/drasi-project/drasi-core/commit/f54e829c81d6dde581001d8c20338afa6818ddd6), "Preserve dispatch order across plugin ABI." | **Retained prototype repair.** This is not #774's lifecycle callback fix. |

**Disposition rationale**

#894 tracks the generic dispatch-order defect, which is distinct from #774; current dispatch_event behavior must be characterized first.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/894

<a id="report-l121"></a>
## `report-L121` - Query output did not reconcile a changed result-row signature correctly.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 121
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Query output did not reconcile a changed result-row signature correctly. | [`01dbaf20`](https://github.com/drasi-project/drasi-core/commit/01dbaf200e2705d86604b819d443df9b93574635), changing query manager/output-state reconciliation. | **Retained prototype repair.** |

**Disposition rationale**

Changed row-signature reconciliation overlaps existing stale aggregate-row/group-migration records; preserve the commit as evidence rather than create a duplicate.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/680
- https://github.com/drasi-project/drasi-core/issues/792

<a id="report-l122"></a>
## `report-L122` - Dynamic reaction delivery could be acknowledged before the plugin callback finished, and callback failures did not propagate reliably.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 122
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Dynamic reaction delivery could be acknowledged before the plugin callback finished, and callback failures did not propagate reliably. | [`cc8d3f71`](https://github.com/drasi-project/drasi-core/commit/cc8d3f712b69fc730b91ce04edf4556e8d2b7899): await callbacks across FFI, propagate errors, fail pending deliveries on shutdown, stop forwarding after rejection; acknowledgement ABI 0.16. Follow-up `eb224595` preserves error chains. | **Retained prototype repair.** Does not by itself establish every generic checkpoint-after-side-effect guarantee. |

**Disposition rationale**

Callback completion and error propagation support the existing checkpoint-after-side-effect contract.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/820
- https://github.com/drasi-project/drasi-core/pull/834

<a id="report-l123"></a>
## `report-L123` - HTTP Reaction treated GraphQL HTTP 2xx error responses as success.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Earlier PR-only and unfiled repairs
- **Sanitized source-report line:** 123
- **Published disposition:** `issue-opened`

**Original finding detail**

> | HTTP Reaction treated GraphQL HTTP 2xx error responses as success. | [PR #747](https://github.com/drasi-project/drasi-core/pull/747), declarative response JSON validation. Retained lineage contains [`16f67030`](https://github.com/drasi-project/drasi-core/commit/16f670304b5956a41f316a0ba670d8e0aa7eab9b) and exact JSON-number classification follow-ups. | **Implementation retained on the prototype branch, but PR #747 was closed unmerged when that earlier stack was abandoned.** |

**Disposition rationale**

#900 tracks the generic HTTP response-validation implementation retained on a prototype branch after its historical PR closed unmerged.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/900
- https://github.com/drasi-project/drasi-core/pull/747

<a id="report-l135"></a>
## `report-L135` - Restoring the sequence alone left in-memory outbox/live state empty despite persistent payloads. A failed live-row write followed by a later successful watermark could certify an incomplete snapshot.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** What the older durable-output hardening actually covered
- **Sanitized source-report line:** 135
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Restoring the sequence alone left in-memory outbox/live state empty despite persistent payloads. A failed live-row write followed by a later successful watermark could certify an incomplete snapshot. | **Repaired in the custom lineage:** hydrate and reconstruct only from contiguous certified history; repair missing snapshot mutations rather than merely advancing the number. |

**Disposition rationale**

Snapshot/outbox hydration is part of the existing durable-output restoration root cause.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/775
- https://github.com/drasi-project/drasi-core/issues/821
- https://github.com/drasi-project/drasi-core/pull/826

<a id="report-l136"></a>
## `report-L136` - Lower sequence writes could regress persisted watermarks. An initial Garnet get-then-set repair still raced and had full-`u64` representation concerns.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** What the older durable-output hardening actually covered
- **Sanitized source-report line:** 136
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Lower sequence writes could regress persisted watermarks. An initial Garnet get-then-set repair still raced and had full-`u64` representation concerns. | **Repaired in `042559d9`:** monotonic Memory/RocksDB paths and atomic Garnet length/lexicographic comparison. |

**Disposition rationale**

Monotonic durable watermarks support the existing hydration and atomic-output issues.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/775
- https://github.com/drasi-project/drasi-core/issues/821
- https://github.com/drasi-project/drasi-core/issues/822

<a id="report-l137"></a>
## `report-L137` - Capacity-based outbox trimming could evict the exact history needed to repair a failed snapshot update.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** What the older durable-output hardening actually covered
- **Sanitized source-report line:** 137
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Capacity-based outbox trimming could evict the exact history needed to repair a failed snapshot update. | **Repaired in `042559d9`:** retain required repair history until snapshot/outbox continuity is restored; real gaps still fail closed. |

**Disposition rationale**

Repair-history retention is part of durable output hydration/atomicity, not a separate WorkGraph defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/775
- https://github.com/drasi-project/drasi-core/issues/821
- https://github.com/drasi-project/drasi-core/issues/822

<a id="report-l138"></a>
## `report-L138` - A fresh replay checkpoint could advance to the response's latest sequence after only part of the batch was enqueued successfully, or advance unconditionally across a gap.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** What the older durable-output hardening actually covered
- **Sanitized source-report line:** 138
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | A fresh replay checkpoint could advance to the response's latest sequence after only part of the batch was enqueued successfully, or advance unconditionally across a gap. | **Repaired in `042559d9`:** record only the last successful sequence and honor the configured gap policy. |

**Disposition rationale**

Advancing only through successful enqueue/side effects is covered by the existing reaction checkpoint issue.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/820
- https://github.com/drasi-project/drasi-core/pull/834

<a id="report-l139"></a>
## `report-L139` - Source/index progress could still commit before output persistence.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** What the older durable-output hardening actually covered
- **Sanitized source-report line:** 139
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Source/index progress could still commit before output persistence. | **Not established as atomically repaired by these commits.** Monotonic watermarks and repairable history are not a transaction spanning input progress, engine state, outbox, and snapshot. See #822/PR #830. |

**Disposition rationale**

Input progress committing before output is the atomic query-output issue.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/822
- https://github.com/drasi-project/drasi-core/pull/830

<a id="report-l147"></a>
## `report-L147` - An initial projector journal retained every full-body transition and required work proportional to history; partial batches also needed recoverable checkpoint/WAL ordering.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 147
- **Published disposition:** `historical-prototype-evidence`

**Original finding detail**

> | An initial projector journal retained every full-body transition and required work proportional to history; partial batches also needed recoverable checkpoint/WAL ordering. | [`a826c56e`](https://github.com/drasi-project/drasi-core/commit/a826c56edd369da55f6fc76069e9f95ed8305123) introduced a bounded opaque post-transition checkpoint, staged token, materialized task map, and pending-origin/offset recovery. **Prototype repair; the narrower append-success/offset-save crash window remained.** |

**Disposition rationale**

A public prototype implementation or bounded historical observation exists, but no separate current-main defect was established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l148"></a>
## `report-L148` - Authenticated VNext Assignment/Dispatch evidence never reached the old native allocator, whose issue/comment/agent identity vocabulary did not match canonical task/assignment/executor identities.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 148
- **Published disposition:** `historical-prototype-evidence`

**Original finding detail**

> | Authenticated VNext Assignment/Dispatch evidence never reached the old native allocator, whose issue/comment/agent identity vocabulary did not match canonical task/assignment/executor identities. | [`9fa2113c`](https://github.com/drasi-project/drasi-core/commit/9fa2113c844715fb253b814bd309dbfab6da69c4) implemented the native canonical allocator. An alias-only adapter was rejected. **Prototype repair, with an explicitly breaking fresh-state schema.** |

**Disposition rationale**

A public prototype implementation or bounded historical observation exists, but no separate current-main defect was established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l149"></a>
## `report-L149` - Delta-only allocator directives could retain an older lease after another artifact, definition conflict, or reparenting invalidated its graph Assignment.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 149
- **Published disposition:** `historical-prototype-evidence`

**Original finding detail**

> | Delta-only allocator directives could retain an older lease after another artifact, definition conflict, or reparenting invalidated its graph Assignment. | The same `9fa2113c` change reconciled against a **complete accepted-state snapshot**, not just the latest input's directives. **Prototype repair.** |

**Disposition rationale**

A public prototype implementation or bounded historical observation exists, but no separate current-main defect was established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l150"></a>
## `report-L150` - Dispatch/Close could destroy historical Lease facts required by subsequent lifecycle queries. Closed-task validation could discard accepted historical assignments.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 150
- **Published disposition:** `historical-prototype-evidence`

**Original finding detail**

> | Dispatch/Close could destroy historical Lease facts required by subsequent lifecycle queries. Closed-task validation could discard accepted historical assignments. | `9fa2113c` separated active slot ownership from immutable historical Lease/LEASE_FOR/detail evidence and preserved closed-task history. **Prototype repair.** |

**Disposition rationale**

A public prototype implementation or bounded historical observation exists, but no separate current-main defect was established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l151"></a>
## `report-L151` - A custom Source descriptor could not preserve unresolved SecretReference configuration; persisted properties contained `[REDACTED]` instead of reloadable references.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 151
- **Published disposition:** `historical-prototype-evidence`

**Original finding detail**

> | A custom Source descriptor could not preserve unresolved SecretReference configuration; persisted properties contained `[REDACTED]` instead of reloadable references. | [`957961c0`](https://github.com/drasi-project/drasi-core/commit/957961c0e4a6137d3d89cbdc0fb38055024e17ea) exposed `.with_raw_config(original_unresolved_json)`. **Prototype repair.** |

**Disposition rationale**

A public prototype implementation or bounded historical observation exists, but no separate current-main defect was established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l152"></a>
## `report-L152` - On a fresh empty WAL, `head == 0` opened the startup pruning fence too soon.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 152
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | On a fresh empty WAL, `head == 0` opened the startup pruning fence too soon. | `7be2e1bd` made the fence begin closed until subscription completion. **Prototype repair, distinct from merely adding #774's ABI callback.** |

**Disposition rationale**

The startup fence complements #774; lost-WAL gap handling is already tracked by #721.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/774
- https://github.com/drasi-project/drasi-core/issues/721

<a id="report-l153"></a>
## `report-L153` - Positionless replay from `oldest - 1` could silently accept incomplete retained history.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 153
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | Positionless replay from `oldest - 1` could silently accept incomplete retained history. | `7be2e1bd` accepts replay from zero only when the WAL starts at sequence 1; pruned nonempty history fails closed with reset instructions. **Prototype repair.** |

**Disposition rationale**

Incomplete positionless replay is covered by existing lost-WAL and surviving-index bootstrap issues.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/721
- https://github.com/drasi-project/drasi-core/issues/722

<a id="report-l154"></a>
## `report-L154` - One subscriber's index lacked a relation despite sharing the same later checkpoint as an intact subscriber.

- **Kind:** `finding-row`
- **Source section:** Restart, delivery, and plugin-boundary defects
- **Source subheading:** Additional Source integration and persistence repairs
- **Sanitized source-report line:** 154
- **Published disposition:** `historical-investigation-unconfirmed`

**Original finding detail**

> | One subscriber's index lacked a relation despite sharing the same later checkpoint as an intact subscriber. | Dogfood Issue #87; exact offline mapping/query replay worked, and the distinguishing WAL evidence had already been pruned. **Cause unresolved: no proof separating loss from late/out-of-order transport.** The proposed contiguous-sequence gap detection was not shown to repair this exact case. |

**Disposition rationale**

The distinguishing WAL evidence had already been pruned, so loss versus late/out-of-order transport was never established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l181"></a>
## `report-L181` - Canonical task Issue admission checked transport/object shape without a corresponding creator/editor/sender provenance gate.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 181
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Canonical task Issue admission checked transport/object shape without a corresponding creator/editor/sender provenance gate. | **Diagnosis only at the audit checkpoint.** Whether manual canonical tasks should be admitted is a policy decision; the proposed actor/provenance restriction was not implemented in that audit. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l182"></a>
## `report-L182` - Lifecycle deletion checked the original comment author rather than the deleting sender.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 182
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Lifecycle deletion checked the original comment author rather than the deleting sender. | **Diagnosed and not repaired by that audit.** Do not confuse author trust with authorization of a later destructive action. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l183"></a>
## `report-L183` - Removing a marker or changing Issue Type could bypass the old ownership path and leave stale workflow evidence.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 183
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Removing a marker or changing Issue Type could bypass the old ownership path and leave stale workflow evidence. | **Confirmed and deferred at the audit checkpoint.** Later Source rewrites and lifecycle-revision handling repaired portions; do not assume the entire old mutation matrix was closed. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l184"></a>
## `report-L184` - Ordinary-to-canonical and canonical-to-ordinary reclassification could leave evidence in both graph namespaces.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 184
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Ordinary-to-canonical and canonical-to-ordinary reclassification could leave evidence in both graph namespaces. | **Diagnosed, no atomic dual-namespace cleanup implemented at that checkpoint.** Later removal of the legacy architecture changes applicability; it is not evidence that the original patch was made. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l185"></a>
## `report-L185` - A trusted artifact rewritten into another role by an untrusted editor could leave its previously accepted evidence active.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 185
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | A trusted artifact rewritten into another role by an untrusted editor could leave its previously accepted evidence active. | **Confirmed in the audit.** Later `c3813ee1` adds prior-artifact retraction for untrusted cross-role edits. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l186"></a>
## `report-L186` - Late edits/removals under new delivery IDs could resurrect deleted state or remove a newer incarnation.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 186
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Late edits/removals under new delivery IDs could resurrect deleted state or remove a newer incarnation. | Delivery-ID dedupe was not entity-version/tombstone ordering. **Deferred initially; later task-generation and comment-revision fencing addressed specific paths.** |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l187"></a>
## `report-L187` - DeleteTask could retain task-bound lifecycle documents, allowing the old chain to return on same-source restoration.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 187
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | DeleteTask could retain task-bound lifecycle documents, allowing the old chain to return on same-source restoration. | **Diagnosed lifecycle-generation gap.** A genuinely recreated GitHub Issue with a new node ID did not inherit the old chain. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l188"></a>
## `report-L188` - Deleting Dispatch evidence could make an old Assignment eligible again and launch duplicate work. Deleting Result/Evaluation could strand or regress lifecycle state.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 188
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Deleting Dispatch evidence could make an old Assignment eligible again and launch duplicate work. Deleting Result/Evaluation could strand or regress lifecycle state. | **Confirmed; no complete cancellation, repair, and retirement closure was found in the original audit.** Do not equate graph retraction with cancellation of an already launched Agent Task. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l189"></a>
## `report-L189` - Sparse `sub_issue_removed` payloads containing only a numeric child ID did not retract canonical parent linkage.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 189
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Sparse `sub_issue_removed` payloads containing only a numeric child ID did not retract canonical parent linkage. | **Later repaired** by [`e1b56a29`](https://github.com/drasi-project/drasi-core/commit/e1b56a2902e3e30904a0f67dff2383968a84ed6d): durable database-ID lookup and authoritative hierarchy convergence. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l190"></a>
## `report-L190` - Deleting a completed child after its parent was assigned could leave the parent authorized for downstream execution with incomplete child evidence.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 190
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Deleting a completed child after its parent was assigned could leave the parent authorized for downstream execution with incomplete child evidence. | A **WorkGraph projector/query/reaction correctness gap**, not a generic Core parser bug. The audit reported it; its complete repair was not established before that work was stopped. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l191"></a>
## `report-L191` - WAL append and pending-offset persistence were separate; the classic delivery marker was also written after state/WAL.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 191
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | WAL append and pending-offset persistence were separate; the classic delivery marker was also written after state/WAL. | **Residual at-least-once duplication window.** A crash after append but before recording progress can append the same raw changes again. Stable IDs are not a general exactly-once side-effect guarantee. |

**Disposition rationale**

The append/progress crash window belongs to the existing atomic output/checkpoint boundary.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/822
- https://github.com/drasi-project/drasi-core/pull/830

<a id="report-l192"></a>
## `report-L192` - `last_dispatched` advanced while constructing a batch, before a successful downstream send.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 192
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | `last_dispatched` advanced while constructing a batch, before a successful downstream send. | **Unfixed at the audit checkpoint.** A send failure could wedge the running process until resubscription/restart despite retained WAL entries. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l193"></a>
## `report-L193` - WAL deletion and Source state clearing were separate deprovision operations.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 193
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | WAL deletion and Source state clearing were separate deprovision operations. | **Unfixed at the audit checkpoint.** Partial failure could restore an old projector/allocator checkpoint into a fresh WAL; merely reversing operation order was not a full solution. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l194"></a>
## `report-L194` - Already-dispatched external work and append-only journals lacked cancellation/retirement.

- **Kind:** `finding-row`
- **Source section:** Explicitly stopped deletion-resilience work
- **Source subheading:** N/A
- **Sanitized source-report line:** 194
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Already-dispatched external work and append-only journals lacked cancellation/retirement. | **Residual orphaning/GC work.** Local deletion released native allocator capacity correctly; this was not a slot leak. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l208"></a>
## `report-L208` - Generated typed tasks carrying the admission label could be misclassified as ordinary Root Issues.

- **Kind:** `finding-row`
- **Source section:** Later Source corrections and misleading leads
- **Source subheading:** N/A
- **Sanitized source-report line:** 208
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Generated typed tasks carrying the admission label could be misclassified as ordinary Root Issues. | Fixed with durable hierarchy handling in `e1b56a29`; generated tasks are excluded from Root Issue admission. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l209"></a>
## `report-L209` - Stale/equal-revision Issue events could reopen or reauthorize state incorrectly.

- **Kind:** `finding-row`
- **Source section:** Later Source corrections and misleading leads
- **Source subheading:** N/A
- **Sanitized source-report line:** 209
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Stale/equal-revision Issue events could reopen or reauthorize state incorrectly. | Task-revision and authorization-generation fences were added through `16cc7d3c`, `95fa3647`, and `2a6fa78e`. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l210"></a>
## `report-L210` - The lease validation consumer inferred the attempt instead of receiving the authoritative active-lease attempt.

- **Kind:** `finding-row`
- **Source section:** Later Source corrections and misleading leads
- **Source subheading:** N/A
- **Sanitized source-report line:** 210
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | The lease validation consumer inferred the attempt instead of receiving the authoritative active-lease attempt. | The contract was made explicit in `22c8a52b`, followed by `509a3bf3` and `726c2e27`. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l211"></a>
## `report-L211` - Lifecycle producers and the Source disagreed on exact Assignment/Evaluation marker names, and Error artifacts needed ingestion.

- **Kind:** `finding-row`
- **Source section:** Later Source corrections and misleading leads
- **Source subheading:** N/A
- **Sanitized source-report line:** 211
- **Published disposition:** `moved-workgraph-out-of-core`

**Original finding detail**

> | Lifecycle producers and the Source disagreed on exact Assignment/Evaluation marker names, and Error artifacts needed ingestion. | [`c3813ee1`](https://github.com/drasi-project/drasi-core/commit/c3813ee178d6dffe4d2dff5afc5f7b6d1cd1dead) changes `WorkGraphTaskAssign/v1` to `WorkGraphTaskAssignment/v1`, `WorkGraphTaskEvaluate/v1` to `WorkGraphTaskEvaluation/v1`, adds `WorkGraphTaskError/v1`, and introduces lifecycle-comment revision handling. |

**Disposition rationale**

This behavior belonged to the WorkGraph Source/protocol removed from Core in b1458cb1; partial historical repairs and remaining limits are preserved without filing a current Core defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l235"></a>
## `report-L235` - #821

- **Kind:** `finding-row`
- **Source section:** Later upstream durability follow-up
- **Source subheading:** N/A
- **Sanitized source-report line:** 235
- **Published disposition:** `existing-durability-follow-up`

**Original finding detail**

> | [#821](https://github.com/drasi-project/drasi-core/issues/821) | Hydrate durable QueryOutputState before processing; directly continues #775. | [#826](https://github.com/drasi-project/drasi-core/pull/826) |

**Disposition rationale**

This later issue already decomposes the durable reaction recovery work under #818.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/821
- https://github.com/drasi-project/drasi-core/pull/826

<a id="report-l236"></a>
## `report-L236` - #822

- **Kind:** `finding-row`
- **Source section:** Later upstream durability follow-up
- **Source subheading:** N/A
- **Sanitized source-report line:** 236
- **Published disposition:** `existing-durability-follow-up`

**Original finding detail**

> | [#822](https://github.com/drasi-project/drasi-core/issues/822) | Commit query output, index changes, and source checkpoints atomically. | [#830](https://github.com/drasi-project/drasi-core/pull/830) |

**Disposition rationale**

This later issue already decomposes the durable reaction recovery work under #818.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/822
- https://github.com/drasi-project/drasi-core/pull/830

<a id="report-l237"></a>
## `report-L237` - #824

- **Kind:** `finding-row`
- **Source section:** Later upstream durability follow-up
- **Source subheading:** N/A
- **Sanitized source-report line:** 237
- **Published disposition:** `existing-durability-follow-up`

**Original finding detail**

> | [#824](https://github.com/drasi-project/drasi-core/issues/824) | Reject durable reactions attached to volatile queries. | [#831](https://github.com/drasi-project/drasi-core/pull/831) |

**Disposition rationale**

This later issue already decomposes the durable reaction recovery work under #818.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/824
- https://github.com/drasi-project/drasi-core/pull/831

<a id="report-l238"></a>
## `report-L238` - #819

- **Kind:** `finding-row`
- **Source section:** Later upstream durability follow-up
- **Source subheading:** N/A
- **Sanitized source-report line:** 238
- **Published disposition:** `existing-durability-follow-up`

**Original finding detail**

> | [#819](https://github.com/drasi-project/drasi-core/issues/819) | Prevent fresh trigger reactions from replaying retained history. | [#832](https://github.com/drasi-project/drasi-core/pull/832) |

**Disposition rationale**

This later issue already decomposes the durable reaction recovery work under #818.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/819
- https://github.com/drasi-project/drasi-core/pull/832

<a id="report-l239"></a>
## `report-L239` - #820

- **Kind:** `finding-row`
- **Source section:** Later upstream durability follow-up
- **Source subheading:** N/A
- **Sanitized source-report line:** 239
- **Published disposition:** `existing-durability-follow-up`

**Original finding detail**

> | [#820](https://github.com/drasi-project/drasi-core/issues/820) | Advance reaction checkpoints only after side effects complete. | [#834](https://github.com/drasi-project/drasi-core/pull/834) |

**Disposition rationale**

This later issue already decomposes the durable reaction recovery work under #818.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/820
- https://github.com/drasi-project/drasi-core/pull/834

<a id="report-l240"></a>
## `report-L240` - #823

- **Kind:** `finding-row`
- **Source section:** Later upstream durability follow-up
- **Source subheading:** N/A
- **Sanitized source-report line:** 240
- **Published disposition:** `existing-durability-follow-up`

**Original finding detail**

> | [#823](https://github.com/drasi-project/drasi-core/issues/823) | Clear prior query output on reconfigure/delete-and-recreate. | [#835](https://github.com/drasi-project/drasi-core/pull/835) |

**Disposition rationale**

This later issue already decomposes the durable reaction recovery work under #818.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/823
- https://github.com/drasi-project/drasi-core/pull/835

<a id="report-l267"></a>
## `report-L267` - Source-wide middleware could drop unrelated graph elements or fail to reconcile prior derived output after an update.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 267
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Source-wide middleware could drop unrelated graph elements or fail to reconcile prior derived output after an update. | `924d01c33158220898a3f2bb6f86686949ad4205` added preserving/reconciling middleware behavior. `a50fff73` corrected documentation that wrongly mapped Update as Insert. **Historical implementation, later integrated by the coordinator.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l268"></a>
## `report-L268` - Malformed JSON edits left a previously derived WorkGraph event alive because parsing failed before reconciliation.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 268
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Malformed JSON edits left a previously derived WorkGraph event alive because parsing failed before reconciliation. | **Configuration repair:** parse in place with `on_error: skip`, then type-gate the reconciliation stage so malformed input produces an empty derived set and retracts old output. This was not a new JSON parser implementation. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l269"></a>
## `report-L269` - Project refresh mishandled secondary-rate-limit 403s and assumed the field was literally named Status.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 269
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Project refresh mishandled secondary-rate-limit 403s and assumed the field was literally named Status. | `e63c14e9` and `266f187f` added bounded rate-limit handling and configurable field names while preserving permanent handling for other 403s. **Historical fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l270"></a>
## `report-L270` - Project refresh used the wrong deterministic status-node identity and insufficient Project/field/destination authority checks; retry could bypass current-row constraints.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 270
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Project refresh used the wrong deterministic status-node identity and insufficient Project/field/destination authority checks; retry could bypass current-row constraints. | `41c2297d`, `403be11d`, `6602e16e`, and `650533bd` corrected identity and authority checks. **Historical fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l271"></a>
## `report-L271` - Refresh scanned the full Redb state on every add; cancellation could strand the pruning permit.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 271
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Refresh scanned the full Redb state on every add; cancellation could strand the pruning permit. | `23a06f98` throttled scans and `4a1d058e` made the permit cancellation-safe. **Historical fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l272"></a>
## `report-L272` - Launcher/refresh examples used config shapes that the actual descriptor/server could not load.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 272
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Launcher/refresh examples used config shapes that the actual descriptor/server could not load. | Flat descriptor-compatible configuration and secret-reference examples were corrected. Final refresh equivalents included `e56e5493`/`4a1ee908`; launcher equivalents included `a373d466`/`5160cf53`. **Documentation/config integration repair, not an engine grammar bug.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l273"></a>
## `report-L273` - A duplicate launcher request after allowlist narrowing could overwrite a durable Started/task identity with Failed.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 273
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A duplicate launcher request after allowlist narrowing could overwrite a durable Started/task identity with Failed. | `926919b41ba2509904d911e7b6841c3143dd11d8` loaded existing durable state before mutable allowlists and removed the blind overwrite. **Historical fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l274"></a>
## `report-L274` - Launcher wire fields, prompt key, authoritative subject/Project/content/profile binding, and token-owner verification were inconsistent or incomplete.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 274
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Launcher wire fields, prompt key, authoritative subject/Project/content/profile binding, and token-owner verification were inconsistent or incomplete. | Corrections culminated in `a373d4662042ec1853145f2533be741c49429c78`; the integrated subtree-identical equivalent was `5160cf53aba477322688758371349e2ac65f793d`. **Parallel integration lineages, not proof that one SHA is the other's ancestor.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l275"></a>
## `report-L275` - Router reservation relied on instance-local get/set locking, which was not an atomic cross-instance primitive. A fallback `create_if_absent` also falsely promised atomicity.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 275
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Router reservation relied on instance-local get/set locking, which was not an atomic cross-instance primitive. A fallback `create_if_absent` also falsely promised atomicity. | `e7190b1d` and `37d0fe35` introduced real provider CAS or explicit Unsupported, plus reservation fencing; `90403176` fenced individual effects. **Original custom-lineage repair.** The later Source-owned allocator deliberately retained a single-active-instance contract, not equivalent HA guarantees. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l276"></a>
## `report-L276` - Extending a vtable while keeping its ABI version could read beyond an older physically smaller table.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 276
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Extending a vtable while keeping its ABI version could read beyond an older physically smaller table. | Router CAS and later durability-port work corrected ABI versions and rejected incompatible hosts/plugins. **A nullable trailing slot was not a valid compatibility strategy.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l277"></a>
## `report-L277` - Router used a nonexistent mutation, omitted real bearer auth, or could write against stale/closed/incorrectly correlated items.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 277
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Router used a nonexistent mutation, omitted real bearer auth, or could write against stale/closed/incorrectly correlated items. | The real `updateProjectV2ItemFieldValue` path, fresh per-effect checks, and auth were corrected through `e7190b1d`, `90403176`, and `ff0dce92`. **Historical fixes.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l278"></a>
## `report-L278` - Router could silently acknowledge an unresolved foreign-policy reservation, accept invalid output ownership/responsibilities, or advance a non-strict checkpoint past unresolved work.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 278
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Router could silently acknowledge an unresolved foreign-policy reservation, accept invalid output ownership/responsibilities, or advance a non-strict checkpoint past unresolved work. | Persisted-policy resumption/error handling and unresolved-sequence barriers were added through `e7190b1d`/`ff0dce92`. **Historical fixes.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l279"></a>
## `report-L279` - Explicit empty router allowlists were treated as omitted and regained permissive defaults; persisted old-policy decisions could bypass current transition restrictions.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 279
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Explicit empty router allowlists were treated as omitted and regained permissive defaults; persisted old-policy decisions could bypass current transition restrictions. | `26d48cde88d7a49684846d46d8f3df407930804b` distinguished omission from explicit emptiness and validated persisted as well as new decisions. **Historical custom-lineage fixes.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l280"></a>
## `report-L280` - The manager acknowledged router enqueue rather than completed side effects.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 280
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> | The manager acknowledged router enqueue rather than completed side effects. | The same `26d48cde` introduced `ManagerCheckpointOwnership`/`Reaction::checkpoint_ownership()` on **August 13**, before the later HTTP recurrence. **A reusable capability was implemented, but that did not establish adoption by every asynchronous Reaction.** |

**Disposition rationale**

Manager acknowledgement before completed side effects is the existing checkpoint-after-side-effect root cause.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/820
- https://github.com/drasi-project/drasi-core/pull/834

<a id="report-l281"></a>
## `report-L281` - Dynamic ReactionProxy lost that checkpoint-ownership capability because its vtable omitted the hook.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 281
- **Published disposition:** `existing-issue-prototype-fix`

**Original finding detail**

> | Dynamic ReactionProxy lost that checkpoint-ownership capability because its vtable omitted the hook. | `44342b390c501032a24519c087cb4df4cd1e539f` forwarded `checkpoint_ownership_fn` across the Reaction ABI. **Distinct from #774's Source subscription-complete callback and from the later callback-completion acknowledgement protocol.** |

**Disposition rationale**

The historical ReactionProxy omitted checkpoint_ownership_fn, so dynamic reactions could silently fall back to manager-owned acknowledgement. Commit 44342b39 forwards the hook; #820 owns the generic checkpoint-after-side-effect contract.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/820

<a id="report-l282"></a>
## `report-L282` - The plugin loader could skip ABI compatibility checks when metadata was absent.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 282
- **Published disposition:** `issue-opened`

**Original finding detail**

> | The plugin loader could skip ABI compatibility checks when metadata was absent. | The same `44342b39` made metadata required. **Historical custom-lineage repair.** |

**Disposition rationale**

Missing or null drasi_plugin_metadata still bypasses ABI compatibility validation on the recorded main baseline. #902 tracks fail-closed metadata enforcement; historical commit 44342b39 is fix provenance.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/902

<a id="report-l283"></a>
## `report-L283` - A reservation key could resume a different candidate after partial effects; fresh reaction-owned replay could race its initial config-hash seed.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 283
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A reservation key could resume a different candidate after partial effects; fresh reaction-owned replay could race its initial config-hash seed. | `25bda3bb2ef6eebef60dd6babdf69138466babae` introduced immutable-context fingerprints and seeded configuration before enqueue. **Partial historical repair:** the next review still required rejecting legacy empty fingerprints with prior progress and including `outcome` in the fingerprint. No completed child follow-up for those two requirements was recovered. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l284"></a>
## `report-L284` - A permanently invalid router candidate could poison the entire strict stream.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 284
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A permanently invalid router candidate could poison the entire strict stream. | **Completion unresolved in the retrieved history.** Repeated requests demanded durable terminal rejection while allowing later candidates and preserving transient retries; the child did not provide a final completion SHA for that exact blocker. Do not treat unrelated fencing/auth repairs as its resolution. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l285"></a>
## `report-L285` - PR #739 review exposed unsafe partial-side-effect recovery, ambiguous recreation, assignment conflicts, and canonical byte differences.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Middleware, launcher, router, and Project refresh
- **Sanitized source-report line:** 285
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | PR #739 review exposed unsafe partial-side-effect recovery, ambiguous recreation, assignment conflicts, and canonical byte differences. | Final `d860611bd75c3790fcc75d3f03ae04c76b52fec0` reported the bounded corrections: durable intent first, no blind recreation after ambiguous sends, trusted-assignment coalescing, ownership serialization, exact JSON/summary bytes, and adaptive HTTP policy. **Pushed to an open draft, then frozen.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l295"></a>
## `report-L295` - Collapsed Assignment/Result parsing disagreed with producer bytes, final newline, summary, and validation-profile contract.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 295
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Collapsed Assignment/Result parsing disagreed with producer bytes, final newline, summary, and validation-profile contract. | The accepted #744-era pin was `59bc6dd31f7106737337ff25faa0586ff92569f4`, not the earlier held candidates. **Historical parser fix; the comment protocol was later superseded by task Issues.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l296"></a>
## `report-L296` - Typing/untyping an Issue did not reclassify its existing comments.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 296
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Typing/untyping an Issue did not reclassify its existing comments. | **Explicitly not fixed in the payload-only Source.** The webhook lacks those comment IDs/bodies; an API read or bootstrap-seeded cache was needed for safe reconciliation. Historical investigation evidence. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l297"></a>
## `report-L297` - The supported request-info task could not select `issue-info-requester`.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 297
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | The supported request-info task could not select `issue-info-requester`. | `4c0d037d` corrected the whitelist in the #746 lineage. **Historical fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l298"></a>
## `report-L298` - Labels/state had inconsistent or absent shapes for the queries.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 298
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Labels/state had inconsistent or absent shapes for the queries. | `5ec967a2` and `a575af4f` normalized state, reason, ordered label arrays, and derived convenience fields through the shared mapper. **Source contract improvement, not an engine fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l299"></a>
## `report-L299` - Task Source/bootstrap handling diverged across type changes, filtered repositories, pagination, parent-repository shapes, reverse-order sub-Issue links, malformed-body repair, and database-ID tombstones.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 299
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Task Source/bootstrap handling diverged across type changes, filtered repositories, pagination, parent-repository shapes, reverse-order sub-Issue links, malformed-body repair, and database-ID tombstones. | Multiple corrections were carried by #746's `409790b0` checkpoint and consumed by the HTTP-validation stack. **Historical convergence fixes; later architecture changes replaced portions.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l300"></a>
## `report-L300` - Comment-backed Lease events could forge/cross-bind endings, corrupt shared identities, overcount active leases, resurrect ended leases on replay, or fail to restore state after an end edit/delete.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 300
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Comment-backed Lease events could forge/cross-bind endings, corrupt shared identities, overcount active leases, resurrect ended leases on replay, or fail to restore state after an end edit/delete. | `43f28285` used role-specific author/editor trust, exact anchors, and deterministic current-artifact folding. **Historical fixes, later replaced by Source-owned allocation.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l301"></a>
## `report-L301` - Worker configuration missed force/create/delete push cases; remote fetches blocked a shared webhook critical section.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 301
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Worker configuration missed force/create/delete push cases; remote fetches blocked a shared webhook critical section. | `43f28285` corrected push matching and moved network work out of the shared gate. **Historical fix.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l302"></a>
## `report-L302` - Worker-ledger persistence preceded WAL, and bootstrap's temporary ledger did not align with live lifecycle state.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 302
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Worker-ledger persistence preceded WAL, and bootstrap's temporary ledger did not align with live lifecycle state. | `43f28285` reordered graph/ledger/delivery recording and reconciled historical comments. **Historical repair, not a claim of cross-provider transactionality.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l303"></a>
## `report-L303` - The minimal dispatcher only suppressed pending work within one scope and lacked some row-identity/worker/repository guards.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 303
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | The minimal dispatcher only suppressed pending work within one scope and lacked some row-identity/worker/repository guards. | `198ecb48` added global pending suppression, scoped exact cleanup, consistency checks, and token redaction; `3064be2d` corrected sorting. **Implemented, then the dispatcher architecture was superseded; PR #765 closed unmerged.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l304"></a>
## `report-L304` - A trusted Assignment posted after task closure could allocate new work.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 304
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A trusted Assignment posted after task closure could allocate new work. | Locally repaired in the August 22 Source-owned allocation checkpoint. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l305"></a>
## `report-L305` - Assignment edit/retract/recreate reused lease identity, allowing a stale Result to release a new allocation.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 305
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Assignment edit/retract/recreate reused lease identity, allowing a stale Result to release a new allocation. | Locally repaired with monotonic assignment attempts and incompatible old-state rejection in the same checkpoint. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l306"></a>
## `report-L306` - TaskCancelled deauthorization depended on artifact map ordering.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 306
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | TaskCancelled deauthorization depended on artifact map ordering. | Locally repaired so deauthorization no longer depended on whether artifact IDs sorted before or after Result. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l307"></a>
## `report-L307` - The webhook signing secret could equal the lease-validation bearer secret.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Payload Source, comment-backed leases, and the early allocator
- **Sanitized source-report line:** 307
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | The webhook signing secret could equal the lease-validation bearer secret. | Locally repaired by rejecting equality of resolved values without exposing them. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l317"></a>
## `report-L317` - Invalid GraphQL selections, unsupported App attribution, and incorrect Project-item locator shapes.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 317
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Invalid GraphQL selections, unsupported App attribution, and incorrect Project-item locator shapes. | Corrected query fragments/shapes; unavailable attribution was removed rather than fabricated. Same-user credential attribution remained a documented limit. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l318"></a>
## `report-L318` - Secret exposure from properties, followed by an overcorrection that stripped required configuration and broke snapshot/recreate; a masked literal authorization header.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 318
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Secret exposure from properties, followed by an overcorrection that stripped required configuration and broke snapshot/recreate; a masked literal authorization header. | Config round-trip and actual bearer-header behavior were corrected. Silently dropping required secret references was explicitly rejected as a solution. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l319"></a>
## `report-L319` - Fatal hydrator failure left the Source reporting Running/accepting work; stop/restart could detach or revive old tasks/listeners; timeouts were missing.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 319
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Fatal hydrator failure left the Source reporting Running/accepting work; stop/restart could detach or revive old tasks/listeners; timeouts were missing. | Source supervision, bounded requests, and stop-before-restart were hardened. Parts of that machinery were then removed with the hydrator architecture. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l320"></a>
## `report-L320` - Dedupe check/append/mark races and indefinitely growing markers.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 320
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Dedupe check/append/mark races and indefinitely growing markers. | Admission durability and serialization were corrected, with bounded retention rather than a claim of eternal dedupe. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l321"></a>
## `report-L321` - Missing/nested pagination and partial GraphQL errors could appear to be a complete inventory and delete real graph objects; reconciliation could race deletion or stale scope.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 321
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Missing/nested pagination and partial GraphQL errors could appear to be a complete inventory and delete real graph objects; reconciliation could race deletion or stale scope. | Corrected in the original inventory/hydration passes. That inventory architecture was later removed. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l322"></a>
## `report-L322` - Bootstrap discarded durable adjacency, making early deletion ineffective; incorrect ownership fallback deleted the wrong Project Item; archived replay could remove restored items.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 322
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Bootstrap discarded durable adjacency, making early deletion ineffective; incorrect ownership fallback deleted the wrong Project Item; archived replay could remove restored items. | Bootstrap/authoritative-state coordination was corrected in the historical design, then superseded. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l323"></a>
## `report-L323` - Concurrent query bootstrap could deliver an older snapshot after a newer one or deadlock on a bounded waiting queue.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 323
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Concurrent query bootstrap could deliver an older snapshot after a newer one or deadlock on a bounded waiting queue. | Per-query ready gating and an atomic prepared reconciliation record were introduced, then removed/reworked with the simplified Source. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l324"></a>
## `report-L324` - A missing/deleted/unsupported object at the FIFO head could retry forever.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** The abandoned general-purpose GitHub Source implementation
- **Sanitized source-report line:** 324
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A missing/deleted/unsupported object at the FIFO head could retry forever. | Bounded absence/tombstone/terminal handling was added; later payload-only deletes used signed locators without hydration, eliminating that specific failure class. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l334"></a>
## `report-L334` - Dedupe compaction rescanned and individually read all markers after every admission, producing quadratic work over a backlog.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 334
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Dedupe compaction rescanned and individually read all markers after every admission, producing quadratic work over a backlog. | Historical investigation evidence. **Intermediate performance defect; different from the refresh Reaction's pruning hot path.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l335"></a>
## `report-L335` - Newly stopping Error queries could hit `DrasiQuery::stop`'s debug assertion, which did not accept Error.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 335
- **Published disposition:** `historical-investigation-unconfirmed`

**Original finding detail**

> | Newly stopping Error queries could hit `DrasiQuery::stop`'s debug assertion, which did not accept Error. | sanitized transcript location. **Coupled regression in the attempted global lifecycle cleanup; subsequent correction was requested.** |

**Disposition rationale**

This was a coupled regression in an attempted lifecycle cleanup; the final surviving code path was not established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l336"></a>
## `report-L336` - A new query's bootstrap overwrote the shared reconciliation baseline without applying that snapshot to older queries.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 336
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A new query's bootstrap overwrote the shared reconciliation baseline without applying that snapshot to older queries. | sanitized transcript location. **Existing queries could remain stale while future reconciliation saw no difference.** The historical bootstrap redesign addressed the problem family before that architecture was removed. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l337"></a>
## `report-L337` - Source and bootstrap selected different state stores because their precedence rules differed.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 337
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Source and bootstrap selected different state stores because their precedence rules differed. | sanitized transcript location. **A shared effective provider was required; not a query-language defect.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l338"></a>
## `report-L338` - Broadcast subscriptions bypassed bootstrapping-query exclusion, allowing delta plus full-snapshot duplication.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 338
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Broadcast subscriptions bypassed bootstrapping-query exclusion, allowing delta plus full-snapshot duplication. | sanitized transcript location. **Broadcast rejection or proper per-receiver targeting was required in the obsolete bootstrap architecture.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l339"></a>
## `report-L339` - A pending committed delta could be cleared after a successful dispatch with no ready subscribers.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 339
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | A pending committed delta could be cleared after a successful dispatch with no ready subscribers. | sanitized transcript location. **The requested repair retained pending work until an eligible subscriber existed.** A success return without a receiver was not delivery proof. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l340"></a>
## `report-L340` - Redb WAL filename escaping could map `a:b` and `a_3a_b` to the same file.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 340
- **Published disposition:** `historical-investigation-unconfirmed`

**Original finding detail**

> | Redb WAL filename escaping could map `a:b` and `a_3a_b` to the same file. | sanitized transcript location. **Introduced shared-WAL identity collision in the intermediate change.** Broad WAL changes were later excluded; no current surviving collision is asserted here. |

**Disposition rationale**

The collision was introduced by an intermediate shared-WAL change later excluded; no current surviving path is asserted.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l341"></a>
## `report-L341` - The proposed switch to `srcid-<hex>.redb` could open a new empty WAL instead of the existing legacy file.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 341
- **Published disposition:** `historical-investigation-unconfirmed`

**Original finding detail**

> | The proposed switch to `srcid-<hex>.redb` could open a new empty WAL instead of the existing legacy file. | sanitized transcript location. **Follow-on migration/access regression**, not a safe repair merely because the new names were collision-free. |

**Disposition rationale**

The proposed filename fix had a legacy migration regression; broad WAL changes were later excluded.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l342"></a>
## `report-L342` - Concurrent subscriptions could associate a resume watermark with the wrong dispatcher index by reading `len() - 1` after releasing the registration lock.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 342
- **Published disposition:** `historical-investigation-unconfirmed`

**Original finding detail**

> | Concurrent subscriptions could associate a resume watermark with the wrong dispatcher index by reading `len() - 1` after releasing the registration lock. | sanitized transcript location. **Atomic creation/registration of the actual subscriber identity was required.** |

**Disposition rationale**

A registration race was identified in an intermediate architecture, but no survival trace to current main was established.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l343"></a>
## `report-L343` - A Source set `source_position` but left wrapper `sequence` to a reset process-local counter, allowing query dedupe to discard new events after restart.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 343
- **Published disposition:** `resolved-existing-record`

**Original finding detail**

> | A Source set `source_position` but left wrapper `sequence` to a reset process-local counter, allowing query dedupe to discard new events after restart. | sanitized transcript location. **Source-event sequence defect, distinct from #775's query-output sequence restoration.** A WAL-derived/restored wrapper sequence was proposed; no separate final fix association was established here. |

**Disposition rationale**

The generic source sequence-reset problem was later filed and fixed upstream.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/827
- https://github.com/drasi-project/drasi-core/pull/833

<a id="report-l344"></a>
## `report-L344` - Permanent GraphQL authentication, permission, or invalid-document failures became endless transient hydration retries.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 344
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Permanent GraphQL authentication, permission, or invalid-document failures became endless transient hydration retries. | sanitized transcript location. **Misclassification could leave the Source Running and accepting work until full.** The later payload-only design eliminated this hydrator path. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l345"></a>
## `report-L345` - Project union queries used the same response field `state` for incompatible Issue/PR enum types.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 345
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Project union queries used the same response field `state` for incompatible Issue/PR enum types. | sanitized transcript location. **Additional GraphQL schema error**, distinct from the invalid Actor/Project-owner selections above. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l346"></a>
## `report-L346` - An object leaving scope was ignored without retracting its prior in-scope graph.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 346
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | An object leaving scope was ignored without retracting its prior in-scope graph. | sanitized transcript location. **Stale projection in the historical inventory model.** Later scope/cache removal changed the contract. |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l347"></a>
## `report-L347` - Cascade deletion iterated a randomized map, while sorted assertions concealed nondeterministic output order.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Additional intermediate Source/shared-runtime findings
- **Sanitized source-report line:** 347
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> | Cascade deletion iterated a randomized map, while sorted assertions concealed nondeterministic output order. | sanitized transcript location. **Historical ordering defect; the user-directed direct-delete design later removed cascade machinery.** |

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l353"></a>
## `report-L353` - Single active allocator: no distributed CAS/leader-election or active-active allocation guarantee in the simplified design.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Other deliberately bounded contracts
- **Sanitized source-report line:** 353
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> - **Single active allocator:** no distributed CAS/leader-election or active-active allocation guarantee in the simplified design.

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l354"></a>
## `report-L354` - Point-in-time lease validation: validation does not atomically cover a subsequent GitHub write.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Other deliberately bounded contracts
- **Sanitized source-report line:** 354
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> - **Point-in-time lease validation:** validation does not atomically cover a subsequent GitHub write.

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l355"></a>
## `report-L355` - Result actor attribution: the old Result schema did not independently identify a worker; trusted reporter plus exact lease possession was the accepted boundary.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Other deliberately bounded contracts
- **Sanitized source-report line:** 355
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> - **Result actor attribution:** the old Result schema did not independently identify a worker; trusted reporter plus exact lease possession was the accepted boundary.

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l356"></a>
## `report-L356` - Bootstrap/activation: no invented restoration of preexisting open Assignments; fresh-state activation and full Source-state/WAL reset requirements were explicit.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Other deliberately bounded contracts
- **Sanitized source-report line:** 356
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> - **Bootstrap/activation:** no invented restoration of preexisting open Assignments; fresh-state activation and full Source-state/WAL reset requirements were explicit.

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l357"></a>
## `report-L357` - Missed webhook reconciliation and atomic GitHub snapshots: payload-only ingress did not implement polling/catch-up, and successful pagination was not a transactional upstream snapshot.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Other deliberately bounded contracts
- **Sanitized source-report line:** 357
- **Published disposition:** `historical-workgraph-or-superseded`

**Original finding detail**

> - **Missed webhook reconciliation and atomic GitHub snapshots:** payload-only ingress did not implement polling/catch-up, and successful pagination was not a transactional upstream snapshot.

**Disposition rationale**

The component/protocol was historical, superseded, or removed from Core; available public fixes and unresolved limits are retained for provenance without asserting current applicability.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="report-l358"></a>
## `report-L358` - Historical publication dependency: automatic standalone plugin installation was gated on loader-compatible published Core/SDK dependencies, referenced as #574; pinned local plugin builds were used instead. This is a historical packaging constraint, not a newly asserted current engine defect.

- **Kind:** `finding-row`
- **Source section:** Earlier WorkGraph component defects in the Core repository
- **Source subheading:** Other deliberately bounded contracts
- **Sanitized source-report line:** 358
- **Published disposition:** `duplicate-mapped`

**Original finding detail**

> - **Historical publication dependency:** automatic standalone plugin installation was gated on loader-compatible published Core/SDK dependencies, referenced as [#574](https://github.com/drasi-project/drasi-core/issues/574); pinned local plugin builds were used instead. This is a historical packaging constraint, not a newly asserted current engine defect.

**Disposition rationale**

The packaging dependency was already tracked by #574 and is not a new engine defect.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/574

<a id="broad-aggregate-identity"></a>
## `broad-aggregate-identity` - broad aggregate identity

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 52
- **Published disposition:** `explicitly-deferred`

**Original finding detail**

> On August 27, the first aggregate investigation identified two bounded fixes for #792 and initially recommended deferring the broader zero-aggregate case. The very next parent/child lifecycle matrix required that case: parents disappeared instead of moving from Fork to JoinAll. It was therefore **not permanently deferred as a whole**. Subsequent work produced #793/#794, then the distinct #806-#809 repairs. However, the general accumulator empty-identity/contributor and Sum-encoding redesign **remained explicitly deferred**; authorizing bounded outer-group cardinality did not authorize a general `is_at_identity` implementation for every aggregate.

**Disposition rationale**

Bounded nested-group lifecycle work was implemented, but no general accumulator identity/Sum redesign was authorized.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/384
- https://github.com/drasi-project/drasi-core/issues/411
- https://github.com/drasi-project/drasi-core/issues/806

<a id="parked-replay-patch"></a>
## `parked-replay-patch` - parked replay patch

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 160
- **Published disposition:** `explicitly-parked`

**Original finding detail**

> The August 14 **reaction replay gap** was real and different from merely restarting a sequence counter:

**Disposition rationale**

The reaction replay patch was explicitly parked and must not be described as adopted.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/818
- https://github.com/drasi-project/drasi-core/issues/889

<a id="legacy-recovery-provenance"></a>
## `legacy-recovery-provenance` - legacy recovery provenance

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 171
- **Published disposition:** `explicitly-parked`

**Original finding detail**

> Follow-up `e26ba153` added narrowly bounded reconciliation for components with durable per-candidate provenance. `c3b5f54078065372f9b75b4560e3abfb8db354c8` documented the remaining legacy checkpoint-equals-clock/no-payload problem; **that documentation was not another fix**. Automatic exactly-once reconstruction was not possible when the needed historical execution evidence was absent. An offline audited recovery manifest was proposed, not implemented. Clearing a reaction checkpoint or replaying every current row into a generic HTTP/gRPC Reaction was explicitly insufficient/unsafe.

**Disposition rationale**

Later commits bounded recovery for durable provenance and documented an unsolved legacy case; documentation was not another fix.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/818
- https://github.com/drasi-project/drasi-core/issues/889

<a id="redb-shutdown-owner"></a>
## `redb-shutdown-owner` - redb shutdown owner

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 173
- **Published disposition:** `historical-investigation-unconfirmed`

**Original finding detail**

> An in-process Redb reopen also encountered a short-lived shutdown handle owner. The historical scenario switched to fresh processes; no separate fix or filed issue for that specific observation was recovered. Treat it as a bounded unresolved observation, not proof that every Redb shutdown path remains broken.

**Disposition rationale**

The scenario switched to fresh processes before root cause was isolated; no general Redb shutdown defect is asserted.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="excess-slot-retirement"></a>
## `excess-slot-retirement` - excess slot retirement

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 156
- **Published disposition:** `explicitly-deferred`

**Original finding detail**

> An excess-slot retirement projection discrepancy was also encountered repeatedly. It matched pre-existing behavior, and the user explicitly excluded changing it. **It remains a deferred contract discrepancy, not a newly demonstrated allocator capacity leak.**

**Disposition rationale**

The discrepancy matched pre-existing behavior and was explicitly excluded from implementation.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="withdrawn-diagnoses"></a>
## `withdrawn-diagnoses` - withdrawn diagnoses

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 213
- **Published disposition:** `withdrawn-or-unsupported`

**Original finding detail**

> Three historical assertions should **not** become additional unresolved Core tickets:

**Disposition rationale**

Route ingestion, projector ownership, and inline-predicate assertions were corrected or not reproducible and must not become Core tickets.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/889

<a id="derived-slot-fixture-collision"></a>
## `derived-slot-fixture-collision` - derived slot fixture collision

- **Kind:** `narrative-anchor`
- **Source section:** None
- **Source subheading:** N/A
- **Sanitized source-report line:** 219
- **Published disposition:** `withdrawn-or-unsupported`

**Original finding detail**

> The alleged failure to maintain a derived `(run, task-definition)` identity also needs correction: one major reproduction used **colliding node/relationship fixture IDs**. Correcting those IDs disproved that diagnosis. The real subsequent solver and nested-group failures were separately minimized as #806-#809.

**Disposition rationale**

The fixture reused node/relationship IDs; correcting it disproved that diagnosis, while the generic collision hazard remains #805.

**Public records**

- https://github.com/drasi-project/drasi-core/issues/805
- https://github.com/drasi-project/drasi-core/issues/889
