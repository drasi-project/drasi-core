# PR #485 — Review Triage & Response Plan

PR: https://github.com/drasi-project/drasi-core/pull/485 — *Unify http-adaptive into http reaction*

This file collects every review comment on PR #485, with a suggested **response** and a suggested
**action** for each. Review the list, then tell me which **item numbers** to act on (e.g. "apply
fixes for 1, 3, 6" or "post responses for all P3 docs items"). I can then:

- **Apply code/doc fixes** for selected items, and/or
- **Push the suggested responses** to the PR comment threads automatically (via `gh api`).

### Priority legend
- **P1** — Blocker / security / correctness; fix before merge.
- **P2** — Should-fix; functional gap or missing coverage.
- **P3** — Nit / docs / observability.
- **P4** — Discussion / out-of-scope / tracking-issue.

### Status legend
`pending` (not started) · `fix-approved` · `response-only` · `wont-do` · `done`

### How responses get posted
- **Inline (line-anchored) comments** → reply in-thread via
  `gh api repos/drasi-project/drasi-core/pulls/485/comments/<COMMENT_ID>/replies -f body=...`
- **Review-level comments** (not line-anchored) → posted as a PR issue comment via
  `gh pr comment 485 --body ...`

---

## Summary table

| # | Pri | File:Line | Topic | Suggested action | Reply IDs |
|---|-----|-----------|-------|------------------|-----------|
| 1 | P1 | http.rs:96 | `http2_prior_knowledge()` breaks HTTP/1.1 | Code fix: remove call | 3298900818, 3298900830, 3298944673 |
| 2 | P1 | process.rs:124 | SSRF via rendered absolute URL | Code fix: host allowlist | 3298918926 |
| 3 | P2 | adaptive_loop.rs:163 | `batch_endpoint` bypassed for single-result batches | Code fix: drop guard | 3298900836, 3298944683 |
| 4 | P2 | config.rs:89 | `batch_endpoint` silently ignored without `adaptive` | Code fix: validate/warn | 3298944676 |
| 5 | P2 | lib.rs:169 | builder size-setters implicitly enable adaptive | Code fix: doc + guard | 3298944679 |
| 6 | P2 | standard_loop.rs:80 | per-query routing drops `rsplit('.')` suffix match | Code fix: add fallback | 3298900891 |
| 7 | P2 | adaptive_loop.rs:192 | adaptive fan-out drops suffix match | Code fix: add fallback | 3298900966 |
| 8 | P2 | process.rs:66 | UPDATE/DELETE paths untested | Code fix: add tests | 3298963021 |
| 9 | P2 | integration_tests.rs:57/62 | `wait_for_requests` silent/flaky | Code fix: assert count | 3298900989, 3298963016 |
| 10 | P3 | adaptive_loop.rs:125 / standard_loop.rs:99 | template context lacks timestamp/sequence | Discuss/defer | 3298900913, 3298900937 |
| 11 | P3 | integration_tests.rs:88 | HTTP 5xx response untested | Code fix: add test | 3298963018 |
| 12 | P3 | adaptive_loop.rs:140 | silent shutdown drain timeout | Code fix: add `warn!` | 3298900847 |
| 13 | P3 | http.rs:104 | two `AdaptiveBatchConfig` types same name | Code fix: rename runtime | 3298944685 |
| 14 | P3 | process.rs:134 | sensitive graph data logged at DEBUG | Code fix: drop `context` | 3298918938 |
| 15 | P3 | http/README.md:32,101 | docs `add/update/delete/control` vs `added/updated/deleted` | Doc fix | 3298901021 |
| 16 | P3 | http/README.md:121 | docs context vars mismatch | Doc fix | 3298901047 |
| 17 | P3 | http/README.md:265 | Builder API example outdated | Doc fix | 3298901072 |
| 18 | P3 | http/README.md:73 | adaptive YAML snake_case vs camelCase | Doc fix | 3298901089 |
| 19 | P3 | http/README.md:89 | `baseUrl` required vs optional+default | Doc fix | 3298901112 |
| 20 | P3 | reactions/README.md:37 | "HTTP POST" → multiple methods | Doc fix (suggestion given) | 3298980481 |
| 21 | P3 | http/README.md:176 | `adaptive_window_size` unit missing | Doc fix (suggestion given) | 3298980491 |
| 22 | P3 | http/README.md:93 | `batchEndpoint` grammar | Doc fix (suggestion given) | 3298980496 |
| 23 | P3 | http/README.md:85 | `config_version` not in table | Doc fix (suggestion given) | 3298980497 |
| 24 | P4 | (design review) | asymmetry vs `grpc-adaptive` | Open tracking issue | 4357640895 |

---

## Items

### Item 1 — P1 — `http2_prior_knowledge()` breaks HTTP/1.1 endpoints
- **Status:** pending
- **Where:** `components/reactions/http/src/http.rs:96`
- **Reply IDs:** 3298900818 (drasi-reviewer), 3298900830 (Copilot), 3298944673 (drasi-reviewer) · review 4357579393
- **Comment (summary):** `build_client()` unconditionally calls `.http2_prior_knowledge()`, forcing
  cleartext h2c on every connection. HTTP/1.1 and TLS-HTTPS webhook targets reject the raw HTTP/2
  preface, silently breaking delivery. Not present in original code — a breaking change.
- **Assessment:** ✅ Valid. Confirmed at http.rs:92-98. reqwest negotiates HTTP/2 via ALPN over TLS
  automatically; the prior-knowledge call only helps for known h2c servers.
- **Suggested action (code fix):** Remove `.http2_prior_knowledge()` from `build_client()`. (Optional
  follow-up: expose as opt-in config flag — defer to a separate issue to keep this PR focused.)
- **Suggested response:**
  > Good catch — agreed this is a regression. `http2_prior_knowledge()` forces cleartext h2c and
  > breaks HTTP/1.1 and TLS targets that negotiate via ALPN. I've removed it so reqwest negotiates
  > HTTP/2 automatically for `https://`. If we want guaranteed h2c for known endpoints we can add an
  > opt-in `http2PriorKnowledge` flag in a follow-up.

### Item 2 — P1 — SSRF via Handlebars URL template rendering
- **Status:** pending
- **Where:** `components/reactions/http/src/process.rs:124`
- **Reply IDs:** 3298918926 (drasi-reviewer)
- **Comment (summary):** When a URL template renders to an absolute `http(s)://` URL it is used
  verbatim. A template referencing graph data (`url: "{{data.callback_url}}"`) lets an attacker who
  can write the source data redirect requests to cloud-metadata/internal endpoints.
- **Assessment:** ✅ Valid in principle. Confirmed at process.rs:124-129 — absolute rendered URLs are
  used with no validation. Severity depends on whether operators template URLs from data; still worth
  a guard.
- **Suggested action (code fix):** When a rendered URL is absolute, validate its host against the
  configured `base_url` host; on mismatch, log a warning and fall back to `/changes/{query_name}`.
  Apply the same guard in the adaptive fan-out path.
- **Suggested response:**
  > Agreed this is a real SSRF vector when URL templates reference graph data. I've added host
  > validation: absolute rendered URLs whose host doesn't match `base_url`'s host are rejected and
  > fall back to the safe `/changes/{queryId}` POST, with a warning logged. Same guard added to the
  > adaptive fan-out path.

### Item 3 — P2 — `batch_endpoint` bypassed for single-result batches
- **Status:** pending
- **Where:** `components/reactions/http/src/adaptive_loop.rs:163`
- **Reply IDs:** 3298900836, 3298944683 (drasi-reviewer) · review 4357579393
- **Comment (summary):** Guard `by_query.values().any(|v| v.len() > 1)` routes to `batch_endpoint`
  only when one query produced >1 result. Multi-query single-result batches (the common coalescing
  case) skip the batch endpoint and fan out per-result, defeating the configured `batch_endpoint`.
- **Assessment:** ✅ Valid. Confirmed at adaptive_loop.rs:160-174. When `batch_endpoint` is set, the
  operator's intent is that all coalesced batches go there.
- **Suggested action (code fix):** Remove the inner `if by_query.values().any(...)` guard so that
  whenever `batch_endpoint` is `Some`, the coalesced batch is always POSTed there. Update README #22
  text accordingly. (If single-item suppression is ever wanted, make it an explicit documented option.)
- **Suggested response:**
  > Agreed. When `batchEndpoint` is configured the intent is for every coalesced batch to land there.
  > I've removed the `len() > 1` guard so any non-empty coalesced batch is posted to the batch
  > endpoint, and updated the README description. A separate min-batch threshold can be added later as
  > an explicit option if needed.

### Item 4 — P2 — `batch_endpoint` silently ignored without `adaptive`
- **Status:** pending
- **Where:** `components/reactions/http/src/config.rs:89`
- **Reply IDs:** 3298944676 (drasi-reviewer)
- **Comment (summary):** `batch_endpoint` is a top-level field but has no effect unless `adaptive` is
  `Some`. Setting `batchEndpoint` without `adaptive` silently yields standard per-result delivery.
  Suggestion: move `batch_endpoint` inside `AdaptiveBatchConfig`.
- **Assessment:** ✅ Valid UX trap (confirmed config.rs:115-120). Moving the field is a clean fix but
  is a **schema-breaking change** to the descriptor DTO; a lighter fix is a startup validation warning.
- **Suggested action (code fix, lighter option):** In `http.rs` start path, if `batch_endpoint.is_some()
  && adaptive.is_none()`, log a `warn!` that `batchEndpoint` is ignored without `adaptive`. Document
  the dependency in the README. (Defer the structural move to a follow-up if we want to avoid a schema
  break in this PR.)
- **Suggested response:**
  > Good point. Structurally moving `batchEndpoint` inside `adaptive` is the cleanest model but it's a
  > schema-breaking change to the plugin descriptor. For this PR I've added a startup warning when
  > `batchEndpoint` is set without `adaptive`, and documented the dependency. I'm happy to do the
  > structural move in a follow-up if you'd prefer enforcing it in the type system.

### Item 5 — P2 — builder size-setters implicitly enable adaptive mode
- **Status:** pending
- **Where:** `components/reactions/http/src/lib.rs:169`
- **Reply IDs:** 3298944679 (drasi-reviewer)
- **Comment (summary):** `with_min/max_batch_size`, `with_window`, `with_batch_timeout` call
  `get_or_insert_with(AdaptiveBatchConfig::default)`, so a "tuning" call silently switches the runtime
  into adaptive mode. Hidden side-effect.
- **Assessment:** ✅ Valid API ergonomics concern. Two reasonable resolutions: (a) document the
  auto-enable explicitly, or (b) make setters require adaptive already enabled.
- **Suggested action (code fix):** Add doc comments to these builder methods stating they enable
  adaptive mode if not already enabled. (Keep behavior — requiring prior `.with_adaptive()` is more
  ergonomic to leave implicit, but must be documented.)
- **Suggested response:**
  > Agreed the implicit enable is surprising. I've documented on each setter that calling it enables
  > adaptive mode (creating a default `AdaptiveBatchConfig` if none is set). I kept the auto-enable for
  > ergonomics rather than returning `Result`, but the doc now makes the side-effect explicit. Open to
  > switching to a required-`with_adaptive` model if you feel strongly.

### Item 6 — P2 — per-query routing drops `rsplit('.')` suffix match (standard loop)
- **Status:** pending
- **Where:** `components/reactions/http/src/standard_loop.rs:80` (also `process.rs` lookups)
- **Reply IDs:** 3298900891 (Copilot)
- **Comment (summary):** `get_template_spec(query_name, op)` matches only the full `query_id`, dropping
  the suffix-matching used elsewhere for namespaced ids like `source.query`.
- **Assessment:** ✅ Valid. Confirmed the SSE reaction (sse.rs:320-332) does exact-then-`rsplit('.')`
  fallback; the shared `TemplateRouting::get_template_spec` (templates.rs:272) does **not**. HTTP loses
  that flexibility.
- **Suggested action (code fix):** Add suffix-fallback routing. Cleanest: add a default method on the
  `TemplateRouting` trait (e.g. `get_template_spec_with_fallback`) that tries the full id, then the
  last dotted segment, then default — and use it in both standard and adaptive loops (covers Item 7).
- **Suggested response:**
  > Agreed — the HTTP reaction should match the suffix-fallback behavior the SSE reaction uses for
  > namespaced `source.query` ids. I've added an exact-match-then-last-segment fallback (shared via the
  > `TemplateRouting` trait) and applied it in both the standard and adaptive routing paths.

### Item 7 — P2 — adaptive fan-out drops suffix match
- **Status:** pending
- **Where:** `components/reactions/http/src/adaptive_loop.rs:192`
- **Reply IDs:** 3298900966 (Copilot)
- **Comment (summary):** Per-result fan-out uses `get_template_spec(&query_id, op)` with the full id,
  so namespaced configs won't match. Same `rsplit('.')` fallback needed.
- **Assessment:** ✅ Valid; same root cause as Item 6 (adaptive_loop.rs:189).
- **Suggested action (code fix):** Apply the shared suffix-fallback routing from Item 6 here too.
- **Suggested response:**
  > Fixed together with the standard-loop routing — the adaptive fan-out now uses the same
  > exact-then-last-segment template lookup, so namespaced query ids resolve their per-query templates.

### Item 8 — P2 — UPDATE and DELETE code paths untested
- **Status:** pending
- **Where:** `components/reactions/http/src/process.rs:66` (`build_context`)
- **Reply IDs:** 3298963021 (drasi-reviewer)
- **Comment (summary):** All integration tests use `enqueue_add`. `build_context`'s UPDATE
  (`before`/`after` extraction) and DELETE branches, plus the non-object fallback, are untested.
- **Assessment:** ✅ Valid. Confirmed branches at process.rs:56-81 are only exercised for ADD.
- **Suggested action (code fix):** Add unit/integration tests: an `Update {before, after}` asserting
  `{{before.*}}`/`{{after.*}}`; a `Delete {data}` asserting `{{before.*}}`; an `Update` with a
  non-object `data` for the fallback branch.
- **Suggested response:**
  > Agreed — added coverage for the UPDATE (before/after extraction), DELETE (before populated), and
  > the non-object UPDATE fallback branches of `build_context`, asserting the rendered template
  > receives the expected fields.

### Item 9 — P2 — `wait_for_requests` returns silently / timing-flaky
- **Status:** pending
- **Where:** `components/reactions/http/tests/integration_tests.rs:57,62`
- **Reply IDs:** 3298900989 (Copilot), 3298963016 (drasi-reviewer)
- **Comment (summary):** `wait_for_requests` polls with `sleep(20ms)` and returns silently at the
  deadline; tests only check `!reqs.is_empty()`, so a zero-request timeout can false-pass, and later
  `reqs[0]` indexing can panic obscurely.
- **Assessment:** ✅ Valid test-robustness issue.
- **Suggested action (code fix):** Make `wait_for_requests` assert the expected count was reached
  (panic with observed count on timeout), or switch to wiremock `Mock::expect(n)` + `verify()`. Keep
  it minimal: assert count met before returning.
- **Suggested response:**
  > Agreed. I've made `wait_for_requests` fail with the observed-vs-expected count when the deadline
  > expires instead of returning silently, so partial/zero-request runs surface as clear test failures
  > rather than downstream index panics.

### Item 10 — P3 — template context lacks timestamp/sequence metadata
- **Status:** pending
- **Where:** `adaptive_loop.rs:125`, `standard_loop.rs:99`, `process.rs`
- **Reply IDs:** 3298900913, 3298900937 (Copilot)
- **Comment (summary):** `process_result()` only gets `data` + `query_name`; the batch channel sends
  only `(query_id, results)`. So templates/batch bodies can't reference `timestamp`/`sequence` even
  though shared docs mention them.
- **Assessment:** ⚠️ Partially valid. `build_context` currently exposes `after/before/data/query_name/
  operation` — no `timestamp`/`sequence`. Threading metadata through the batcher channel and
  `process_result` is a non-trivial change touching the channel payload type.
- **Suggested action:** Defer to a follow-up issue (scope: add `timestamp`/`sequence` to template
  context + batch payload). For *this* PR, fix the README (Item 16) so docs match the actual context.
- **Suggested response:**
  > Good observation. Exposing `timestamp`/`sequence` in the template context requires threading
  > `QueryResult` metadata through the batch channel and `process_result`, which I'd prefer to do in a
  > focused follow-up rather than expand this consolidation PR. For now I've aligned the README with the
  > context the reaction actually provides. I'll open a tracking issue for the metadata enhancement.

### Item 11 — P3 — HTTP 5xx response handling untested
- **Status:** pending
- **Where:** `components/reactions/http/tests/integration_tests.rs:88`
- **Reply IDs:** 3298963018 (drasi-reviewer)
- **Comment (summary):** No test verifies behavior when the downstream returns 4xx/5xx (should log and
  continue, not drop/stop).
- **Assessment:** ✅ Valid, reasonable coverage gap.
- **Suggested action (code fix):** Add a test mounting `ResponseTemplate::new(500)`, enqueue a result,
  assert the reaction keeps running and a subsequent 200 request still succeeds.
- **Suggested response:**
  > Added a test that mounts a 500 response, enqueues a result, and asserts the reaction keeps
  > processing (a follow-up success request is still delivered) rather than stopping on the error.

### Item 12 — P3 — silent shutdown drain timeout
- **Status:** pending
- **Where:** `components/reactions/http/src/adaptive_loop.rs:140` (code at :134)
- **Reply IDs:** 3298900847 (drasi-reviewer)
- **Comment (summary):** The 5s batcher drain timeout result is discarded; if it fires, in-flight
  batches are dropped with no log.
- **Assessment:** ✅ Valid. Confirmed `let _ = tokio::time::timeout(...)` at adaptive_loop.rs:134.
- **Suggested action (code fix):** Log a `warn!` when the timeout elapses.
- **Suggested response:**
  > Agreed — added a `warn!` when the 5s drain window elapses so operators know in-flight batches may
  > have been dropped during shutdown.

### Item 13 — P3 — two `AdaptiveBatchConfig` types share a name
- **Status:** pending
- **Where:** `components/reactions/http/src/http.rs:104` / `adaptive_batcher.rs`
- **Reply IDs:** 3298944685 (drasi-reviewer)
- **Comment (summary):** `config::AdaptiveBatchConfig` (serialization) and
  `adaptive_batcher::AdaptiveBatchConfig` (runtime, `Duration`) share a name; only an import alias
  (`RuntimeAdaptiveConfig`) disambiguates.
- **Assessment:** ✅ Valid maintainability nit. Confirmed the alias in http.rs.
- **Suggested action (code fix):** Rename the runtime struct in `adaptive_batcher.rs` to
  `AdaptiveBatcherConfig` (or `RuntimeBatchConfig`) and drop the alias.
- **Suggested response:**
  > Agreed. Renamed the runtime type in `adaptive_batcher.rs` to `AdaptiveBatcherConfig` and removed
  > the `RuntimeAdaptiveConfig` import alias so the two types are unambiguous without aliasing.

### Item 14 — P3 — sensitive graph data logged at DEBUG
- **Status:** pending
- **Where:** `components/reactions/http/src/process.rs:134`
- **Reply IDs:** 3298918938 (drasi-reviewer)
- **Comment (summary):** The body-render debug log prints `context` verbatim (built from `before`/
  `after`/`data`), risking PII/secret exposure to log sinks.
- **Assessment:** ✅ Valid. Confirmed process.rs:133-136 logs `context` with `{:?}`.
- **Suggested action (code fix):** Drop the `context` (and template body) from the debug log; keep
  query name + result type only, per the reviewer's suggestion.
- **Suggested response:**
  > Agreed — removed the verbatim `context` (and template body) from the DEBUG log to avoid leaking
  > graph-data fields into log sinks. The log now records only the query name and operation type.

### Item 15 — P3 (docs) — operation keys `add/update/delete/control` vs `added/updated/deleted`
- **Status:** pending
- **Where:** `components/reactions/http/README.md:32,101-104`
- **Reply IDs:** 3298901021 (Copilot)
- **Comment (summary):** Docs use `add`/`update`/`delete` + a `control` op, but the schema uses
  `added`/`updated`/`deleted` only (no `control`); configs won't deserialize.
- **Assessment:** ✅ Valid. `QueryConfig` fields are `added`/`updated`/`deleted` (templates.rs:298-300);
  there is no `control`.
- **Suggested action (doc fix):** Update README features list and the outputTemplates table to
  `added`/`updated`/`deleted`; remove `control`.
- **Suggested response:**
  > Thanks — corrected the README to use the actual operation keys `added`/`updated`/`deleted` and
  > removed the non-existent `control` operation.

### Item 16 — P3 (docs) — Handlebars context variables mismatch
- **Status:** pending
- **Where:** `components/reactions/http/README.md:121-128`
- **Reply IDs:** 3298901047 (Copilot)
- **Comment (summary):** Docs list `queryId`/`sequence`/`timestamp`/`data`, but the reaction provides
  `after`/`before`/`data` plus `query_name` and `operation`.
- **Assessment:** ✅ Valid. Confirmed `build_context` (process.rs:53-92). (See Item 10 for the
  implementation-side enhancement.)
- **Suggested action (doc fix):** Replace the context-variables table with the actual variables:
  `after`, `before`, `data`, `query_name`, `operation`.
- **Suggested response:**
  > Corrected the template-context table to match what the reaction actually populates: `after`,
  > `before`, `data`, `query_name`, and `operation`. (Adding `timestamp`/`sequence` is tracked
  > separately as an implementation enhancement.)

### Item 17 — P3 (docs) — Builder API example outdated
- **Status:** pending
- **Where:** `components/reactions/http/README.md:265`
- **Reply IDs:** 3298901072 (Copilot)
- **Comment (summary):** The Builder example doesn't match the current API (`with_token` takes a
  string, `with_default_template`/`with_query_template` take `HttpQueryConfig`, `with_adaptive`
  expects `AdaptiveBatchConfig`).
- **Assessment:** ✅ Likely valid; needs the example rewritten against the current builder signatures.
- **Suggested action (doc fix):** Rewrite the Builder API example to compile against the current
  `HttpReactionBuilder` API (verify signatures in `lib.rs` while editing).
- **Suggested response:**
  > Updated the Builder API example to match the current `HttpReactionBuilder` signatures
  > (`with_token`, `with_default_template`/`with_query_template` taking `HttpQueryConfig`, and
  > `with_adaptive` taking an `AdaptiveBatchConfig`).

### Item 18 — P3 (docs) — adaptive YAML snake_case vs camelCase
- **Status:** pending
- **Where:** `components/reactions/http/README.md:72-76`
- **Reply IDs:** 3298901089 (Copilot)
- **Comment (summary):** Quick-start `adaptive` block uses snake_case keys, but the dynamic-plugin DTO
  is camelCase.
- **Assessment:** ✅ Valid for the dynamic-plugin path. The descriptor DTO `AdaptiveBatchConfigDto`
  has `#[serde(rename_all = "camelCase")]` (descriptor.rs:83), so plugin YAML needs
  `adaptiveMinBatchSize`, `adaptiveMaxBatchSize`, `adaptiveWindowSize`, `adaptiveBatchTimeoutMs`.
  (Note: the embedded Rust `AdaptiveBatchConfig` is snake_case — but README YAML targets the plugin.)
- **Suggested action (doc fix):** Change the quick-start adaptive keys to camelCase.
- **Suggested response:**
  > Correct — the dynamic plugin descriptor DTO is camelCase, so the YAML example should use
  > `adaptiveMinBatchSize`/`adaptiveMaxBatchSize`/`adaptiveWindowSize`/`adaptiveBatchTimeoutMs`. Fixed
  > the quick-start example.

### Item 19 — P3 (docs) — `baseUrl` required vs optional
- **Status:** pending
- **Where:** `components/reactions/http/README.md:89`
- **Reply IDs:** 3298901112 (Copilot)
- **Comment (summary):** Table marks `baseUrl` "required", but the DTO makes it optional and the
  config default is `http://localhost`.
- **Assessment:** ✅ Valid. `default_base_url()` = `http://localhost` (config.rs:30-32), `#[serde(default
  = "default_base_url")]`.
- **Suggested action (doc fix):** Change the table to show `baseUrl` optional with default
  `http://localhost`.
- **Suggested response:**
  > Updated the config table: `baseUrl` is optional and defaults to `http://localhost` (not required).

### Item 20 — P3 (docs) — "HTTP POST" understates supported methods
- **Status:** pending
- **Where:** `components/reactions/README.md:37`
- **Reply IDs:** 3298980481 (drasi-reviewer) — *includes a suggested diff*
- **Comment (summary):** Calling it "HTTP POST" is misleading; the plugin supports GET/POST/PUT/PATCH/
  DELETE.
- **Assessment:** ✅ Valid. Methods supported per `HttpCallExt.method` docs (config.rs:48-51).
- **Suggested action (doc fix):** Apply the reviewer's suggested row:
  `| `drasi-reaction-http` | HTTP webhooks to external endpoints (with optional adaptive batching) | `http/` |`
- **Suggested response:**
  > Applied — the description now reads "HTTP webhooks to external endpoints (with optional adaptive
  > batching)" rather than implying POST-only.

### Item 21 — P3 (docs) — `adaptive_window_size` unit missing
- **Status:** pending
- **Where:** `components/reactions/http/README.md:176`
- **Reply IDs:** 3298980491 (drasi-reviewer) — *includes a suggested diff*
- **Comment (summary):** The window-size description omits the 100ms unit that the deleted
  http-adaptive README documented.
- **Assessment:** ✅ Valid. Unit confirmed in `AdaptiveBatchConfig` docs (common/config.rs:84-89).
- **Suggested action (doc fix):** Apply the reviewer's suggested row documenting "100 ms units
  (10 = 1 s, 50 = 5 s, 100 = 10 s)".
- **Suggested response:**
  > Applied the suggested wording documenting that `adaptiveWindowSize` is in 100 ms units
  > (10 = 1 s, 50 = 5 s, 100 = 10 s).

### Item 22 — P3 (docs) — `batchEndpoint` grammar/clarity
- **Status:** pending
- **Where:** `components/reactions/http/README.md:93`
- **Reply IDs:** 3298980496 (drasi-reviewer) — *includes a suggested diff*
- **Comment (summary):** "a batch with results from a query >1" is awkward/ambiguous.
- **Assessment:** ✅ Valid. Note: reword should also reflect the Item 3 behavior change (any coalesced
  batch, not only multi-result).
- **Suggested action (doc fix):** Reword; if Item 3 is applied, describe it as "When set, POST the
  entire coalesced batch to `{baseUrl}{batchEndpoint}` as a single payload."
- **Suggested response:**
  > Reworded for clarity. Since I also removed the single-result bypass (see the adaptive-loop thread),
  > the description now reads: "When set, POST the entire coalesced batch to `{baseUrl}{batchEndpoint}`
  > as a single payload."

### Item 23 — P3 (docs) — `config_version` mentioned but not in table
- **Status:** pending
- **Where:** `components/reactions/http/README.md:85`
- **Reply IDs:** 3298980497 (drasi-reviewer) — *includes a suggested diff*
- **Comment (summary):** `config_version` is referenced in prose but not in the config table.
- **Assessment:** ✅ Valid nit.
- **Suggested action (doc fix):** Apply the reviewer's suggestion (drop the inline reference, keeping
  "All top-level fields are camelCase.").
- **Suggested response:**
  > Applied — removed the stray `config_version` reference from the prose so it doesn't imply a
  > user-settable field.

### Item 24 — P4 — design asymmetry with `grpc-adaptive`
- **Status:** pending
- **Where:** Design review (not line-anchored)
- **Reply IDs:** 4357640895 (drasi-reviewer, review body) — reply via PR issue comment
- **Comment (summary):** This PR merges `http-adaptive` into `http` but leaves `grpc-adaptive`
  standalone, creating two patterns for the same concept. Suggests consolidating grpc too or opening a
  tracking issue.
- **Assessment:** ✅ Reasonable; out of scope for this PR.
- **Suggested action:** Open a tracking issue "Consolidate grpc-adaptive into grpc reaction" and
  reference it. No code change here.
- **Suggested response:**
  > Agreed the long-term goal is one unified reaction per transport. Consolidating `grpc-adaptive` is
  > out of scope for this PR, so I've opened a tracking issue to mirror this change for gRPC: <ISSUE_URL>.

---

## Batch shortcuts (suggested groupings)
- **Code — P1 (must fix):** 1, 2
- **Code — P2 behavior:** 3, 4, 5, 6, 7
- **Code — P2/P3 tests + observability:** 8, 9, 11, 12
- **Code — P3 cleanups:** 13, 14
- **Docs — all:** 15, 16, 17, 18, 19, 20, 21, 22, 23
- **Tracking issue + response only:** 10, 24
