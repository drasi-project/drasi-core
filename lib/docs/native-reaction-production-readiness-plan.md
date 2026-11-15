# Native reaction (sink) plugins: production-readiness plan

**Status:** Proposed plan, written 2026-11-14 against `6d294f43` ("Native lifecycle
and component management (ABI 4.0)"). **Progress (2026-11-15):** the shared
network sink runtime and the HTTP, gRPC, SSE and log sinks are implemented, with
X-01, X-02, X-05 and X-07 delivered as scaffolding. See the status columns in
[§3](#3-shared-network-sink-runtime-commonnetwork) to [§6](#6-sse-sink-drasi-computation-sink-sse)
and [§13](#13-log-drasi-computation-sink-log), and
[Implementation status](#implementation-status-2026-11-15). The stored-procedure,
bridge, capture and profiler sinks and the §15 ports have not been started.
**Scope:** The ten native ComputationGraph sink plugins under
`components/computation-plugins/sinks/`, the sink-side parts of the shared
crates they depend on (`computation-plugins/common/network`,
`computation-plugins/common/bridge`, `postgres-common`, `mysql-common`,
`mssql-common`, `http-common`, `grpc-common`, `reaction-common`), and every
legacy ordinary reaction under `components/reactions/`, including those that
have no native equivalent.
**Companion:** [`native-source-production-readiness-plan.md`](native-source-production-readiness-plan.md)
(the "source plan"). Where a source-plan item applies unchanged, this plan
cites it (for example "source X-03") and does not restate it.
**Ground rules:**

- This is a **breaking change** for native sink plugins. Configuration shapes,
  defaults, factory names, wire bodies, template contexts and capability
  declarations may change freely.
- There is **no migration of existing deployments** and **no backward
  compatibility**.
- **No functionality may be lost.** Every capability of the current native sink
  must survive, as must every capability of the legacy ordinary reaction
  (`components/reactions/*`). The only exceptions are those listed in
  [Decisions required](#2-decisions-required), which must be explicitly
  accepted or rejected. This includes the legacy reactions that have no native
  sink at all (§15).

Items from [`computation-graph-remaining-work.md`](computation-graph-remaining-work.md)
are referenced by CW ID. This plan assigns work to plugins; it does not replace
the CW entries' finish criteria. When work here closes a CW obligation, update
that entry.

Finding references use `path:line` at `6d294f43`. Paths under
`computation-plugins/` are relative to `components/` unless they start with
`lib/`. The headline findings in §0.4 were confirmed by reading the cited code
during this review. Items marked **(reproduce first)** are plausible from the
code but need a failing test before a fix is chosen.

---

## Contents

0. [Inventory and headline findings](#0-inventory-and-headline-findings)
1. [Cross-cutting workstream (all native sinks)](#1-cross-cutting-workstream-all-native-sinks)
2. [Decisions required](#2-decisions-required)
3. [Shared network sink runtime (`common/network`)](#3-shared-network-sink-runtime-commonnetwork)
4. [HTTP sink](#4-http-sink-drasi-computation-sink-http)
5. [gRPC sink](#5-grpc-sink-drasi-computation-sink-grpc)
6. [SSE sink](#6-sse-sink-drasi-computation-sink-sse)
7. [Stored procedures: shared work](#7-stored-procedures-shared-work)
8. [PostgreSQL stored procedures](#8-postgresql-stored-procedures-drasi-computation-sink-storedproc-postgres)
9. [MySQL stored procedures](#9-mysql-stored-procedures-drasi-computation-sink-storedproc-mysql)
10. [SQL Server stored procedures](#10-sql-server-stored-procedures-drasi-computation-sink-storedproc-mssql)
11. [Bridge sink](#11-bridge-sink-drasi-computation-sink-bridge)
12. [Capture](#12-capture-drasi-computation-sink-capture)
13. [Log](#13-log-drasi-computation-sink-log)
14. [Profiler](#14-profiler-drasi-computation-sink-profiler)
15. [Legacy reactions with no native sink](#15-legacy-reactions-with-no-native-sink)
16. [Sequencing](#16-sequencing)
17. [Definition of done (per plugin)](#17-definition-of-done-per-plugin)
18. [Implementation status (2026-11-15)](#implementation-status-2026-11-15)
19. [Appendix: remaining-work cross reference](#appendix-remaining-work-cross-reference)

---

## 0. Inventory and headline findings

### 0.1 Native sinks

| Plugin crate (`computation-plugins/sinks/…`) | Factory (implementation ID) | Code (plugin; shared engine) | Completion / delivery model | Legacy counterpart | Hosting |
|---|---|---|---|---|---|
| `http` | `drasi.network/http-sink` | 1.0k; `common/network`, `http-common` | `Handled`, plain `NativeSink`, in-`handle` retry; optional host batching. An unused `delivery::HttpDeliveryHandler` (DeliveryRunner + `DeliveryBatchIdentity`) exists but no factory uses it | `reactions/http` (7.1k, incl. adaptive mode) | Dynamic + Rust |
| `grpc` | `drasi.network/grpc-sink` | 0.74k; `common/network`, `grpc-common` | `Handled` after exact `items_processed` acknowledgement; plain `NativeSink` | `reactions/grpc` (8.3k, fixed and adaptive runners) | Dynamic + Rust |
| `sse` | `drasi.network/sse-sink` | 0.87k; `common/network` | `Accepted` (volatile broadcast); owns an axum server; output-snapshot reader | `reactions/sse` (3.4k) | Dynamic + Rust |
| `storedproc-postgres` | `drasi.postgres/storedproc` | 0.71k; `postgres-common::delivery`, `sql::template` | `Handled`, **`ConsumerMode::External`** consumer with a destination-side cursor table (per-operation exactly-once when procedures do not commit internally) | `reactions/storedproc-postgres` (5.1k) | Dynamic + Rust |
| `storedproc-mysql` | `drasi.mysql/storedproc` | 0.43k; `mysql-common::storedproc` (~0.6k) | `Handled`, plain `NativeSink`, no transaction; at-least-once with partial effects | `reactions/storedproc-mysql` (3.1k) | Dynamic + Rust |
| `storedproc-mssql` | `drasi.mssql/storedproc` | 0.67k (single `lib.rs`); `mssql-common::sql` | `Handled`, plain `NativeSink`, no transaction; at-least-once with partial effects | `reactions/storedproc-mssql` (2.5k) | Dynamic + Rust (live qualification deferred, CW-78) |
| `bridge` | `drasi.bridge/sink` | 0.4k; `common/bridge` (7.8k, shared with the source) | Volatile `Accepted` / durable `Handled`; private single-frame ledger, not DeliveryRunner | none (native-origin) | **Static only; depends on `drasi-lib`** |
| `capture` | `drasi.standard/capture` | 0.2k | `Handled` after write + flush | none (test/standard sink); `reactions/file` is the nearest | Dynamic + Rust |
| `log` | `drasi.standard/log` | 0.39k; `reaction-common` templates | `Handled` after `log::info!` | `reactions/log` (2.2k) | Dynamic + Rust |
| `profiler` | `drasi.standard/profiler` | 0.37k | `Handled`; owns a periodic report task | `reactions/profiler` (1.9k) | Dynamic + Rust |

### 0.2 Legacy reactions with no native sink

All are ordinary `ReactionBase` reactions that depend on `drasi-lib`. Details
and per-reaction tasks are in §15.

| Legacy reaction | LOC (`src`) | Published plugin | Container tests | Nearest native | Recommendation (D-7) |
|---|---|---|---|---|---|
| `application` | 3.4k | no (in-process) | – | **`NativeApplicationReaction` already exists** (`reactions/application/src/native.rs`) | Migrate consumers, retire legacy API |
| `aws-sqs` | 1.6k | yes | ElasticMQ | none | Port |
| `rabbitmq` | 1.6k | yes | RabbitMQ | none | Port |
| `loki` | 1.6k | yes | Loki | http (partial) | Port (thin) |
| `eventgrid` | 2.7k | yes | mock only | http (insufficient) | Port |
| `azure-storage` | 2.0k | yes | Azurite | none | Port |
| `a2a` | 3.2k | yes | mock server | none | Port |
| `dashboard` | 4.5k + JS | yes | mock source | sse (transport only) | Port |
| `file` | 2.0k | yes | tempdir | capture (path + append) | Extend capture into a file sink |
| `mcp` | 3.2k | yes | local | none | Port |
| `platform` | 3.2k | **no** (feature undefined) | ignored (Redis) | none | Decide with Platform roadmap |
| `snapshot-test` | 0.35k | test cdylib | – | SDK fixtures | Retire with the ordinary ABI (CW-63) |

### 0.3 State of the ABI 4 port (all sinks)

| Concern | http | grpc | sse | sp-pg | sp-mysql | sp-mssql | bridge | capture | log | profiler |
|---|---|---|---|---|---|---|---|---|---|---|
| Owns independent workers | no | no | **server** | conn. driver | no | per-input task | no | no | no | **report task** |
| `pause` real or `unsupported` | ✓ host-driven (D-6); retry wait is drop-safe | ✓ host-driven (D-6) | ✓ gate (503, keep-alives only) | no-op | no-op | no-op | passive macro | no-op (OK) | ✓ host-driven (D-6) | ✗ **no-op, reports continue** (CW-96) |
| `stop` honours `StopMode` + ctx deadline | ✓ (Drain bounded by `retryBudgetMs`) | ✓ (Drain bounded by `retryBudgetMs`) | ✓ deadline, Drain/Abort | ✗ config timeout | ✗ config timeout | ✗ config timeout, cannot abort | ✗ | ✗ | n/a | ✗ |
| `start` honours ctx cancellation/deadline | – | ✓ lazy; `requireConnectedAtStart` within deadline | ✓ | ✗ | ✗ | ✗ | ✗ | – | – | – |
| `dispose` / `StateIntent::Erase` | ✓ declared no-op | ✓ declared no-op | ✓ declared no-op | ✗ **leaks cursor rows** | n/a* | n/a* | ✗ refuses; ledger orphaned | n/a (file kept) | ✓ declared no-op | n/a* |
| `NativeManagement` / `Factory::assess` | ✓ | ✓ | ✓ (outputs Recreate; CW-94) | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | ✗ |
| `configuration_details` | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ | ✓ | ✓ typed `routes` | ✓ |
| Secrets modelled per field | ✓ `token`, `secretHeaders` | ✓ `secretMetadata` | n/a | ✗ whole `settings` secret | ✗ whole `settings` secret | ✗ whole `settings` secret | resource | n/a | n/a | n/a |
| Activation credentials | ✗ | ✗ | n/a | ✗ (legacy had identity providers) | ✓ | ✓ (SQL auth only) | n/a | n/a | n/a | n/a |
| Delivery via DeliveryRunner | ✓ opt-in `drasi.network/http-delivery` | ✗ (needs a `grpc-common` protocol field) | n/a (`Accepted`) | ✓ External | ✗ | ✗ | ✗ own ledger | ✗ | ✗ | ✗ |
| Retry / backoff | ✓ shared policy: jitter, `Retry-After`, budget, status classes | ✓ shared policy; `tonic::Code` classes, pushback | n/a | host runner, rollback SQLSTATEs only | connect only | **none** | fixed delay, no backoff | n/a | n/a | n/a |
| Recovers after a failed `handle` without Stop/Start | yes | yes | yes | ✗ wedged | ✗ wedged | ✗ wedged | yes | yes | ✓ render failure falls back (or strict, opt-in) | ✗ non-query input fails |
| Structured logging / metrics | `log` facade, sanitised; no metrics | same | same | `log` | `log` | none | **none** | none | is the log | `log` reports |
| DESIGN + CONFIGURATION docs | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | ✗ |
| README ABI text current | ✓ | ✓ | ✓ | ✗ says 2.0.0 | no mention | ✗ says 2.0.0 / wire 2 | ✗ says wire 1 | ✓ (STANDARD.md) | ✓ | ✓ (STANDARD.md) |

\* The component owns no external state. Erase removal still fails in the host
because `erases_owned_state` is undeclared; see source D-1 and [D-1](#2-decisions-required).

Columns for HTTP, gRPC, SSE and log show the state on 2026-11-15; the other
columns are unchanged from `6d294f43`.

### 0.4 Highest-risk findings (production blockers)

1. **SQL Server stored-procedure templates allow SQL injection.**
   `mssql-common/src/sql/template.rs:245-251` validates templates by compiling
   them only. The `json` helper writes unescaped JSON into the SQL text
   (`template.rs:145-180`, documented as unsafe at `:136-143`), and triple-stash
   `{{{x}}}` is raw. A template such as `EXEC p '{{json after.name}}'` is
   accepted, so row data reaches the SQL text. PostgreSQL
   (`postgres-common/src/sql/template.rs:211`, `ensure_param_only`) and MySQL
   (`mysql-common/src/storedproc.rs:111`) reject such templates. Add the
   failing unit test first, then the fix (SMS-01).
2. **Every stored-procedure sink wedges after one failure.** Each sets an
   `interrupted` flag before work and clears it only on success
   (PG `storedproc-postgres/src/component.rs:294-303`, MySQL
   `mysql-common/src/storedproc/runtime.rs:306-309`, SQL Server
   `storedproc-mssql/src/lib.rs:383-397`). After a timeout, a non-retryable
   error or a dropped connection, every later `handle` fails until the host
   stops and restarts the component. PostgreSQL also never reconnects after its
   driver dies.
3. **MySQL and SQL Server sinks fail permanently on large inputs (CW-44
   "large sink-batch").** MySQL rejects an input with more than 4,096 changes
   or more than `max_protocol_bytes` in total (`runtime.rs:95-98,124-131`). SQL
   Server rejects more than 100,000 commands (`storedproc-mssql/src/lib.rs:375-378`)
   and applies one `command_timeout_ms` to the whole input (`lib.rs:394-396`).
   Each such input is a poison input that is retried forever.
4. **MySQL and SQL Server have no delivery progress.** They run commands with no
   transaction and no cursor, and SQL Server has no retry at all. A failure part
   way through an input leaves partial effects, and the host's replay repeats
   the completed commands. PostgreSQL already has the right design
   (`postgres-common/src/delivery.rs`). Legacy retried each command with backoff
   (for example `reactions/storedproc-mssql/src/executor.rs:172-195`).
5. **PostgreSQL's delivery table is shared, capped and never cleaned.**
   `public.drasi_delivery_stream_v1` is hard-coded (`postgres-common/src/delivery.rs:12-20`)
   with a database-wide cap of `MAX_STREAMS = 4096` (`:11`, enforced at
   `:151-158`). `dispose` never deletes the sink's rows (and Erase is
   undeclared), so repeated create/remove eventually exhausts the cap for every
   consumer on that database. On PostgreSQL 15+ a least-privilege user may not
   be able to create the table in `public` (reproduce first).
6. **Two sinks report a silent successful Pause while their workers run**
   (CW-96). *SSE resolved 2026-11-15 (SS-01); profiler open.* SSE keeps accepting connections, heartbeats and snapshot reads
   (`sse/src/component.rs:456-458`), and the profiler keeps its report task
   (`profiler/src/component.rs:195-197`). This violates
   `computation-plugin-sdk/src/component.rs:85-88` and `lib/AGENTS.md`.
7. **No sink honours `StopMode` or the lifecycle deadline, and in-flight
   retries ignore Stop.** *Resolved for HTTP, gRPC and SSE on 2026-11-15
   (NS-01, HS-01, GS-01, SS-02); the host's drop of an in-flight `handle` on
   Abort or a deadline is relied on and tested through the retry budget. Open
   for the other sinks and for the SDK `ActivationScope::stop`.* HTTP retries inside `handle` can take about
   4 × 60 s plus backoff (`http/src/component.rs:99-123`). gRPC `start` runs up
   to 10 connection attempts of 15 s each with no cancellation check
   (`grpc/src/component.rs:323-345`). The shared `backoff`
   (`common/network/src/config.rs:174-179`) has no cancellation hook. SSE stop
   uses a fixed 5 s (`sse/src/component.rs:585`). The SDK's own
   `ActivationScope::stop` joins workers with no deadline
   (`computation-plugin-sdk/src/lifecycle.rs:273-292`). Whether the host drops
   an in-flight `handle` future on `Stop(Abort)` is unconfirmed
   (reproduce first).
8. ~~**Network sinks have no idempotency key and treat queued 202 as
   completion.**~~ *Resolved 2026-11-15 (HS-02, HS-06, GS-08 key part).* HTTP marks any 2xx `Handled` (`http/src/component.rs:107`;
   documented at `http/README.md:33`). The DeliveryRunner path that requires an
   exact 200 plus `DeliveryBatchIdentity` (`http/src/delivery.rs:122-141`)
   exists but no factory exposes it. No request carries a stable delivery
   identity for the receiver to deduplicate retries or replays.
9. **Sink credentials are plain configuration.** *Resolved for HTTP and gRPC
   on 2026-11-15 (HS-04, GS-05); open for the stored-procedure sinks and the
   SDK `ConfigOption::secret()`.* HTTP `token` and `headers`
   and gRPC `metadata` are not marked secret, and legacy accepted
   `ConfigValue` secret/env references for them. The SDK `ConfigOption` builder
   has no `secret()`; MySQL sets the flag by hand
   (`storedproc-mysql/src/lib.rs:73-77`). The stored-procedure sinks mark the
   whole `settings` object secret, including the templates. PostgreSQL has no
   activation credentials, so the legacy identity-provider support
   (`reactions/storedproc-postgres/src/executor.rs:51-72`) is lost.
10. ~~**SSE exposes an unauthenticated server on `0.0.0.0:8080` with CORS `*`**
    and no subscriber limit~~ *Resolved 2026-11-15 (SS-03, SS-04).* (`sse/src/component.rs:43-46,358-367`).
11. ~~**Log sink regressions.**~~ *Resolved 2026-11-15 (LG-01, LG-02).* It omits the legacy `json` helper
    (`log/src/component.rs:126` uses a bare `Handlebars::new()`; legacy
    registered it at `reactions/log/src/log.rs:458-475`). A template that uses
    `{{json after}}` compiles (`component.rs:99`) but fails at render, and the
    error fails `handle`, so the input is retried forever (reproduce first).
    Legacy fell back to logging the raw row.
12. **Profiler rejects non-query input.** Its port declares the generic
    graph-change schema, but `handle` calls `QueryChangeCodec::metadata(...)?`
    (`profiler/src/component.rs:249`), which fails for source-produced
    envelopes. This contradicts `computation-plugins/SINKS.md:3-8`
    (reproduce first).
13. **The bridge sink breaks the native dependency rule** (as source finding
    10). It also has a retry budget that can hold one `handle` for roughly
    80 minutes (16 attempts × several 60 s deadlines, computed from
    `common/bridge/src/config.rs:195-196,211-213`; reproduce first). It treats
    local retry settings as part of the wire contract, so editing
    `retry_attempts` invalidates the durable sender ledger
    (`config.rs:372`, `state.rs:172-177`).
14. **Eleven legacy reactions have no native sink** (§0.2). CW-44's finish
    criterion ("every required … reaction/sink exists") cannot be met until D-7
    decides each one.

---

## 1. Cross-cutting workstream (all native sinks)

Do these first. Doing them inside each plugin separately would produce ten
divergent implementations. IDs are `X-nn` within this plan; source-plan items
are cited as "source X-nn".

### X-01: Native sink lifecycle conformance standard (P0)

**Status (2026-11-15): done.** `components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md`
holds the standard, delivery profiles, retry and failure rules and the doc
template. The SDK `ActivationScope::stop` follow-up below is still open.

Add a sink section to the source plan's conformance standard (source X-01), or
a `components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md`. It replaces
`components/reactions/README.md` §14 for native sinks (the legacy checklist
items that still apply are folded in below).

| Hook | Required sink behaviour |
|---|---|
| `initialize` | Compile every template and validator, check secret and credential presence, and validate route/alias resolution. No connections, listeners or workers. |
| `start` | Observe `ctx.cancellation()` and `ctx.deadline()` across every connect, retry and readiness wait. A connection loop must not outlive the start budget. Partial start leaves a retry-safe cleanup guard. |
| `pause` | **Host-driven sinks** (no worker of their own) may succeed only if the host guarantees it calls no `handle` while paused and the in-flight `handle` has settled (confirm under D-6). They must also make an in-flight retry or backoff observe pause. **Sinks with workers** (SSE server, profiler reporter, PostgreSQL driver keepalives) must gate them with `ActivationScope`/`PauseGate` or return `lifecycle::unsupported(...)`. |
| `resume` | Reopen exactly what pause closed. |
| `stop` | `Drain`: finish the in-flight request, command or frame within `ctx.deadline()`, then close connections. `Abort`: cancel promptly, including backoff sleeps and connection attempts. On timeout retain ownership (`join_owned_worker_gracefully` for servers). A stop must not report an earlier delivery failure as a stop failure. |
| `dispose` | Implement both intents. Under `Erase`, remove exactly the destination-side state the sink owns (cursor rows, sender ledgers), idempotently; never shared tables or user data. Declare `erases_owned_state` iff Erase does something (D-1). |
| `handle` / `handle_batch` | Return `Ok` only when the declared completion is actually met (see X-03). A failure must leave the sink able to retry the same input without a Stop/Start cycle; never latch an "interrupted" flag that refuses all later input. |

SDK follow-up: give `ActivationScope::stop` a deadline-aware variant
(`computation-plugin-sdk/src/lifecycle.rs:273-292`) that retains unjoined
workers on timeout, in line with `context::workers`.

### X-02: Sink lifecycle conformance test kit (P0)

**Status (2026-11-15): linked kit done.** `computation-plugin-sdk::testing::SinkConformance`
(with a `SinkProbe` destination fixture) covers lifecycle, retry after a failure
without Stop/Start, Erase, configuration round-trip, the assess matrix and live
apply; HTTP, gRPC, SSE and log use it. Running it through the dynamic library
on both Tokio flavours, and the consumer crash case, remain open.

Extend the source plan's kit (source X-02) with a sink driver. It runs through
the linked `plugin()` definition and the freshly built dynamic library, on both
Tokio flavours (CW-41, CW-37). Each plugin supplies only a destination fixture
(mock HTTP/gRPC server, database container) and its assess table.

- start → stop → start on the same object; stop without start; stop after a
  failed start; double start refused;
- `handle` while paused is not called, or is refused, per D-6; pause during an
  in-flight `handle` with an active retry; resume restores delivery;
- `StopMode::Abort` vs `Drain` with a stalled destination; stop timeout
  followed by a successful retry;
- a failed `handle` (destination down, then up) followed by a successful retry
  of the same input **without** Stop/Start;
- dispose for each `DisposeReason`; `Erase` twice (CW-98);
- `configuration()` round-trip; `Factory::assess` matrix; stale or forged plan
  refused;
- for consumers: crash between the external effect and progress commit, then
  verify the declared duplicate behaviour (CW-14).

### X-03: Delivery semantics profile (P0)

Each sink must declare and document exactly one delivery profile, test it, and
never claim a stronger one (CW-44: "event-only sinks must not advertise
replacement or completion guarantees they do not implement").

| Profile | Meaning | Mechanism |
|---|---|---|
| `Accepted` | Accepted into a local volatile buffer | `SinkCompletion::Accepted` (SSE, volatile bridge) |
| `Handled` | The destination completed the effect; a crash before the host acknowledges may repeat it; a multi-change input may repeat completed changes | plain `NativeSink` (today: HTTP, gRPC, MySQL, SQL Server, log, capture, profiler) |
| `Consumer/External` | Host DeliveryRunner records input identity first and advances only the completed operation prefix; only the operation in flight can repeat; `DeliveryItem::id` is offered to the destination for deduplication | `ConsumerMode::External` (today: PostgreSQL) |
| `Consumer/External + destination cursor` | As above, plus a destination-side cursor committed in the same destination transaction, giving per-operation exactly-once when the procedure does not commit independently | PostgreSQL `delivery.rs` |
| `Consumer/Transactional` | Business state and completion commit in the host transaction | not used by any sink today |

Work: document the profile in each DESIGN.md; add the `DeliveryRunner`-based
profile to every sink whose destination can confirm completion (D-3, D-4); keep
`Handled` sinks honest (never `Ok` on a queued response unless the profile says
so, D-4).

### X-04: DeliveryRunner adoption and consumer progress (P0 for databases, P1 for network)

- Follow `lib/AGENTS.md`: record exact input identity before effects, advance
  only the completed prefix, keep keys free of transport/retry identity,
  never serialize the full envelope once per operation, use
  `DeliveryBatchIdentity` for any remote confirmation.
- Implement `retryable(&anyhow::Error)` explicitly per family
  (`computation-plugin-sdk/src/consumer.rs:59-61` defaults to `false`).
- Expose `consumer_progress_reader` where the factory keeps progress, so
  operators can see the delivered position (CW-10).
- The SDK requires consumers to be `Handled`, non-snapshot sinks
  (`consumer.rs:28-42`); SSE and volatile bridge are therefore out of scope.

### X-05: Retry, backoff and failure classification (P0)

**Status (2026-11-15): done for the network sinks.** `common/network::retry`
(feature `sink`) provides full-jitter exponential backoff (100 ms to 5 s),
`Retry-After` parsing, and a `retryBudgetMs` total budget (default 120 s, at
most 1 h). Waits are drop-safe, so a host cancellation ends them. HTTP
classifies by status code and gRPC by `tonic::Code` (with
`grpc-retry-pushback-ms`; `RetryInfo` details are not parsed). Skips are logged
with sanitised fields but not counted (X-11). The database sinks and the bridge
have not adopted it.

One shared helper (in `common/network` or a small shared crate, reused by the
database sinks and the bridge, and aligned with source X-06):

- bounded exponential backoff with full jitter; honour `Retry-After`
  (HTTP 429/503) and gRPC `RetryInfo`;
- a time budget, not only an attempt count, and cancellation-aware sleeps that
  observe stop, pause and the lifecycle deadline;
- classification by status code, SQLSTATE or `tonic::Code`, never by error
  text (`grpc-common` classifies by string today, `send.rs:309-333`);
- permanent errors (authentication, invalid argument, constraint violation) are
  not retried; outcome-unknown errors are surfaced to the runner as uncertain,
  never silently retried as a new effect;
- skips under `failurePolicy: skip` are counted and reported (today only
  `eprintln!`, `common/network/src/config.rs:97`).

Typed errors follow source X-05.

### X-06: Batching and large inputs (P0)

- No sink may reject an input purely for its size when it can be split.
  Replace whole-input caps with per-operation or sub-batch chunking (MySQL,
  SQL Server, HTTP/gRPC request splitting already exists).
- Timeouts apply per command or request, never to a whole input.
- Host batching (`NativeSink::batching`/`handle_batch`) stays the only batching
  mechanism. Document `SinkBatching` limits; remove the silent clamp of
  `max_inputs` to 1024 (`common/network/src/batching.rs:39`) or publish it.
- A consumer batch must be bounded in memory before rendering
  (SQL Server renders the whole input today, `storedproc-mssql/src/lib.rs:371`).

### X-07: Idempotency keys (P1)

**Status (2026-11-15): done for HTTP and gRPC.** Every request carries
`x-drasi-delivery-id: {component}/{port}/{sequence}.{ordinal}[-{sequence}.{ordinal}]`
(`common/network::config::delivery_id`), a range for a batch request. The
header name is reserved.

Every remote sink sends a stable, content-bound delivery identity that the
destination can deduplicate on: an HTTP header (for example
`x-drasi-delivery-id` from `DeliveryItem::id` or producer sequence + ordinal),
a gRPC metadata entry, SQS `MessageDeduplicationId`, AMQP `message_id`, Event
Grid event ID, Loki timestamp. The key must exclude retry transport identity and
replacement configuration (`lib/AGENTS.md`, DeliveryRunner bullet).

### X-08: Output templates (P0)

**Status (2026-11-15): partly done.** HTTP, gRPC, SSE and log compile templates
once at construction, register `json`, reject unknown helpers at construction
and follow D-5 (as taken in §2). Log route matching is consistent (LG-02) and
its `routes`/`defaultTemplate` schemas are typed. **Open:** the single shared
renderer in `reaction-common` (gRPC keeps a strict renderer of its own; SSE and
log carry local helper registration), the SQL validators, and the
legacy-to-native context mapping beyond the four sinks' CONFIGURATION.md files.

- One shared renderer and validator in `reaction-common`, used by HTTP, gRPC,
  SSE, log, the stored-procedure sinks and new ports. Today there are three
  SQL validators (PG `ensure_param_only`, MySQL
  `validate_parameterized_template`, SQL Server compile-only) and the `json`
  helper is duplicated in about seven crates
  (`postgres-common/src/sql/template.rs:162`, `grpc-common/src/output/templates.rs:193`,
  `reactions/loki/src/loki.rs:159`, …).
- Register the same helper set everywhere (`json` at minimum). Compile every
  template once at construction; validate helper names at construction.
- Route resolution is identical in validation and lookup (log accepts a
  dotted-suffix route at `log/src/component.rs:80` that
  `reaction-common/src/templates.rs:297` never matches; reproduce first).
- Render-failure policy is explicit (D-5). It must never wedge a pipe silently.
- Publish a legacy-to-native template context mapping: legacy `query_id`,
  `query_name`, `data` versus the native truthful-change context
  (`SINKS.md:79-113`), and the `PATCH` semantics where `after` holds only the
  changed properties (verify whether query output emits `REPLACE` or `PATCH`
  for row updates; reproduce first).
- Publish typed JSON schemas for `routes`, `defaultTemplate` and
  `outputTemplates` (log publishes opaque `Object`s, `log/src/component.rs:30-41`).

### X-09: Configuration, secrets and credentials (P0)

**Status (2026-11-15): HTTP and gRPC done.** HTTP `token` and a new
`secretHeaders` object, and gRPC `secretMetadata`, are top-level secret fields;
client TLS keys are secret. It is unverified that the host resolves a secret
reference to a whole JSON object (see [Implementation status](#implementation-status-2026-11-15)).
`ConfigOption::secret()` in the SDK and the stored-procedure items are open.

As source X-03 (flatten secrets to top-level fields, D-2), plus:

- Add `ConfigOption::secret()` to the SDK so factories stop hand-setting the
  flag.
- Mark HTTP `token`, header values and gRPC metadata values secret, or move them
  to a separate `secretHeaders`/`secretMetadata` map of secret references.
- Stored-procedure sinks: `password` and TLS private keys are top-level secrets;
  templates and routes become inspectable.
- `configuration_details` for the three stored-procedure sinks and the bridge.
- Activation credentials for PostgreSQL (MySQL and SQL Server already have
  them).

### X-10: Update planning and management (P1)

**Status (2026-11-15):** HTTP, gRPC, SSE and log implement `Factory::assess`
and `NativeManagement` with every field classified; each CONFIGURATION.md has
the matrix. Other sinks open.

As source X-04. Sink-specific guidance:

- **InPlace (Pause):** templates, routes, headers/metadata, timeouts, retry
  policy, failure policy, log level, profiler interval: anything read per
  delivery. Requires a real pause per X-01.
- **Restart (Stop):** endpoint/host/port, TLS material, credentials, listener
  address (SSE), connection settings.
- **Recreate:** output aliases and bindings (CW-94), delivery profile changes,
  destination cursor identity (PostgreSQL table or schema), bridge wire
  contract.

### X-11: Observability (P1)

As source X-07, plus sink metrics: deliveries, items, bytes, retries, skips,
failures by class, delivery latency, in-flight age, destination
acknowledgement lag and (consumers) completed-prefix position. Port the legacy
log-field sanitiser (`http-common/src/reaction/process.rs:38-50`,
`MAX_LOG_FIELD_LEN`) and never log secrets or full bodies.

### X-12: Server-style sinks: exposure and draining (P0 for SSE)

**Status (2026-11-15): done for SSE.** The listener moved from the HTTP source
to `common/network::server` (feature `listener`) and SSE uses it with
`common/network::security`. Dashboard and MCP will reuse it when ported.

SSE today, and dashboard/MCP after porting, run HTTP servers. Share with source
NET-03/NET-04: default bind `127.0.0.1`; non-loopback requires authentication
or an explicit opt-in; configurable CORS origins; a subscriber/session cap and
per-client admission; optional TLS; `join_owned_worker_gracefully` with the
lifecycle deadline; a real pause gate.

### X-13: Hygiene (P2)

As source X-08. Additionally replace `unreachable!`/`expect` on FFI-reachable
paths in sinks (`grpc/src/component.rs:200,270,366`;
`sse/src/component.rs:349,636`; `storedproc-mssql/src/lib.rs:276`; the bridge
list in BS-20).

### X-14: Documentation (P0 for release)

**Status (2026-11-15):** DESIGN.md and CONFIGURATION.md exist for HTTP, gRPC,
SSE and log. `computation-plugins/README.md`, `NETWORK.md`, `SINKS.md` and
`STANDARD.md` carry ABI 4.0.0 text and list log and profiler. The
stored-procedure READMEs and `common/bridge/README.md` are still stale.

As source X-09, with a sink template. Every plugin gets:

- **`DESIGN.md`**: purpose; architecture (connections, workers, servers);
  input decoding and change projection; delivery profile (X-03) with the exact
  duplicate and loss windows; batching; retry and classification; idempotency
  key; owned vs borrowed destination state; lifecycle semantics per hook;
  security model; known limitations; legacy differences.
- **`CONFIGURATION.md`**: every field (type, default, range, required, secret,
  update boundary); an example per common topology; template reference
  (context keys, helpers, routing, render-failure policy); destination
  prerequisites (grants, procedures, receiver protocol); update/assess matrix;
  removal and Erase behaviour; operational runbook; legacy configuration
  mapping.
- Fix stale version text: `computation-plugins/README.md:109,128`,
  `NETWORK.md:6-11,231,438`, `STANDARD.md:4,116`,
  `storedproc-postgres/README.md:5`, `storedproc-mssql/README.md:7-8`,
  `common/bridge/README.md:24-27`. Add log and profiler to
  `computation-plugins/README.md:30-45` and `SINKS.md:126-133`.

### X-15: Test infrastructure (P1)

As source X-10, plus: add log and profiler to the host-sdk `plugin_package`
mapping (`host-sdk/tests/native_computation_pipeline.rs:66-75`); run the
PostgreSQL storedproc live suite (`sources/postgres/tests/native_transactions/libraries.rs:516-749`)
and the MySQL suite (`host-sdk/tests/native_mysql_components.rs`) in named CI
jobs (CW-85); obtain the SQL Server environment (CW-78); port the useful
assertions from `components/reactions/tests/{lifecycle_support,recovery_support}.rs`
and the legacy per-reaction suites into native tests (CW-41, CW-77).

---

## 2. Decisions required

| ID | Decision | Options | Recommendation | Blocks |
|---|---|---|---|---|
| D-1 | Erase for sinks that own nothing (all except PG storedproc and durable bridge). | As source D-1. | As source D-1 (host accepts Erase as a no-op for "owns nothing"). | X-01, all dispose tasks |
| D-2 | Secret modelling. | As source D-2. | Flatten; add `ConfigOption::secret()`. | X-09 |
| D-3 | **Delivery profile for MySQL and SQL Server stored procedures.** | (a) Keep `Handled` at-least-once and document partial effects; (b) `ConsumerMode::External` with the host DeliveryRunner only; (c) (b) plus a destination cursor table committed in the same destination transaction, as PostgreSQL does. | (c). It restores and improves on legacy per-command retry, and matches PostgreSQL. Document that procedures which commit internally downgrade to at-least-once. | SP-01, SMY-01, SMS-02 |
| D-4 | **HTTP/gRPC sink completion.** HTTP treats any 2xx (incl. 202) as `Handled`; `HttpDeliveryHandler` is unused. | (a) Keep plain `Handled`, add a configurable success-status set; (b) add a `delivery: consumer` mode that uses `HttpDeliveryHandler`/DeliveryRunner with `DeliveryBatchIdentity` (requires a receiver that speaks the confirmation protocol); (c) delete `HttpDeliveryHandler`. | (a) for ordinary webhooks (default success set 200–204 excluding 202; `202` opt-in) **and** (b) as an explicit mode. Do the equivalent for gRPC (exact acknowledgement already exists). | HS-01, HS-02, GS-08 |
| D-5 | **Template render-failure policy.** Native fails `handle` (an infinite retry for a deterministic error); legacy logged and fell back to the default body or raw data. | (a) Fail at construction for unknown helpers, and treat a render failure as a permanent per-change error governed by `failurePolicy` (strict = fail and surface status, skip = count and skip); (b) fall back to the default body. | (a). Never retry a deterministic failure as transient. | X-08, LG-01 |
| D-6 | **Pause for host-driven sinks.** | (a) Success is permitted when the host guarantees no `handle` while paused and waits for the in-flight `handle`; (b) every sink must gate explicitly or return `unsupported`. | (a), after confirming the host behaviour with a test; in-flight retries must still observe pause. SSE and profiler need real gates (CW-96). Ask the CW-96 owners to list the bridge sink and PostgreSQL storedproc driver. | X-01, all pause tasks |
| D-7 | **Which legacy-only reactions become native sinks** (§15). | Per reaction: port / extend an existing sink / keep ordinary / retire. | Port: aws-sqs, rabbitmq, loki, eventgrid, azure-storage, a2a, dashboard, mcp. Extend capture into a `file` sink. Application: migrate consumers to the existing native type. Platform: owner decision with the Drasi Platform roadmap (keep ordinary meanwhile). snapshot-test: retire with CW-63. | LR-* |
| D-8 | **Bridge hosting.** | As source D-3. | Port to the SDK. | BS-10 |
| D-9 | **Legacy wire and template compatibility.** gRPC uses the new `drasi.v1.ChangeService` instead of legacy `ReactionService`; templates lose `query_id`, `query_name` and `data`; HTTP/gRPC defaults differ (timeouts, batch sizes). | (a) Breaking, with a migration guide; (b) add a `legacyReactionService` output mode and legacy context aliases. | (a); breaking changes are allowed. Publish the mapping in each CONFIGURATION.md. | HS-10, GS-10, LG-05 |
| D-10 | **PostgreSQL delivery-table ownership.** | (a) Keep the shared `public` table; (b) configurable schema/table, owner column per consumer scope, per-consumer row limits, Erase deletes the consumer's rows. | (b). | SPG-01 |
| D-11 | **Capture truncation.** `append=false` truncates on every start (`capture/src/component.rs:107`), wiping the previous run on the same retained object. | Truncate once per object vs per activation, or an explicit mode. | An explicit `truncate: onCreate \| onStart` with `onCreate` default. | CP-01 |
| D-12 | **Log output channel.** Legacy printed to stdout; native uses `log::info!` with a fixed target. | (a) Keep `log` with configurable level/target; (b) add a `stdout` option. | (a) plus (b). | LG-03 |
| D-13 | **Profiler metric definitions** (units, `total` definition, stddev). | Align with legacy or document the new definitions. | Restore legacy definitions (ms, stddev, `source_send → reaction_complete`) and stamp reaction times correctly. | PF-03 |
| D-14 | **gRPC connection at start.** Legacy connected lazily, so start succeeded while the receiver was down; native connects eagerly. | Eager (fail start) vs lazy (`not_ready` status until connected). | Lazy by default with readiness reporting, bounded by the start deadline when `requireConnectedAtStart: true`. | GS-02 |
| D-15 | **SQL Server authentication.** Only SQL authentication; tokens are collapsed into a SQL-auth pair (`storedproc-mssql/src/lib.rs:242-243`). Azure SQL needs Entra ID. | Add access-token authentication (Tiberius `AADToken`) via activation credentials, or document the gap. | Add it (shared with source MS-12). | SMS-06 |

**Decisions taken (2026-11-15)** for the HTTP, gRPC, SSE and log sinks:

- **D-1:** accepted. The four sinks declare `erases_owned_state` with a no-op
  Erase.
- **D-2:** accepted (flatten). HTTP `token`/`secretHeaders` and gRPC
  `secretMetadata` are top-level secret fields. `ConfigOption::secret()` is not
  yet added.
- **D-4:** (a) and (b). HTTP `successStatusCodes` defaults to `[200, 201, 203,
  204]`; `202` must be listed. The HTTP library also registers
  `drasi.network/http-delivery`, a DeliveryRunner consumer that requires the
  exact `DeliveryBatchIdentity` confirmation. gRPC keeps the exact
  acknowledgement and adds the delivery ID; its consumer mode waits for a
  protocol field (GS-08).
- **D-5:** (a) for HTTP and gRPC: a render failure is a permanent per-change
  failure under `failurePolicy`. **Log differs:** `renderFailurePolicy`
  defaults to `fallback` (the legacy behaviour: emit the plain format and warn),
  with `strict` opt-in. **SSE also differs:** a body that fails to render
  falls back to the per-change frame and a path to the configured path, with a
  warning; only an invalid rendered path fails the input (a volatile broadcast
  has no useful strict mode).
- **D-6:** (a) for HTTP, gRPC and log (host-driven pause; retry waits are
  drop-safe). SSE has a real gate.
- **D-9:** (a). Each CONFIGURATION.md has a legacy mapping.
- **D-12:** (a) plus (b): `level`, `target`, `output: log | stdout | stderr`.
- **D-14:** lazy by default; `requireConnectedAtStart: true` connects within the
  start deadline.

D-3, D-7, D-8, D-10, D-11, D-13 and D-15 are untouched by this work.

---

## 3. Shared network sink runtime (`common/network`)

HTTP, gRPC and SSE share configuration helpers, batching, backoff and the
failure policy in `common/network`. Fix these once.

| ID | Pri | Task | Status |
|---|---|---|---|
| NS-01 | P0 | **Cancellable, deadline-aware backoff (X-05).** Replace `backoff(attempt)` (`config.rs:174-179`: 100 ms × 2ⁿ, cap 5 s, no jitter) with the shared policy: jitter, `Retry-After`, a time budget and a cancellation token supplied by the component's activation scope. | Done: `common/network::retry` (feature `sink`); drop-safe waits |
| NS-02 | P0 | SDK `ConfigOption::secret()` and secret header/metadata maps (X-09). | Secret fields done; SDK `ConfigOption::secret()` open |
| NS-03 | P1 | Replace `eprintln!` (HTTP, gRPC and SSE sinks; receiver) with `tracing`, sanitise logged fields (port `MAX_LOG_FIELD_LEN`), count skips under `FailurePolicy::Skip` (`config.rs:97`). | `eprintln!` replaced by the `diag!` log macro with `MAX_LOG_FIELD_LEN` sanitising; skip counters open (X-11) |
| NS-04 | P1 | One retry/classification implementation shared with `http-common`/`grpc-common` (the native loops duplicate `http-common/src/reaction/process.rs:462-557` and `grpc-common/src/send.rs` with different semantics). | One native policy in `common/network`; the legacy `http-common`/`grpc-common` loops are not converged |
| NS-05 | P2 | Publish or remove the silent `max_inputs` clamp (`batching.rs:39`). Deduplicate adaptive validation (also flagged as source NET-07). | Clamp kept and published (`maxInputs` ≤ 1024 in CONFIGURATION.md); adaptive-validation dedup open |
| NS-06 | P2 | Fix `NETWORK.md` ABI text (`:6-11,231,438`) and add lifecycle and delivery-profile sections per sink. | Done |

---

## 4. HTTP sink (`drasi-computation-sink-http`)

### 4.1 Current state

- Configuration (`http/src/config.rs:29-60`, camelCase, `deny_unknown_fields`):
  `outputs`, `url`/`baseUrl`, `token`, `headers`, `outputTemplates`, `adaptive`,
  `batchEndpoint`, `maxBatchBytes` (16 MiB), `maxRequestBytes` (32 MiB),
  `timeoutMs` (60 000), `maxRetries` (3, ≤100), `failurePolicy`.
- One request per change, or `{"batch":[…]}` per output with host batching;
  templates via `http-common`; redirects disabled; identity headers
  `x-drasi-component`/`x-drasi-port`.
- `start` builds the client and `stop` drops it (`component.rs:364-377`); no
  workers. `pause`/`resume`/`dispose` are `nothing_to_do` (`:343-361`).

### 4.2 Lifecycle and management tasks

| ID | Pri | Task | Status |
|---|---|---|---|
| HS-01 | P0 | **In-flight retries observe stop and pause.** The retry loop (`component.rs:99-123`) uses NS-01's cancellable policy. `Abort` cancels within the deadline; `Drain` lets the current request finish. Test with a stalled server. | Done: budgeted, drop-safe retries; Drain waits at most `retryBudgetMs` |
| HS-02 | P0 | **Completion semantics (D-4).** A configurable success-status set, default excluding 202; document it. Add a `delivery: consumer` factory mode or flag that uses `delivery::HttpDeliveryHandler` (`delivery.rs:87-141`) with the DeliveryRunner, or delete the module per D-4. | Done: `successStatusCodes`; `drasi.network/http-delivery` factory |
| HS-03 | P1 | **Management (X-10).** InPlace: templates, headers, timeouts, retry, failure policy. Restart: `url`/`baseUrl`, TLS. Recreate: `outputs`, delivery mode. | Done |
| HS-04 | P0 | **Secrets (X-09):** `token` and header values secret; secret references or activation credentials. | Done: `token`, `secretHeaders` (object-secret resolution unverified) |

### 4.3 Defects and functional gaps

| ID | Pri | Task | Status |
|---|---|---|---|
| HS-05 | P1 | Honour `Retry-After` on 429/503; read a capped error body for diagnostics and connection reuse (`component.rs:106-121`). | Done: `Retry-After`; capped, sanitised error body |
| HS-06 | P1 | Idempotency header (X-07). | Done: `x-drasi-delivery-id` |
| HS-07 | P1 | Reject templated `x-drasi-*` headers at parse time instead of at runtime (`component.rs:177-189`). | Done: reserved headers rejected at parse |
| HS-08 | P1 | Under `failurePolicy: strict`, a batch-template render failure falls back to the default body (`component.rs:211-218`); apply D-5 instead. | Done (D-5) |
| HS-09 | P1 | TLS options: custom CA and client certificate (mTLS); pool and connect-timeout tuning (legacy set idle 90 s and 10 idle per host, `reactions/http/src/http.rs:101-107`). | Done: CA, mTLS, pool and connect timeouts |
| HS-10 | P2 | Document default drift from legacy (timeout 5 000 → 60 000; base URL default removed; redirects no longer followed) per D-9. | Done (CONFIGURATION.md) |
| HS-11 | P2 | Name the batch-envelope overhead constant `12` (`component.rs:276,288,305`). In `delivery/receiver.rs`: replace `eprintln!` (`:90`), stop returning internal error text to clients (`:93`), name the constants (`:61,113`). | Done |

### 4.4 Legacy parity (`reactions/http`, `http-common`)

| Legacy capability | Native | Action |
|---|---|---|
| Per-query/default templates for body, URL, method, headers per operation | Present (shared renderer), keyed by output alias | Document mapping |
| Bearer token as `ConfigValue` (`reactions/http/src/descriptor.rs:117`) | Plain string | HS-04 |
| Adaptive batching + `batchEndpoint` | Present (host batching) | none |
| Retry 3 attempts, 5xx/408/409/425/429 classes | Configurable; same classes | NS-04 |
| Log sanitisation | Missing | NS-03 |
| Strict/AutoSkipGap recovery with checkpoints | `failurePolicy` + host progress | Document |
| Poison drop advanced checkpoint | Removed intentionally | Document |
| Durable delivery with recovery policy | `Handled` only | HS-02 |

### 4.5 Tests and documentation

| ID | Pri | Task | Status |
|---|---|---|---|
| HS-30 | P0 | X-02 kit. Today: 3 config unit tests (`config.rs:182-236`), loopback retry/skip/pending (`host-sdk/tests/native_network_loopback.rs:858-918`), templates and coalescing (`native_network/http_family.rs:351`). Add: stop/pause during backoff, `Retry-After`, 202/204, header rejection, `maxRequestBytes` splitting, TLS, redirect rejection, consumer mode crash/replay. | Linked kit and destination tests done; dynamic-library kit and consumer crash case open |
| HS-40 | P0 | `DESIGN.md` and `CONFIGURATION.md` (X-14), including the receiver contract for consumer mode and the template reference. | Done |

**Related CW:** 44, 41, 14, 36, 96 (confirm host-driven pause), 98, 17, 21, 22, 26.

---

## 5. gRPC sink (`drasi-computation-sink-grpc`)

### 5.1 Current state

- Configuration (`grpc/src/config.rs:46-77`): `endpoint` (plaintext only,
  `:114-123`), `metadata`, `timeoutMs` 60 000, `maxRetries` 5,
  `connectionRetryAttempts` 10, `initialConnectionTimeoutMs` 15 000,
  `batchSize`/`batchFlushTimeoutMs` or `batching`, `outputTemplates`,
  `maxBatchBytes`, `maxRequestBytes` 4 MiB, `outputFormat`.
- Protocol `drasi.v1.ChangeService/ProcessChanges`; `Handled` only when
  `success` and `items_processed` equals the item count (`component.rs:136-148`).

### 5.2 Tasks

| ID | Pri | Task | Status |
|---|---|---|---|
| GS-01 | P0 | **Start honours cancellation and deadline.** The connect loop (`component.rs:323-345`) can run about 3 minutes regardless of `StartContext`. | Done |
| GS-02 | P1 | **Connection policy (D-14):** lazy connect with readiness/status reporting, or eager bounded by the deadline. | Done: lazy; `requireConnectedAtStart` |
| GS-03 | P0 | **TLS/mTLS** (`https://` endpoints, CA, client identity via secret references; CW-44 "Native gRPC TLS is not advertised"). | Done |
| GS-04 | P1 | **Classification by `tonic::Code` (X-05).** Stop retrying InvalidArgument, Unauthenticated, PermissionDenied and Unimplemented (`component.rs:158-180`; `grpc-common/src/send.rs:309-333`, where `"connection reset"` shadows BrokenPipe). Treat `success=false` per D-5. Remove the triple timeout (`component.rs:97,128,130`) and the double connect timeout (`:93-103`). Make the ResourceExhausted 5 s delay (`:177`) part of the backoff policy. | Done: `tonic::Code` classes; `success=false` permanent; single timeout |
| GS-05 | P0 | `metadata` values secret (X-09). | Done: `secretMetadata` |
| GS-06 | P1 | In-flight retries observe stop and pause (as HS-01). | Done |
| GS-07 | P1 | Management: InPlace templates/metadata/timeouts/batching; Restart endpoint/TLS; Recreate outputs. | Done |
| GS-08 | P1 | Consumer mode with `DeliveryBatchIdentity` on the acknowledgement (D-4), and an idempotency metadata entry (X-07). | Delivery ID done; consumer mode open (needs a `grpc-common` protocol field) |
| GS-09 | P2 | Keepalive, HTTP/2 tuning, optional compression. Replace `expect("pending batch")` (`:270`) and `unreachable!` (`:200,366`). | Done: keepalive, gzip; panics removed |
| GS-10 | P2 | Migration guide from legacy `ReactionService` and default drift (batch 100 → 1, flush 1 000 → 100 ms, timeout 5 000 → 60 000, `grpc-common/src/output/config.rs:21-47`) per D-9. GoAway handling parity (`send.rs:235-253`). | Done (CONFIGURATION.md); GOAWAY handled by classification |
| GS-30 | P0 | X-02 kit. Existing coverage: `native_network/grpc_family.rs`, `native_network_loopback.rs:1077`. Add: start cancellation, reconnect mid-batch, permanent status codes, stop during retry, TLS, and the CW-36 completion profile for native gRPC factories. | Linked kit and loopback done; TLS and permanent codes tested; dynamic-library kit open |
| GS-40 | P0 | `DESIGN.md` (service contract, acknowledgement rule, batching) and `CONFIGURATION.md`. | Done |

### 5.3 Legacy parity (`reactions/grpc`, `grpc-common`)

| Legacy capability | Native | Action |
|---|---|---|
| Lazy connection | Eager | GS-02 |
| Fixed and adaptive batching | Host batching | none |
| `ConfigValue` endpoint/metadata | Plain | GS-05 |
| GoAway/ResourceExhausted special cases | Generic reconnect / 5 s | GS-04, GS-10 |
| Templates, metadata templates, `outputFormat` | Present | none |
| `ReactionService` wire | `ChangeService` | D-9 |
| TLS | Absent in both | GS-03 (improvement) |

**Related CW:** 44 (TLS), 36, 41, 14, 96, 98, 17, 22, 26, 67.

---

## 6. SSE sink (`drasi-computation-sink-sse`)

### 6.1 Current state

- Configuration (`sse/src/component.rs:63-82`): `host` 0.0.0.0, `port` 8080,
  `ssePath`, `heartbeatIntervalMs` 30 000, `broadcastCapacity` 1 024, `routes`,
  `defaultTemplate`, `maxPaths` 1 024.
- `Accepted` completion; output-snapshot reader served at `/snapshot/:alias`
  (`:309-411`); lagging subscribers are disconnected (`:428-431`).
- `start` binds and spawns an owned server (`:471-578`). `stop` cancels and
  calls `join_owned_worker_gracefully` with a fixed 5 s (`:579-593`) and
  correctly retains ownership on timeout.

### 6.2 Tasks

| ID | Pri | Task | Status |
|---|---|---|---|
| SS-01 | P0 | **Real pause (CW-96).** Gate new connections and snapshot reads (503 with `Retry-After`), stop heartbeats or keep only keep-alive comments, and reopen on resume. Otherwise return `unsupported`. | Done |
| SS-02 | P0 | **Stop:** use `ctx.deadline()` instead of 5 s; `Abort` closes connections immediately. | Done |
| SS-03 | P0 | **Exposure (X-12):** default `127.0.0.1`; configurable CORS origins (hard-coded `*` at `:358-367,507-516,535-551`); optional bearer authentication from a secret reference; optional TLS. | Done |
| SS-04 | P1 | Subscriber cap and per-client admission; prune idle dynamic-path broadcasters. | Done |
| SS-05 | P1 | **Management (CW-94):** assess with InPlace templates/heartbeat, Restart host/port, Recreate outputs. Re-enable `sse_tests::native_sse_declared_inputs_follow_a_live_rebind_*` (`host-sdk/tests/native_network/sse.rs:914,923`) once CW-94 is settled. | Assess/apply done; the two ignored rebind tests stay ignored under CW-94 |
| SS-06 | P2 | SSE comment keep-alive (legacy `reactions/sse/src/sse.rs:588-591`) and optional `id:` fields with `Last-Event-ID` semantics documented as volatile. Replace `eprintln!` (`:334,403,429,553,571,609`) and the `expect`s (`:349,636`). `Running::drop` cancels without joining (`:255-259`); make sure no path relies on it. | Done: keep-alive comments, `eventIds`; `Running::drop` is a safety net only |
| SS-30 | P0 | X-02 kit. Existing: roundtrip on both flavours, dynamic routes, snapshot policy and templates (`native_network/sse.rs`), in-crate lag/cleanup/bind tests (`component.rs:707-799`). Add: pause, short stop deadline, Abort, subscriber flood, CORS configuration, authentication. | Linked kit and in-crate tests done; dynamic-library kit open |
| SS-40 | P0 | `DESIGN.md` (broadcast model, volatility, snapshot handover, lifecycle) and `CONFIGURATION.md` (security deployment guidance). | Done |

### 6.3 Legacy parity (`reactions/sse`)

| Legacy capability | Native | Action |
|---|---|---|
| host/port/path/heartbeat defaults | Same | SS-03 changes the host default (intentional) |
| Per-query and default templates with static/dynamic paths | Present, bounded by `maxPaths` | none |
| Keep-alive comments | Missing | SS-06 |
| CORS `Any` | Hard-coded `*` | SS-03 |
| `/snapshot/:query_id` via snapshot fetcher | `/snapshot/:alias` via output snapshots | Document |
| Stop: heartbeat cancel + graceful join | Graceful join | SS-02 |

**Related CW:** 96, 94, 44, 41, 98, 17, 26.

---

## 7. Stored procedures: shared work

The three stored-procedure sinks diverge structurally (PostgreSQL is split and
is a consumer; MySQL is a thin wrapper over `mysql-common::storedproc`; SQL
Server is one 674-line `lib.rs`), in validation (three validators), in delivery
profile and in credentials. Converge them.

| ID | Pri | Task |
|---|---|---|
| SP-01 | P0 | **Delivery profile (D-3).** Move MySQL and SQL Server to `ConsumerMode::External` with a destination cursor table modelled on `postgres-common/src/delivery.rs`: one destination transaction per operation commits the procedure effect and the cursor. Factor the cursor protocol into a shared crate or a common pattern so the three stay identical. |
| SP-02 | P0 | **Lifecycle (X-01).** Honour `StopMode` and `ctx.deadline()`/`cancellation()` in start and stop instead of only configuration timeouts (PG `component.rs:213-269`; MySQL `runtime.rs:258-283`; SQL Server `lib.rs:286-305,339-353`). `Abort` cancels; MySQL already has a session KILL proof (`runtime.rs:175-227`); SQL Server documents that Tiberius cannot send ATTENTION. |
| SP-03 | P0 | **No wedging (finding 2).** Remove the latched `interrupted` refusal. Classify errors: confirmed rollback or not-executed is retryable; outcome unknown is surfaced to the runner; reconnect inside the activation. |
| SP-04 | P1 | **Configuration (X-09):** `configuration_details` with published defaults; top-level secret `password` and TLS keys; templates no longer secret. |
| SP-05 | P0 | **One template validator (X-08)** enforcing parameter-only templates for all three. |
| SP-06 | P1 | **Persistent activation-owned connection** with bounded reconnect, for MySQL and SQL Server (legacy used a pool or a shared client). |
| SP-07 | P2 | `Factory::assess` + `NativeManagement`: InPlace templates, routes and timeouts; Restart connection and credentials; Recreate outputs and the cursor identity. |
| SP-08 | P1 | Tests (all three): pause, Abort, Erase twice, reconnect after a database restart, large inputs, template-injection rejection, credential refresh, recovery after a failure without Stop/Start, crash between effect and cursor commit. |
| SP-09 | P1 | Migration guide: legacy context fields versus the native change context, and the `PATCH` `after` semantics (reproduce first). |

---

## 8. PostgreSQL stored procedures (`drasi-computation-sink-storedproc-postgres`)

### 8.1 Current state

- `ConsumerMode::External` consumer (`lib.rs:76-84`) over
  `postgres-common/src/delivery.rs`: one `ReadCommitted` transaction per
  operation applies the procedure and advances the cursor
  (`delivery.rs:94-187`); retryable SQLSTATEs 40001, 40P01, 55P03
  (`:226-231`); a failed commit is `CommitUnknown` (`:257`).
- Configuration (`config.rs:11-44`): host, port, database, user, password,
  `ssl`, optional CA and client identity (≤64 KiB each), outputs, routes,
  default template, command/connect/shutdown timeouts. Whole `settings` secret
  (`lib.rs:57-61`).
- TLS with system trust, optional CA and client identity; plaintext to a
  non-loopback host only warns (`component.rs:140-176`).

### 8.2 Tasks

| ID | Pri | Task |
|---|---|---|
| SPG-01 | P0 | **Delivery table ownership (D-10).** Configurable schema/table (today `public.drasi_delivery_stream_v1`, `delivery.rs:12-20`); add an owner column for the consumer scope; per-consumer or configurable stream limit instead of the database-wide 4 096 (`:11,151-158`); declare `erases_owned_state` and delete only this consumer's rows in `dispose(Erase)`, idempotently. Document the grants. Verify PostgreSQL 15+ least-privilege creation (reproduce first). |
| SPG-02 | P1 | **Activation credentials** (`supports_activation_credentials`) for token/password identity providers; restores legacy `executor.rs:51-72`. Decide whether certificate providers need an SDK extension or remain static PEM. |
| SPG-03 | P0 | **Reconnect** when the driver dies; treat 08xxx and `CommitUnknown` as retry-after-reconnect, since the cursor makes the retry idempotent (`delivery.rs:226-231`; `component.rs:274-280`). |
| SPG-04 | P1 | Expose `consumer_progress_reader`. Move table creation out of `start` into a checked, retry-safe step with clear privilege errors. |
| SPG-05 | P2 | Remove the double connect timeout (`component.rs:135,219-224`). Option to refuse plaintext to non-loopback hosts instead of warning. |
| SPG-30 | P0 | X-02 kit plus SP-08. Existing live coverage: partial-batch resume, deduplication, retryable rollback and cancellation (`sources/postgres/tests/native_transactions/libraries.rs:516-749`, ignored; needs Docker). Add: stream-cap exhaustion, Erase, reconnect, PostgreSQL 15 non-superuser. |
| SPG-40 | P0 | `DESIGN.md` (cursor protocol, exactly-once conditions, ownership) and `CONFIGURATION.md` (grants, procedures, TLS, credentials, update matrix, Erase, runbook for the cursor table). Fix the ABI text (`README.md:5`). |

### 8.3 Legacy parity (`reactions/storedproc-postgres`)

| Legacy capability | Native | Action |
|---|---|---|
| Per-query added/updated/deleted commands with route fallback | Present (keyed by alias) | Document |
| `{{param}}` binding | Present, parameter-only | none |
| Whole-command retry with backoff (`executor.rs:268-295`) | Host runner retry on rollback SQLSTATEs | SPG-03 |
| Identity providers incl. certificate mTLS (`executor.rs:51-72,120-135`) | Static PEM only | SPG-02 |
| Opt-in transactional delivery (`postgres_common::delivery`) | Default | none |
| Log-and-skip errors | Propagate; wedges | SP-03 |
| `ConfigValue` per field | Whole-object secret | SP-04 |

**Related CW:** 44, 41, 14, 98, 96 (driver), 22, 26.

---

## 9. MySQL stored procedures (`drasi-computation-sink-storedproc-mysql`)

### 9.1 Current state

- Plain `Component::Sink` (`lib.rs:97`); configuration re-exported from
  `mysql-common/src/storedproc/config.rs:39-73` (`deny_unknown_fields`):
  host, port, database, user, password, `ssl` (→ verify-full), CA, outputs,
  routes, default template, timeouts, `max_protocol_bytes` (8 MiB, ≤16 MiB),
  `retry_attempts` (3, ≤8, connect only).
- Each `handle` opens **two** connections (control and procedure), checks
  `server_id`, runs commands sequentially and cleans up the session
  (`runtime.rs:175-227`). No transaction or cursor (`runtime.rs:24-25`).
- Activation credentials supported (`lib.rs:47-61`).

### 9.2 Tasks

| ID | Pri | Task |
|---|---|---|
| SMY-01 | P0 | **Remove poison-input caps:** chunk instead of rejecting more than 4 096 changes or more than `max_protocol_bytes` per input (`runtime.rs:95-98,124-131`). |
| SMY-02 | P0 | SP-01 (consumer + destination cursor) and SP-03 (no wedging, `runtime.rs:306-309`). |
| SMY-03 | P1 | SP-06: a persistent control/procedure session pair per activation (today two new connections per input and two more at start, `runtime.rs:258-270`). |
| SMY-04 | P2 | Rename `retry_attempts` to `connectRetryAttempts` (it is not legacy's command retry). Stop reusing binlog fields (`server_id = 1`, heartbeat 1 000) in `connection()` (`config.rs:89-90`). |
| SMY-30 | P0 | X-02 kit plus SP-08. Existing: construction tests (`src/tests.rs:35-175`), live procedure lifecycle (`host-sdk/tests/native_mysql_components.rs:593,1317-1763`, ignored; Podman). Add: more than 4 096 changes, partial failure and replay, reconnect. |
| SMY-40 | P0 | `DESIGN.md` and `CONFIGURATION.md`; document limits and connection cost until SMY-01/03 land. |

### 9.3 Legacy parity (`reactions/storedproc-mysql`)

| Legacy capability | Native | Action |
|---|---|---|
| Connection pool (`executor.rs:94`) | Two connections per input | SMY-03 |
| Command retry with backoff (`executor.rs:174-195`) | Connect retry only | SP-01 / X-05 |
| Identity providers (`executor.rs:43-70`) | Activation credentials | none |
| `SslOpts::default` | Verify-full + CA | Improvement |
| Unbounded input size | Hard caps | SMY-01 |
| `ConfigValue` per field (`descriptor.rs:58-81`) | Whole-object secret | SP-04 |

**Related CW:** 44, 41, 14, 98, 22, 26, 85.

---

## 10. SQL Server stored procedures (`drasi-computation-sink-storedproc-mssql`)

### 10.1 Current state

- Plain `Component::Sink` (`lib.rs:166`), all code in `lib.rs`. Configuration
  (`lib.rs:33-49`): `connection: MsSqlSourceConfig`, outputs, routes, default
  template, command and shutdown timeouts.
- Each `handle` spawns a `Work` task that opens a new TDS connection and runs
  every command with no transaction (`lib.rs:384-393`), under one timeout for
  the whole input (`:394-396`). No retry.
- Activation credentials (SQL authentication only, `lib.rs:115-129,229-254`).
- Live qualification deferred (CW-78).

### 10.2 Tasks

| ID | Pri | Task |
|---|---|---|
| SMS-01 | P0 | **Template injection (finding 1).** Parameter-only validation via SP-05: reject bare `{{x}}` in SQL text, `{{{x}}}`, inline `{{json x}}`, partials and decorators. Failing test first. |
| SMS-02 | P0 | **Large inputs (CW-44):** remove the 100 000-command cap (`lib.rs:375-378`), chunk, bound rendering memory, apply the timeout per command; enforce the 2 100-parameter limit per command with a clear error. |
| SMS-03 | P0 | SP-01 (consumer + destination cursor) and SP-03 (no wedging, `lib.rs:383-397`). |
| SMS-04 | P1 | **Own configuration struct.** `MsSqlSourceConfig` lacks `deny_unknown_fields`, derives `Debug` with the password and accepts source-only fields (`mssql-common/src/config.rs:150-196`; shared with source MS-13). Default `encryption` to `On` or warn on plaintext. Require at least one template (PG `config.rs:105-108` and MySQL `config.rs:110-113` already do). |
| SMS-05 | P1 | SP-06: a persistent connection (today one TCP+TLS+login per input; the start test connection is discarded, `lib.rs:295-298`). |
| SMS-06 | P1 | Entra ID access-token authentication (D-15). |
| SMS-07 | P2 | Drop guard for pending `Work` (today a pending task is detached when the sink is dropped; reproduce first). Replace `unreachable!` (`lib.rs:276`). Split `lib.rs` into `config.rs`/`component.rs`/`lib.rs` as PostgreSQL does. |
| SMS-30 | P0 | X-02 kit plus SP-08 with the CW-78 environment. Existing: construction tests (`lib.rs:426-628`); live cases blocked (`host-sdk/tests/native_mssql_components.rs:335-351`). |
| SMS-40 | P0 | `DESIGN.md` and `CONFIGURATION.md`; fix the ABI text (`README.md:7-8`); keep and extend the capability table (`README.md:31-43`). |

### 10.3 Legacy parity (`reactions/storedproc-mssql`)

| Legacy capability | Native | Action |
|---|---|---|
| Command retry with backoff (`executor.rs:172-195`) | None | SP-01 / X-05 |
| Shared client (`executor.rs:115-119`) | New connection per input | SMS-05 |
| Per-command timeout | Per input | SMS-02 |
| Inline `{{json}}` allowed | Allowed (unsafe) | SMS-01 (intentional tightening) |
| `trust_cert()` always (`executor.rs:88`) | Explicit trust | Improvement |
| Identity providers (password; certificates rejected) | Activation credentials (SQL auth) | SMS-06 |
| Unbounded input size | 100 000-command cap | SMS-02 |

**Related CW:** 44 (large sink batch), 78, 41, 14, 98, 66, 22, 26.

---

## 11. Bridge sink (`drasi-computation-sink-bridge`)

### 11.1 Current state

- Implements `drasi_lib::computation::v1::{ComputationComponent, EnvelopeSink, ComponentFactory}`
  (`sinks/bridge/src/component.rs:8-11`), not the plugin SDK; `drasi-lib`
  dependency at `sinks/bridge/Cargo.toml:13` and `common/bridge/Cargo.toml:13`;
  registered only by `instance-host` (`components/instance-host/src/host.rs:723-728`).
  `ci/check_native_dependencies.py:49` silently excludes it because it lacks
  abi-family metadata.
- Delivery: volatile `Accepted` or durable `Handled`; a private single-frame
  sender ledger (`common/bridge/src/state.rs:4-5,23,66-69`) in a private graph
  namespace; replay dedups on logical sequence (`state.rs:286-302`). Not the
  DeliveryRunner.
- Retry in `handle` (`component.rs:252-318`): fixed delay, defaults 3 × 50 ms,
  bounds 1..=16 attempts, delay ≤5 s, timeout ≤60 s (`config.rs:195-196,211-213`).
- Lifecycle: `passive_component_lifecycle!` for initialize/pause/resume
  (`component.rs:213-215`); start and stop ignore their contexts; dispose
  refuses Erase (`:217-226`). No logging.

### 11.2 Tasks

| ID | Pri | Task |
|---|---|---|
| BS-01 | P0 | **Retry budget** (shares source BR-01/BR-09): X-05 policy with jitter and a time budget; a receiver `Busy`/`Paused` is a cancellable wait; sleeps observe stop and pause. |
| BS-02 | P0 | **StopMode** (shares BR-02): `Drain` finishes the in-flight send and receipt within the deadline; `Abort` cancels; test that a cancelled durable `handle` resumes via the receipt lane. |
| BS-03 | P0 | **Volatile restart against a live receiver** returns non-retryable `StateLost` (`common/bridge/src/receiver.rs:401-405` vs `component.rs:204`), so the link stays dead until the receiver restarts (reproduce first). Define semantics (fresh volatile session epoch or a typed recoverable error) and test. |
| BS-04 | P0 | **Erase** (shares BR-03, source D-12, CW-98): erase the durable mapping namespace through its cleanup owner (`component.rs:165,217-226`), idempotently. |
| BS-05 | P0 | **Split wire contract from local settings** (shares BR-07): remove timeout/retry/queue settings from `LinkContract` and the fingerprint (`config.rs:372,398-400`; ledger refusal `state.rs:172-177`). |
| BS-06 | P0 | **Observability** (shares BR-04): trace each attempt, code, session/generation change, exhaustion and seal; surface egress status. |
| BS-07 | P0 | Docs (shares BR-05/BR-40): fix wire version text (`common/bridge/README.md:24-27`), the retry description (`:256-257`) and the worker claim (`:388-389`, true only for the receiver); add a sink `DESIGN.md` and `CONFIGURATION.md`. |
| BS-10 | P1 | **SDK port (D-8, = BR-06):** `NativeSink`, `export_computation_plugin!`, no `drasi-lib`, plugin metadata so `check_native_dependencies` covers it. Consider the consumer extension for the durable profile instead of the private ledger. |
| BS-11 | P1 | Start honours the context; fix `failed_cleanup` ordering (`component.rs:165-177`); stop idempotent after a partial failure. |
| BS-12 | P1 | Management: local settings InPlace, contract Recreate. Full schema; remove the duplicated `sender_endpoint` (`factory.rs:118-121`). |
| BS-13 | P1 | Control lane: `BridgeControl::request` uses `try_lock` (`client.rs:39-41`), so a concurrent `control().status()` call makes delivery fail with `Busy` and `invalidate` drops the user's control socket; use a queued lock or per-caller connection. Do not swallow the `producer_status` error (`client.rs:141-146`). |
| BS-14 | P1 | Egress state: set `pending` after validation (`component.rs:237-243`); allow a failed remote seal to be retried (`boundary.rs:280`). |
| BS-20 | P2 | Remove `expect`s (`component.rs:178,239,257,264,268`; `client.rs:247`; `transport.rs:173`; `state.rs:143,226`; `config.rs:227,333,399`; `endpoint_control.rs:98,113`; `factory.rs:95`); typed errors without `to_string` (`error.rs:50-52,83`); name constants; event-driven `wait_drained` (`endpoint_control.rs:137-152`). |
| BS-21 | P2 | Align ALPN `drasi-bridge/1` (`transport.rs:16`) with protocol 2 and negotiate versions; remove the per-`handle` `producer_status` round trip; TLS pin rotation (shares BR-11/BR-12). |
| BS-30 | P0 | Sink tests (none in-crate): retry exhaustion with attempts > 1, receiver paused beyond the budget, receiver restart mid-frame, `AcceptanceUnknown` resolved through receipts, volatile restart, stop during `handle`, Erase, concurrent control, limits-only change. After BS-10, the X-02 kit. CW-05/CW-91 partition qualification. |

**Related CW:** 05, 75, 91, 96 (not listed; should be), 98, 17, 22, 26, 41, 14, 97.

---

## 12. Capture (`drasi-computation-sink-capture`)

Configuration `path` (required) and `append` (default `false`); writes one JSON
envelope frame per line and flushes (`capture/src/component.rs:18-44,126-137`).
It is the main real-library test sink (`host-sdk/tests/native_computation_pipeline.rs`,
`native_component_updates.rs`, `instance-host/tests/processes.rs`).

| ID | Pri | Task |
|---|---|---|
| CP-01 | P1 | Truncation semantics (D-11); today every start truncates (`component.rs:100-112`). Test stop/start retention. |
| CP-02 | P1 | Honour `StopMode`/deadline in the final flush; detect or document a partial trailing line after a cancelled write (`README.md:16`). |
| CP-03 | P1 | **`file` parity (D-7, LR-09):** templates and routes, write modes `append`/`overwrite`/`perChange`, `filenameTemplate` from `reactions/file/src/config.rs:24-56`, either in capture or a new `drasi.standard/file` sink on the shared renderer. |
| CP-04 | P2 | Named codec-limit constant (`component.rs:61`), optional `fsync`, avoid the per-frame copy (`:132`), in-crate unit tests (there are none). |
| CP-40 | P0 | `DESIGN.md` (frame format, flush vs fsync, truncation, no Erase) and `CONFIGURATION.md`. Fix `STANDARD.md:4,116`. |

---

## 13. Log (`drasi-computation-sink-log`)

Configuration `outputs`, `routes` (alias → added/updated/deleted templates),
`defaultTemplate`; logs via `log::info!("[{id}] {msg}")`
(`log/src/component.rs:20-55,216`).

| ID | Pri | Task | Status |
|---|---|---|---|
| LG-01 | P0 | **`json` helper and render-failure policy** (X-08, D-5): register the shared helper set (missing; `component.rs:126`); validate helper names at construction; apply D-5 so a render error never retries forever. Port the legacy template tests (`reactions/log/src/tests.rs:146-661`). | Done (D-5 as taken: `fallback` default) |
| LG-02 | P0 | **Route matching:** validation accepts `.{route}` suffixes (`component.rs:80`) that lookup never matches (`reaction-common/src/templates.rs:297`); make them consistent and test with real aliases (reproduce first). | Done |
| LG-03 | P1 | Precompile templates once (today re-parsed per message, `component.rs:149`); configurable level and target, plus a `stdout` option (D-12). | Done |
| LG-04 | P1 | Management: templates/routes/level InPlace. | Done |
| LG-05 | P2 | Typed schema for `routes`/`defaultTemplate` (legacy DTOs at `reactions/log/src/descriptor.rs:26-80`); optional per-envelope header line and legacy compact formats (`[ADD] …`, `before -> after`) as a `format` option; document the context mapping (D-9). | Done |
| LG-30 | P0 | Host-sdk real-library tests (none today; add to `plugin_package`): six hooks, templates, routes, render failure. | **Open** |
| LG-40 | P0 | `DESIGN.md` and `CONFIGURATION.md` with the template reference and legacy mapping. | Done |

Legacy parity (`reactions/log`): per-query and default templates (present,
keyed by alias); dotted-suffix routing (LG-02); `json` helper (LG-01);
render-error fallback (D-5); no-template compact format (LG-05); stdout output
(D-12); header line (LG-05); template validation at build (present).

---

## 14. Profiler (`drasi-computation-sink-profiler`)

Configuration `windowSize` (1 000) and `reportIntervalSecs` (60); a background
task logs count, mean, min, max, p50, p95 and p99 in nanoseconds
(`profiler/src/component.rs:20-157,210-229`).

| ID | Pri | Task |
|---|---|---|
| PF-01 | P0 | **Pause (CW-96):** move the reporter onto `ActivationScope` + `PauseGate` so pause stops reports and resume reopens them, or return `unsupported` (`component.rs:195-197`). Honour `StopMode` and the deadline; emit a final report on `Drain`. |
| PF-02 | P0 | **Accept non-query input:** skip and count envelopes without profiling metadata instead of failing (`component.rs:249`; reproduce first). |
| PF-03 | P1 | **Metric definitions (D-13):** stamp `reaction_receive_ns` on receipt and `reaction_complete_ns` after processing (today stamped back to back, `component.rs:250-252`); align `total` with legacy (`reactions/profiler/src/profiler.rs:267-268`); restore stddev, ms formatting and the report header. |
| PF-04 | P1 | Cap `windowSize` (schema maximum and parse check) and allocate lazily (today `VecDeque::with_capacity` in `create`, `component.rs:90,172`). Compute statistics outside the mutex `handle` uses (`:107,223`). |
| PF-05 | P1 | Management: interval and window InPlace. |
| PF-30 | P0 | Host-sdk tests (none today): pause gating, source-only input, statistics (port `reactions/profiler/src/tests.rs:44-561`). |
| PF-40 | P0 | `DESIGN.md` (metric definitions, units, window semantics) and `CONFIGURATION.md`. |

---

## 15. Legacy reactions with no native sink

CW-44 requires that "every required source, bootstrapper and reaction/sink
exists, and a capability-by-capability comparison finds no omitted working
legacy functionality". No CW entry names these reactions individually; record
each decision there (see the appendix). Every new port starts compliant with
X-01 to X-15 and uses the shared renderer, retry policy and, where the
destination can confirm, the DeliveryRunner with an idempotency key.

| ID | Pri | Reaction | Native design | Size |
|---|---|---|---|---|
| LR-01 | P0 | `application` | Already native (`NativeApplicationReaction`, `reactions/application/src/native.rs`; in-process, acceptance-only; uses `drasi_lib::passive_component_lifecycle!` for initialize/pause/resume, `:254-256`). Move remaining old-interface consumers (CW-44 "Application components", CW-77), then retire the legacy `ApplicationReaction` (CW-63). Confirm its pause is host-driven per D-6. | S |
| LR-02 | P1 | `aws-sqs` | Consumer External; `MessageDeduplicationId` derived from the delivery ID (legacy uses `Uuid::new_v4()`, `reactions/aws-sqs/src/reaction.rs:280`, which defeats SQS deduplication); optional `SendMessageBatch` with per-entry results; templates for body, attributes and `message_group_id`; credentials via activation credentials. | M |
| LR-03 | P1 | `rabbitmq` | Consumer External with publisher confirms (legacy `basic_publish` without confirms, `reactions/rabbitmq/src/rabbitmq.rs:345-348`, so its checkpoint does not prove broker acceptance); AMQP `message_id` from the delivery ID; TLS; reconnect via X-05. | M |
| LR-04 | P1 | `loki` | Thin native sink on the DeliveryRunner: label templates, tenant, bearer/basic auth, deterministic timestamps for Loki deduplication. An http-sink recipe is not sufficient without new helpers. | S |
| LR-05 | P2 | `eventgrid` | Consumer External; deterministic event IDs (legacy UUIDv5, `reactions/eventgrid/src/event.rs:27-29`) seeded from the delivery identity; CloudEvents/EventGrid schemas; access key or Entra ID. | S–M |
| LR-06 | P2 | `azure-storage` | Blob/Queue/Table targets; blob/table writes idempotent on keys derived from the delivery ID; queue documented as at-least-once; move off the 0.21 SDK (CW-66). | M–L |
| LR-07 | P2 | `a2a` | Consumer External with task-activation state; the deterministic message ID as idempotency key. Needs a decision on where per-key task state lives (destination vs host state). | M–L |
| LR-08 | P2 | `dashboard` | HTTP/WebSocket server per X-12 (shares SSE patterns and the output-snapshot reader); dashboard persistence needs a native state resource; or rebuild the UI on the SSE sink. | L |
| LR-09 | P2 | `file` | CP-03. | S |
| LR-10 | P2 | `mcp` | MCP Streamable-HTTP server per X-12; `resources/read` from output snapshots; bearer auth; session cap. | M–L |
| LR-11 | P2 | `platform` | Owner decision (D-7). It is not published (`export_plugin!` is gated on an undefined `dynamic-plugin` feature, `reactions/platform/src/lib.rs:215-217`). If ported: consumer with a Redis pipeline, control-event mapping. | M |
| LR-12 | P2 | `snapshot-test` | Retire with the ordinary ABI (CW-63) after confirming native fixtures cover snapshot-reader and initialization-failure paths. | S |

---

## 16. Sequencing

**Phase 0: decisions and scaffolding (blocking).** D-1, D-2, D-3, D-4, D-5,
D-6, D-7, D-8, D-10. X-01 (sink guide), X-02 (sink kit), X-05 (retry policy),
X-08 (shared renderer and validator), X-14 (doc templates), the X-09 SDK
secret builder.

**Phase 1: safety-critical P0s, in parallel per family.**

- SQL Server: SMS-01 (injection) first, then SMS-02, SMS-03.
- Stored procedures: SP-03 (no wedging), SP-05, SPG-03, SPG-01, SMY-01.
- SSE: SS-01, SS-02, SS-03.
- Profiler: PF-01, PF-02. Log: LG-01, LG-02.
- Network: NS-01, HS-01, GS-01, HS-04, GS-05.
- Bridge: BS-01 to BS-06.

**Phase 2: delivery profiles, lifecycle and configuration.** SP-01 (MySQL and
SQL Server consumers), HS-02/GS-08 (D-4), SP-02, X-04, X-09, X-10 for every
plugin; the X-02 kit for every plugin in CI.

**Phase 3: parity and capability.** TLS (GS-03, HS-09), credentials (SPG-02,
SMS-06), persistent connections (SP-06), idempotency keys (X-07), CP-03
(file), the bridge SDK port (BS-10 to BS-14), and the legacy ports in priority
order LR-01, LR-02, LR-03, LR-04, then the P2 ports.

**Phase 4: quality and qualification.** Observability (X-11), hygiene (X-13),
the CW-41/14/36/78/85 qualification runs, DESIGN and CONFIGURATION docs
finalised against shipped behaviour, then retirement of legacy reactions
under CW-63 once their native replacement passes.

Each phase ships per plugin as a separate reviewable change.

**Progress (2026-11-15):** from Phase 0, X-01, X-02 (linked), X-05 and the
D-1, D-2, D-4, D-5, D-6 decisions are done for the network and log sinks. The
network and log items of Phases 1 to 3 are done except GS-08 consumer mode. The
next work is the stored-procedure safety items (SMS-01, SP-03), then the
profiler (PF-01, PF-02) and the bridge.

---

## 17. Definition of done (per plugin)

A native sink is production ready when:

1. All six hooks meet X-01. Pause is real, `unsupported`, or a documented and
   tested host-driven success (D-6); stop honours `StopMode` and deadlines,
   including in-flight retries; dispose implements its declared intents
   idempotently.
2. A failed `handle` can be retried without Stop/Start, and no input is
   rejected only for its size.
3. Its delivery profile (X-03) is declared, documented with exact duplicate and
   loss windows, and proven by tests, including a crash test for consumers.
   Remote sinks send an idempotency key.
4. `Factory::assess` covers every configuration field with a tested boundary,
   and `NativeManagement` exists wherever a Pause or Stop boundary is declared.
5. Configuration publishes complete schemas, `configuration_details` and exact
   defaults; secrets are individual secret fields or activation credentials;
   `configuration()` round-trips.
6. Templates use the shared renderer and validator, compile at construction,
   and follow the D-5 failure policy.
7. Every legacy capability in its parity table is present or covered by an
   accepted decision in §2.
8. The X-02 kit passes through the linked and dynamic library on both Tokio
   flavours, and the destination suite runs in a named CI job (CW-85), with no
   skipped case counted as a pass.
9. Errors are typed and classified; retries use the shared policy; status,
   readiness and metrics are reported (CW-22, CW-26).
10. `DESIGN.md` and `CONFIGURATION.md` exist and match the code; README and
    version text is current.
11. No `expect`/`unwrap`/`unreachable!` on FFI-reachable paths, no `drasi-lib`
    dependency, and `make clippy`, `make check-native-dependencies` and
    `cargo fmt` are clean.
12. The related CW entries are updated.

---

## Implementation status (2026-11-15)

Implemented: `common/network` sink runtime (§3), HTTP (§4), gRPC (§5), SSE
(§6) and log (§13), plus X-01, X-02 (linked), X-05, X-07 and X-12. Verified with
the plugin test suites (HTTP 38, gRPC 29, SSE 28, log 32, network-common 24,
SDK 105), the Host SDK `native_network_loopback` (34 passed, 2 ignored under
CW-94), `native_network_wire` and `native_output_snapshots` suites against
freshly built libraries, clippy with warnings as errors, `cargo fmt --check` and
the native-dependency check.

Breaking changes beyond the plan:

- The HTTP library registers two factories (`drasi.network/http-sink`,
  `drasi.network/http-delivery`).
- gRPC `connectionRetryAttempts` and `initialConnectionTimeoutMs` are replaced
  by `connectTimeoutMs` and `requireConnectedAtStart`; a `success=false`
  acknowledgement is now permanent (a receiver signals a transient failure with
  a retryable status such as `UNAVAILABLE`).
- `SINKS.md` no longer claims that templates fail on absent fields: only gRPC
  renders strictly.
- The HTTP source's listener moved to `common/network::server`.

New issues found during implementation:

| ID | Pri | Issue | Owner |
|---|---|---|---|
| NI-01 | P0 | Confirm that the host resolves a secret reference to a whole JSON object (`secretHeaders`, `secretMetadata`). If not, replace the maps with one secret field per header. | host / D-2 |
| NI-02 | P1 | gRPC consumer mode needs a `DeliveryBatchIdentity` field in the `grpc-common` acknowledgement (GS-08). | grpc-common |
| NI-03 | P1 | SSE and log carry local copies of the pause gate, the exposure check and template-helper registration. Move a public pause gate and `Authenticator::authorize_token` into `common/network`, and the helper set into `reaction-common` (X-08). | common/network, reaction-common |
| NI-04 | P1 | No sink metrics (X-11); skips are logged, not counted. | CW-22, CW-26 |
| NI-05 | P1 | LG-30: log has no Host SDK real-library tests or `plugin_package` mapping (X-15). | host-sdk |
| NI-06 | P2 | A strict-render log failure is deterministic and retried until the template changes; document the InPlace fix in the runbook or route it through `failurePolicy`. | log |
| NI-07 | P2 | The sink conformance kit runs only through the linked definition; add the dynamic-library and dual-flavour drivers (X-02). | SDK, host-sdk |

Remaining order for these four sinks: NI-01, NI-02/GS-08, NI-05, NI-07, NI-03,
NI-04.

---

## Appendix: remaining-work cross reference

| Component | Applicable CW IDs |
|---|---|
| http sink | 44, 41, 14, 36, 96 (confirm host-driven pause), 98, 17, 18, 19, 21, 22, 23, 25, 26, 29, 63, 72, 73, 03, 05, 08, 10, 37, 46, 85, 88, 95, 97 |
| grpc sink | 44 (TLS), 36, 41, 14, 96, 98, 17, 18, 21, 22, 23, 25, 26, 29, 63, 67, 72, 73, 03, 05, 08, 10, 37, 46, 85, 88, 95, 97 |
| sse sink | 96, 94, 44, 41, 98, 17, 22, 23, 25, 26, 29, 63, 72, 73, 03, 05, 08, 10, 37, 46, 85, 88, 95, 97 |
| storedproc-postgres | 44, 41, 14, 98, 96 (driver), 17, 18, 21, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 46, 85, 88, 95, 97 |
| storedproc-mysql | 44, 41, 14, 98, 17, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 46, 85, 87, 88, 95, 97 |
| storedproc-mssql | 44 (large sink batch), 78, 41, 14, 98, 66, 17, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 46, 85, 88, 95, 97 |
| bridge sink | 05, 75, 91, 96 (not listed), 98, 17, 22, 23, 25, 26, 41, 14, 18, 19, 21, 63, 72, 73, 03, 08, 10, 36, 37, 46, 85, 88, 95, 97 |
| capture | 41, 98, 17, 22, 26, 72, 73, 03, 08, 10, 37, 46, 77, 85, 88, 95, 97 |
| log | 41, 44, 98, 22, 26, 72, 73, 03, 08, 10, 37, 46, 77, 85, 88, 95, 97 |
| profiler | 96, 41, 44, 98, 17, 22, 26, 72, 73, 03, 08, 10, 37, 46, 85, 88, 95, 97 |
| common/network (sink side) | 96, 44, 17, 19, 21, 25, 26, 29, 67, 05, 08, 14, 36, 41 |
| legacy-only reactions (§15) | 44, 63, 45, 77, 41, 14, 66 (aws/azure), 96 (dashboard/mcp), 70, 92 |

Notes:

- **CW-72**: `CheckpointedSink` forwards hooks but cannot yet prove scoped
  progress deletion; any sink wrapped by it must keep explicit unsupported
  Erase results until that exists.
- **CW-95** (configuration version ranges and migration) is user-deferred.
  Starting every sink at configuration version 1 after this breaking change keeps
  that work clean.
- **CW-97** (Server ABI 4 port) must rebuild every sink library.
- **Recorded in the remaining-work document (2026-11-15):** the SSE pause gate
  under CW-96, gRPC TLS and HTTP sink completion under CW-44, the four sinks'
  Erase under CW-98, and the sink follow-ups (NI-01 to NI-07) as CW-101.
- **Still to record in the remaining-work document:** the SQL Server
  stored-procedure injection gap; the stored-procedure wedge-after-failure
  behaviour; the MySQL large-input cap alongside the SQL Server one under
  CW-44; the PostgreSQL delivery-table cap and leak under CW-98; the bridge
  sink and PostgreSQL storedproc driver under CW-96; per-reaction entries for
  the §15 legacy-only reactions under CW-44; the profiler non-query input
  regression. (The log `json` regression is fixed.)
