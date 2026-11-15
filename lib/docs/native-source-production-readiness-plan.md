# Native source plugins: production-readiness plan

**Status:** Proposed plan, written 2026-11-14 against `6d294f43` ("Native lifecycle
and component management (ABI 4.0)"). **Progress (2026-11-14):** the shared
network runtime, the HTTP source and the gRPC source are implemented, with
X-01, X-02 and X-09 delivered as scaffolding. See the status columns in
[§7](#7-shared-network-runtime-commonnetwork), [§8](#8-http-drasi-computation-source-http)
and [§9](#9-grpc-drasi-computation-source-grpc), and
[Implementation status](#implementation-status-2026-11-14). **Progress
(2026-11-15):** the PostgreSQL source is implemented, with decisions D-7 to
D-10 taken; see the status column in [§3](#3-postgresql-drasi-computation-source-postgres)
and [PostgreSQL implementation status](#postgresql-implementation-status-2026-11-15).
MySQL, SQL Server, SQLite, bridge and counter have not been started.
**Scope:** The eight native ComputationGraph source plugins under
`components/computation-plugins/sources/`, the shared crates they depend on, and
the native bootstrappers where a source depends on them.
**Ground rules:**

- This is a **breaking change** for native source plugins. Configuration shapes,
  defaults, element-ID formats, factory names and capability declarations may
  change freely.
- There is **no migration of existing deployments** and **no backward
  compatibility**.
- **No functionality may be lost.** Every capability of the current native
  implementation must survive, as must every capability of the legacy ordinary
  implementation (`components/sources/*`, `components/bootstrappers/*`). The only
  exceptions are those listed in [Decisions required](#2-decisions-required),
  which must be explicitly accepted or rejected.

Items from [`computation-graph-remaining-work.md`](computation-graph-remaining-work.md)
are referenced by CW ID. This plan assigns work to plugins; it does not replace
the CW entries' finish criteria. When work here closes a CW obligation, update
that entry.

Finding references use `path:line` at `6d294f43`. Several findings were
reproduced by inspection during the review; the others came from code reading.
Items marked **(reproduce first)** are plausible from the code but need a failing
test before a fix is chosen.

---

## Contents

0. [Inventory and headline findings](#0-inventory-and-headline-findings)
1. [Cross-cutting workstream (all native sources)](#1-cross-cutting-workstream-all-native-sources)
2. [Decisions required](#2-decisions-required)
3. [PostgreSQL](#3-postgresql-drasi-computation-source-postgres)
4. [MySQL](#4-mysql-drasi-computation-source-mysql)
5. [SQL Server](#5-sql-server-drasi-computation-source-mssql)
6. [SQLite](#6-sqlite-drasi-computation-source-sqlite)
7. [Shared network runtime (`common/network`)](#7-shared-network-runtime-commonnetwork)
8. [HTTP](#8-http-drasi-computation-source-http)
9. [gRPC](#9-grpc-drasi-computation-source-grpc)
10. [Bridge](#10-bridge-drasi-computation-source-bridge)
11. [Counter](#11-counter-drasi-computation-source-counter)
12. [Sequencing](#12-sequencing)
13. [Definition of done (per plugin)](#13-definition-of-done-per-plugin)
14. [Implementation status (2026-11-14)](#implementation-status-2026-11-14)
    and [PostgreSQL implementation status (2026-11-15)](#postgresql-implementation-status-2026-11-15)
15. [Appendix: remaining-work cross reference](#appendix-remaining-work-cross-reference)

---

## 0. Inventory and headline findings

### 0.1 Native sources

| Plugin crate | Factories (implementation IDs) | Code (plugin crate; shared engine) | Legacy counterpart | Hosting |
|---|---|---|---|---|
| `sources/postgres` | `drasi/postgres-transactions`, `drasi.postgres/changes`, `drasi.postgres/shared-changes`, `drasi.postgres/shared-transactions` | 3.9k; `postgres-common` | `sources/postgres` (14.4k), `bootstrappers/postgres` | Dynamic + Rust |
| `sources/mysql` | `drasi.mysql/changes`, `positional-changes`, `untracked-transactions`, `transactions`, `shared-transactions`, `shared-changes`, `shared-positional-changes` | 0.6k; `mysql-common::native` (~7.4k) | `sources/mysql` (11.1k), `bootstrappers/mysql` | Dynamic + Rust |
| `sources/mssql` | `drasi.mssql/changes` | 0.7k; `mssql-common::native` | `sources/mssql` (4.9k), `bootstrappers/mssql` | Dynamic + Rust (qualification deferred, CW-78) |
| `sources/sqlite` | `drasi.sqlite/changes`, `transactions`, `replay-transactions`, `shared-transactions` | 1.1k; `sqlite-common::native` (~3.9k) | `sources/sqlite` (8.9k, including a host-linked native component and `SqliteClientResource`), `bootstrappers/sqlite` | Dynamic + Rust |
| `sources/http` | `drasi.network/http-source` | 1.0k; `common/network`, `http-common` | `sources/http` (7.4k), `bootstrappers/http` | Dynamic + Rust |
| `sources/grpc` | `drasi.network/grpc-source` | 0.75k; `common/network`, `grpc-common` | `sources/grpc` (2.7k) | Dynamic + Rust |
| `sources/bridge` | `drasi.bridge/source` | 0.4k; `common/bridge` | none (native-origin) | **Static only; depends on `drasi-lib`** |
| `sources/counter` | `drasi.standard/volatile-counter` | 0.55k | `sources/mock` (4.2k) | Dynamic + Rust |

### 0.2 State of the ABI 4 port (all sources)

| Concern | postgres | mysql | mssql | sqlite | http | grpc | bridge | counter |
|---|---|---|---|---|---|---|---|---|
| `pause`/`resume` real or `unsupported` | ✓ gate at a transaction boundary; feedback continues | ✗ no-op, worker runs | ✗ no-op | ✗ no-op, worker+REST run | ✓ gate | ✓ gate | ✗ passive macro, admits | ✓ (no workers; still undocumented) |
| `stop` honours `StopMode` + ctx deadline | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ | n/a |
| `start` honours ctx cancellation/deadline | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ | – |
| `dispose` honours `StateIntent::Erase` | ✓ drops managed slots | ✗ | ✗ | ✗ (leaks journal tables) | ✓ declared no-op* | ✓ declared no-op* | refuses | ✗ (host refuses: capability undeclared) |
| `NativeManagement` / `Factory::assess` | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ (`plan_update` default) | ✓ (`interval_ms` only; bugs) |
| `configuration_details` | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ | ✓ |
| Secrets modelled per field | ✓ `password` only | ✗ whole `settings` secret | ✗ whole `settings` secret | n/a | ✓ tokens/TLS; webhook secrets env-var | ✓ | resource | n/a |
| Activation credentials | ✓ | ✗ (helper exists, unused) | ✗ (helper exists, unused) | n/a | n/a | n/a | n/a | n/a |
| `source_schema_reader` | ✓ | ✓ | ✓ | ✓ | ✓ webhook schema | n/a | n/a | ✗ (mock had it) |
| Reconnect / transient retry | ✓ every mode, jittered backoff, budget | ✗ (legacy had backoff) | ✗ (legacy had backoff) | n/a | n/a | n/a | sender retry only | n/a |
| Structured logging / metrics | `log`; no metrics | `log::error` only | none | `log` only | `diag!` (`log`/stderr); no metrics | `diag!` (`log`/stderr); no metrics | **none** | none |
| DESIGN + CONFIGURATION docs | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ | ✗ |
| README ABI version current | ✓ | ✗ defers to legacy README | ✗ says 2.0.0 | ✗ says 2.0.0 | ✓ | ✓ | ✗ says wire v1 (is v2) | ✓ |

\* HTTP/gRPC durable admission state is host-owned (QoS channel). Both declare
`erases_owned_state` with an idempotent no-op erase (D-1, option a). Host-side
erasure of that state is still open question [D-12](#2-decisions-required).
The HTTP and gRPC columns reflect the 2026-11-14 implementation and the
PostgreSQL column the 2026-11-15 implementation; the other columns describe
`6d294f43`.

### 0.3 Highest-risk findings (production blockers)

1. ~~**PostgreSQL leaks replication slots and requires unbounded WAL retention.**~~
   *Resolved 2026-11-15 (PG-01, PG-02, PG-09).*
   Startup requires `max_slot_wal_keep_size = -1`
   (`postgres-common/src/replication.rs:434-437`). `dispose` never drops managed or
   auto-created slots, so every removed source eventually fills the database
   server's disk. Slot creation also *adopts* an existing slot by matching the
   error text "already exists" (`postgres-common/src/connection.rs:348-360`), so
   two sources sharing the default `drasi_slot` silently share one slot.
2. ~~**PostgreSQL TRUNCATE causes a permanent replay failure loop**~~
   *Resolved 2026-11-15 (PG-04: refused at start).*
   (`replication/transaction.rs:172` bails; every restart replays the same commit).
3. **MySQL has no reconnect and fences on unrelated DDL.** Any worker error ends
   replication (`mysql-common/src/native/source.rs:947-957`). Any non-`CREATE`
   statement in the configured database fences the source, including ALTER on
   unselected tables and ANALYZE/OPTIMIZE (`native/binlog.rs:595-606`). The
   legacy source tolerated both.
4. **Every source except counter reports a silent successful Pause while its
   workers continue** (CW-96). HTTP and gRPC were fixed on 2026-11-14. That is a contract violation
   (`computation-plugin-sdk/src/component.rs:85-88`, `lib/AGENTS.md`).
5. ~~**HTTP/gRPC ingress has no authentication, TLS or rate limiting, and binds
   `0.0.0.0` by default**~~ (`common/network/src/config.rs:36-38`, CW-44).
   **Resolved 2026-11-14** by NET-03 and NET-04.
6. ~~**Legacy transparent durability is lost for HTTP/gRPC.**~~ Legacy WAL-backed
   standard routes, webhooks and gRPC RPCs became durable without client changes.
   **Resolved 2026-11-14** by NET-05: a server-sequenced producer restores this,
   with at-least-once delivery.
7. **SQLite possible stop deadlock (reproduce first).** While output is pending,
   the worker stops taking commands
   (`sqlite-common/src/native/worker.rs:1194-1196`). `stop` joins REST before it
   signals database shutdown (`runtime.rs:205-216`). An accepted HTTP write could
   therefore hold stop until timeout, and every retry repeats the wait.
8. **SQLite leaks `__drasi_native_*` journal tables** into the user's database on
   removal. Re-provisioning a source on the same file then fails.
9. **SQL Server rejects whole tables containing `json`, `xml` and other
   unmapped column types** (`mssql-common/src/native/catalog.rs:283-299`). It
   reconnects and re-reads the entire catalog every poll, caps a poll at 32
   change groups (`paging.rs:15`), and has no retry.
10. **The bridge source breaks the native dependency rule.** It and
    `common/bridge` depend on `drasi-lib` and implement the in-process
    `ComputationComponent` contract, not the plugin SDK. Server cannot load it
    and it has no real-library coverage.
11. **Counter cannot represent `sources/mock`.** Mock offers sensor and generic
    generators, update and relation events, a schema and bootstrap. No native
    equivalent exists, which is a functionality loss.

---

## 1. Cross-cutting workstream (all native sources)

Do these first. Doing them inside each plugin separately would produce eight
divergent implementations. IDs are `X-nn`.

### X-01: Native source lifecycle conformance standard (P0)

**Status: Done.** Written as `components/computation-plugins/sources/NATIVE-SOURCE-GUIDE.md`.

Write a short normative section, or a new `components/computation-plugins/sources/NATIVE-SOURCE-GUIDE.md`,
that turns the ABI 4 contract into source-specific rules. Every plugin section
below then implements it.

| Hook | Required source behaviour |
|---|---|
| `initialize` | Validation that can be done without I/O against external systems. Examples: secret/credential presence, path parents, route collisions. No workers, no listeners. |
| `start` | Observe `ctx.cancellation()` and `ctx.deadline()` across *every* await, including `progress.wait_ready()`. Never use a plugin config timeout as the overall start budget. Partial start leaves a retry-safe cleanup guard. |
| `pause` | **Either** a real gate (no new admission or external reads, in-flight work settled, bounded memory, liveness keepalives maintained) **or** `lifecycle::unsupported(...)`. Never a silent `nothing_to_do()` while workers run. Host-pulled sources with no independent worker may return success, but must document why. |
| `resume` | Reopen exactly what pause closed. Handle upstream timeouts that fired during the pause (re-establish from the last committed position). |
| `stop` | `StopMode::Drain`: finish in-flight units (frame, transaction, request) within `ctx.deadline()`, then graceful close (final feedback, Terminate). `StopMode::Abort`: cancel promptly. On timeout, retain ownership (`join_owned_worker_gracefully` for servers, retained handle otherwise). Do not report an earlier *processing* failure as a stop failure; surface it once through `next()`/status. |
| `dispose` | Implement both intents. Under `Erase`, remove exactly the state the component owns (slots, journal tables, producer registrations), never borrowed or shared state. Erase must be idempotent: "already gone" counts as success. Declare `erases_owned_state` iff Erase does something; see [D-1](#2-decisions-required) for components that own nothing. Fence schema readers in `dispose`, not only in `Drop`. |

### X-02: Shared lifecycle conformance test kit (P0)

**Status: Partial.** `drasi_computation_plugin_sdk::testing` (feature
`test-support`) provides `SourceConformance` with these checks:

- lifecycle: stop before start, double start, pause refusal or `unsupported`,
  resume, Drain, restart and Abort;
- cancelled start;
- erase twice;
- configuration round-trip with published defaults;
- the assess matrix;
- live apply at the Pause and Stop boundaries.

HTTP and gRPC run it on both Tokio flavours.

Still to add:

- stop timeout followed by a successful retry;
- the full `DisposeReason` matrix, and dispose after initialization failure;
- refusal of a stale or forged plan;
- running the kit through the freshly built dynamic library, not only the
  linked `plugin()`.

Add a test-support module, in `computation-plugin-sdk` behind a `test-support`
feature or in a new dev crate, that runs a standard scenario matrix against any
source factory. Run it through both the linked `plugin()` definition and the
freshly built dynamic library, on both Tokio flavours (CW-41, CW-37):

- start → stop → start on the same object; stop without start; stop after a
  failed start; double start refused;
- pause gates output and admission (when supported) or returns `unsupported`;
  resume restores flow; pause → stop;
- `StopMode::Abort` vs `Drain` observable difference; stop timeout followed by a
  successful retry;
- start cancellation and deadline;
- dispose for each `DisposeReason`; `Erase` twice (CW-98); dispose after
  initialization failure;
- `configuration()` round-trips into `create` and produces an equal
  `configuration()`;
- `Factory::assess` matrix: a table of (field, change) → expected boundary,
  supplied by each plugin; `apply` exposes the proposed configuration; a stale or
  forged plan is `Refused`;
- metadata/schema/`configuration_details` consistency: every published default
  equals the serde default.

Each plugin supplies only a fixture (how to provision its external system) and
its assess table.

### X-03: Configuration model, secrets and defaults (P0)

**Status:** Applied to HTTP and gRPC using the flattening option of D-2:

- tokens and TLS material are top-level `secret` fields;
- `SecretString` redacts its `Debug` output;
- the SDK gained `ConfigOption::secret()`.

Exception: HTTP webhook secrets inside `webhooks.routes[].auth` still name
environment variables, because they are nested. Flattening them needs a
secret-map design (HT-04).

Problem: native configuration references are resolved **per top-level field**,
and a field marked `secret` must be a secret reference
(`lib/src/computation/v1/graph/specification.rs:135-170`). The database sources
therefore mark their whole nested `settings` object secret. Operators must put
host, tables and limits into a secret store, and non-secret configuration cannot
be inspected or diffed.

Work:

1. Decide the mechanism ([D-2](#2-decisions-required)). Either flatten every
   source's configuration so each secret (password, token, CA key) is a
   top-level field marked `secret`, or extend the host specification resolver to
   accept references at nested JSON-pointer paths. Flattening is the smaller
   change and is recommended.
2. Publish full nested schemas. No `Object`-typed opaque blobs for `settings`,
   `rest_api`, `webhooks` or `limits`.
3. Implement `configuration_details` everywhere, and give every operational
   bound a serde default equal to its published default. Required fields are
   only those with no safe default (database, user, stream, tables policy).
4. `#[serde(deny_unknown_fields)]` on every configuration struct, including
   nested connection structs (`mssql-common/src/config.rs:150` lacks it).
5. Redacting `Debug` for every struct containing a password
   (`postgres-common/src/config.rs:63`, `replication.rs:38`,
   `mssql-common/src/config.rs:150`). MySQL already redacts it.
6. `configuration()` must equal the constructed configuration: MySQL drops
   `persistent` (`mysql-common/src/native/source.rs:868-870`). Once activation
   credentials exist, it should not include credentials.
7. Activation credentials for the database sources: implement
   `supports_activation_credentials` / `activation_credential_context`. MySQL and
   MSSQL already have `validate_with_*_credentials` helpers. This lets host
   identity providers (Entra, AWS IAM tokens) and rotation work without
   persisting secrets.

### X-04: Update planning and management (P0)

**Status:** Done for HTTP and gRPC. The shared `BoundaryPlanner` helper is in
`common/network/src/update.rs`.

For every factory, implement `supports_update_planning` + `Factory::assess`
(pure, configuration-only). Wherever a Pause or Stop boundary is meaningful,
also implement `NativeManagement`. Declare `management` and `update_modes`
accordingly. Each plugin section contains a field classification table.
General rules:

- Fields that participate in a recovery binding hash or checkpoint identity
  require **Recreate**. Their data treatment is the plugin's decision: usually
  `Rebootstrap`, or `Unsupported` where reset safety is not established (CW-92).
- Credentials, timeouts, limits and endpoints that do not change upstream
  identity: **Restart** (Stop boundary).
- **InPlace** (Pause) only for settings read per request or per poll, and only
  once pause gating is real.
- A Recreate of a component that owns external state must go through `Erase`
  disposal, otherwise state leaks (PostgreSQL slots, SQLite journal tables).

### X-05: Typed errors and failure classification (P1)

All sources use prose `anyhow::ensure!`. Introduce a per-family error enum
carried inside `anyhow` and downcastable at the boundary, following the
`lib/AGENTS.md` error-layering rule. Its variants: `Config`, `Transient` (retry
with backoff), `PositionUnavailable` (retention gap, binding change: typed for
recovery policy), `SchemaChanged`, `Unsupported` (server or version), `Auth`,
`Fenced`. Classify transient errors by SQLSTATE or error code, not by message
text or `Elapsed` (`sources/postgres/src/rows.rs:267-289`). Preserve causes
(`with_cause`); no `error.to_string()` re-wrapping (`common/bridge/src/receiver.rs:519-617`).
This is CW-22 for sources.

### X-06: Reconnect and supervision policy (P0 for CDC sources)

Define one shared policy, for example a `Backoff` helper in a small shared crate
or in the SDK: bounded exponential backoff with jitter, a configurable maximum
number of attempts or `unbounded`, cancellation-aware, reporting `not_ready`/status
while retrying, and resuming from the last committed position. Apply it to
PostgreSQL (all modes), MySQL and SQL Server. Positional, untracked and volatile
modes must document what a reconnect can lose. Add a dead-peer watchdog for
streaming protocols (MySQL binlog, PostgreSQL replication).

### X-07: Observability (P1)

**Status:** HTTP and gRPC replaced `eprintln!` with `diag!`. Spans, readiness
through `ControlSender` and metrics remain open (NET-07, NET-13, NET-14).

- Replace `eprintln!` (14 sites in network code) and the absence of logging
  (bridge, mssql) with `tracing` spans carrying instance/component IDs.
  Rate-limit per-request logs.
- Report readiness, retry and failure through the `ControlSender`. Today every
  factory drops it (`_control`).
- Metric set per CW-26: events, transactions, bytes, lag (LSN, binlog position or
  commit-time delta), reconnects, admission rejects, queue occupancy, blocked
  admission. Names and units are shared across sources.

### X-08: Hygiene common to every plugin (P2)

- Remove `.expect()` from `Factory::metadata()` paths, which are reachable from
  the FFI exporter in every plugin (for example `sources/postgres/src/lib.rs:79,124,181`
  and `sources/mssql/src/lib.rs:47,51,55`). Build metadata fallibly or in a
  `LazyLock` validated by a test.
- Cache constant `PortId`s instead of `PortId::try_new("out")` per event.
- Name the magic constants.
- Add crate-level `//!` docs and doc comments on all public items and
  configuration fields.

### X-09: Documentation standard and templates (P0 for release)

**Status: Partial.** `NATIVE-SOURCE-GUIDE.md` contains the native standard and
the `DESIGN.md`/`CONFIGURATION.md` templates. HTTP and gRPC follow it. The
stale text is fixed in `NETWORK.md`, `computation-plugins/README.md` and
`common/network/README.md`. The other plugins' docs and READMEs remain.

`components/sources/source-documentation-standards.md` targets the ordinary
`SourceBase` family. Add a native section or companion standard and two
templates. Every plugin then gets:

- **`DESIGN.md`**: purpose and scope; architecture (workers, connections,
  listeners); change-capture mechanism; data mapping (labels, element-ID format,
  type mapping table); ordering, transaction and checkpoint/cursor model;
  bootstrap handover; recovery profile and durability; owned vs borrowed
  external state; lifecycle semantics per hook; failure model and retry; security
  model; known limitations; legacy differences.
- **`CONFIGURATION.md`** (configuration and management): every field (type,
  default, range, required, secret, update boundary); example per factory;
  external-system prerequisites (privileges, server settings, versions);
  credentials and secret references; update/assess matrix; removal and Erase
  behaviour; operational runbook (monitoring, retention, troubleshooting).
- `README.md` reduced to a short overview linking the two.
- Fix stale version text everywhere it appears: `NETWORK.md:6-11`, the
  postgres/mssql/sqlite READMEs and the bootstrapper READMEs (ABI 2.0.0), bridge
  (wire v1 → v2), and `common/network/README.md:26-30` (a non-existent
  `SourceConfig`/`parse` API).

### X-10: Test infrastructure (P1)

- Wire every source's container suite into explicit CI jobs (CW-85). SQL Server
  needs an approved environment (CW-78).
- Fix CW-99: MySQL `shared::` multi-thread deadline failures; expired Postgres
  TLS fixtures; exclude the `postgres_abrupt_process_exit` helper from `--ignored`.
- Reduce fixture cost (CW-87). MySQL's matrix takes about 1,020 s for 31 cases.
- Port legacy integration assertions to native construction (CW-41), per plugin
  below.
- Migrate connector tests off the linked query factory so they count as native
  evidence (CW-77).
- Crash-restart (SIGKILL) matrix per recovery-capable mode (CW-14).

### X-11: Untrusted-input limits (P1; CW-19, CW-29)

**Status:** HTTP and gRPC enforce size limits. Depth limits remain open
(NET-10).

Apply nesting-depth and size limits to JSON, XML, YAML and protobuf `Struct`
decoding (HTTP, gRPC, bridge), and to JSON column decoding in the database
sources. Define behaviour on a full buffer and on an oversized input.

---

## 2. Decisions required

These change scope. Each needs an explicit owner decision before its dependent
tasks start.

| ID | Decision | Options | Recommendation | Blocks |
|---|---|---|---|---|
| D-1 | How do components that own **no** external state handle `Erase` disposal? Today the host fails Erase before the plugin runs when `erases_owned_state` is undeclared (CW-98). Removal-with-erase of counter, MySQL, MSSQL, HTTP or gRPC therefore always fails. | (a) Declare `erases_owned_state` with a vacuous idempotent erase; (b) host accepts Erase as a no-op for components declaring "owns nothing"; (c) refuse at management acceptance. | (b), with an explicit `owned_state: None\|Erasable` capability; otherwise (a). | X-01, every dispose task |
| D-2 | Secret modelling for nested configuration. | Flatten secrets to top-level fields, or add nested-path references to the host resolver. | Flatten. | X-03, every config task |
| D-3 | **Bridge hosting.** | Port to the plugin SDK and ABI 4 with dynamic export (TLS by recipe, admission via `create_with_admission`, seal/drain via control), or record static-only as a supported profile. | Port. Today it violates the native dependency rule and cannot be loaded by Server. | BR-* |
| D-4 | **Mock parity.** | Extend counter into a native generator source (counter, sensor, generic; update and relation events; schema; bootstrap), or add a separate `drasi.standard/mock` plugin and keep counter minimal. | A separate native mock plugin. Counter stays a minimal reference implementation. | CT-*, MK-* |
| D-5 | **Transparent (server-sequenced) durability for HTTP/gRPC.** Legacy acknowledged after WAL append on unchanged routes. | Implement an "implicit producer" durable mode in `common/network` that registers its own producer and serves standard routes, webhooks and the old RPCs; or accept the loss. | Implement. Without it, durable third-party webhooks are impossible. | NET-05 |
| D-6 | **SQLite SQL/DDL handle for loaded plugins.** The legacy and host-linked versions offered arbitrary SQL, DDL and callback transactions (`SqliteClientResource`); the cdylib offers REST CRUD only. | An authenticated SQL route plus an `init_sql` configuration, a native service resource, or a documented exception. | `init_sql` plus an optional authenticated `/api/sql` route. Reject `path: null` in loaded mode until one exists. | SQ-05 |
| D-7 | **Schema evolution policy for CDC sources** (Postgres binding hashes every column; MySQL fences on DDL; MSSQL fails on capture-instance changes). | Fence only on changes to selected tables or key columns, with a typed `SchemaChanged` error and an assess-driven Recreate with Rebootstrap; or narrow binding hashes to key columns and types; or both. | Both. | PG-08, MY-02, MS-10 |
| D-8 | **PostgreSQL `max_slot_wal_keep_size = -1` requirement.** It excludes managed services that forbid it and amplifies slot leaks. | Keep, with Erase and monitoring; or allow a finite value and detect `wal_status = lost` as a typed `PositionUnavailable`. | Allow a finite value with detection; keep `-1` as the recommended profile. | PG-09 |
| D-9 | **PostgreSQL TRUNCATE semantics.** | Reject `pubtruncate = true` at startup; or emit deletes for the known keys of affected tables; or emit a typed reset requiring Rebootstrap. | Reject at startup now (P0); add a typed reset with rebootstrap later. | PG-04 |
| D-10 | **Keyless tables** (legacy Postgres used UUID fallback IDs; native rejects them). | Keep the rejection (documented); or opt-in `REPLICA IDENTITY FULL` whole-row identity. | Keep the rejection and document it. Opt-in full-row identity is P2. | PG-10 |
| D-11 | **MariaDB** (native explicitly rejects it; legacy was unqualified). | Document as unsupported or qualify it. | Document as unsupported. | MY-21 |
| D-12 | **Host erasure of durable admission state.** Does removing a durable HTTP, gRPC or bridge source erase its QoS producer registry and journal? No evidence was found in `lib/src`. | Confirm and test, or add it. | Confirm; if absent, add host-side erase and treat it as owned-by-host. | NET-08, BR-03 |
| D-13 | **Pause semantics for ingress sources.** | Reject fast (503 with `Retry-After`, `UNAVAILABLE`, `Busy`), or block up to `timeoutMs`. | Reject fast, so pause does not extend the host's pause deadline. | NET-01 |

**Decisions taken (2026-11-14), for HTTP and gRPC only:**

- **D-1:** option (a). Both declare `erases_owned_state` with an idempotent
  no-op erase; dispose refuses while the source is still running. The host-side
  alternative (b) remains open for the other plugins.
- **D-2:** flatten. Tokens and TLS material are top-level secret fields.
  Webhook secrets remain environment variables pending HT-04.
- **D-5:** implemented as NET-05.
- **D-13:** reject fast.
- **Still open:** D-12 (host erasure of durable admission state) and NET-08.

**Decisions taken (2026-11-15), for PostgreSQL:**

- **D-1:** not applicable: the source owns slots, so it declares
  `erases_owned_state` with a real erase.
- **D-2:** flatten. `password` is the only secret field; activation
  credentials are supported.
- **D-7:** both options. The binding (format version 2) covers only key
  columns, key column types, replica identity, publication operations and row
  filters of the selected tables, so non-key column changes no longer fence the
  cursor. Changes to bound facts, and a table joining an all-tables
  publication, fail with a typed `SchemaChanged`. Instead of an assess-driven
  Recreate with Rebootstrap, the documented action is removal with erase and
  re-addition (see PG-05).
- **D-8:** a finite `max_slot_wal_keep_size` is allowed with a warning; an
  invalidated slot (`wal_status = lost`, or a slot that can no longer stream)
  fails with a typed `PositionUnavailable`. The `idle_replication_slot_timeout
  = 0` requirement was dropped for the same reason.
- **D-9:** a publication that publishes TRUNCATE is refused at start; a
  TRUNCATE published later fails with a typed `Unsupported`. The typed reset is
  a follow-up (CW-102).
- **D-10:** keyless tables stay rejected and are documented.

---

## 3. PostgreSQL (`drasi-computation-source-postgres`)

### 3.1 Current state

- Four factories (§0.1), all using `Capabilities::default()` (`sources/postgres/src/lib.rs:280`).
- Configuration is `{stream, settings, coordinated_snapshot}`, plus row options:
  `persistent`, `auto_create_slot`, `start_from_slot`, `reconnect_attempts`
  (default 1, max 32) and `reconnect_delay_ms`. Row defaults are duplicated at
  `lib.rs:166-169`, `lib.rs:246-249` and `rows.rs:31-40`.
- `settings` is a `PostgresTransactionConfig`. Its required fields have no
  defaults: `start_lsn`, `transactions`, `max_protocol_bytes`, `io_timeout_ms`,
  `feedback_interval_ms`.
- Implemented SDK surface: `recovery_reader`, `replay_retention`, and
  `consumer_progress_reader`/`input_position_policy` for the shared modes.
- **Missing:** `source_schema_reader`, management, assess,
  `configuration_details`, activation credentials and bootstrap coordination.
- The data path is strong: bounded transaction builder, binding-checked
  checkpoints, verified TLS (better than legacy, whose replication stream was
  plaintext), and SCRAM authentication.

### 3.2 Lifecycle and management tasks

| ID | Pri | Task | Status |
|---|---|---|---|
| PG-01 | P0 | **Erase and slot ownership.** Declare `erases_owned_state`. In `dispose(Erase)`, drop the managed coordinated slot (from the committed position's `managed_slot`, `replication.rs:128-131`, read via `recovery_reader`) and any slot the source provably created. Never drop configured or external slots (shared modes, `start_lsn` mode). Treat SQLSTATE 42704 ("undefined object") as success; refuse while the slot is active; keep it retry-safe. Replace `auto_create_slot` with explicit `slot_ownership: managed \| external`, and persist the ownership marker. | Done: `slotOwnership` managed/external; managed names derived from graph and component IDs, so no persisted marker is needed; Erase drops every slot with the prefix, refuses active ones, treats 42704 as success |
| PG-02 | P0 | **Stop adopting slots by text match.** `create_replication_slot` (`postgres-common/src/connection.rs:348-360`) must use SQLSTATE 42710 and fail unless ownership is established. Remove the shared default `slot_name: drasi_slot` (`config.rs:113-115`): require it, or derive it from the component UUID. | Done: catalog lookup plus SQLSTATE 42710; no default slot name |
| PG-03 | P0 | **Pause.** Gate `run_connected` behind a paused watch: stop reading CopyData, but keep sending standby status so `wal_sender_timeout` does not fire. Otherwise return `unsupported`. Also fix the legacy-side wrapper `components/sources/postgres/src/native.rs`, which uses `passive_component_lifecycle!` for pause. | Done, including the legacy in-process wrapper |
| PG-05 | P0 | **Management.** `Factory::assess` with `update_modes = [Restart, Recreate]` and `NativeManagement` for Stop-boundary apply (classification below). A Recreate must route through Erase so it does not leak slots. | Done with a deviation: bound fields are Unsupported (remove with erase and re-add) rather than Recreate with Rebootstrap, which this source cannot coordinate; `update_modes = [InPlace, Restart]` |
| PG-06 | P1 | **Context-aware lifecycle.** Select on cancellation and deadline in `start`, including the unbounded `wait_ready()` (`component.rs:637,643`). `StopMode::Drain` sends final standby feedback then Terminate (legacy did this); `Abort` drops. Stop must not re-raise an earlier worker failure (`join(true)?`, `component.rs:684`). | Done: connection setup bounded by `ioTimeoutMs`; readiness (which can include a coordinated snapshot) bounded only by the host deadline; a failed start still requires stop |

Field classification for PG-05:

| Boundary | Fields |
|---|---|
| Restart (same checkpoint) | `io_timeout_ms`, `feedback_interval_ms`, `max_protocol_bytes`, `transactions.*` (increases only), reconnect settings, `user`/`password` (via credentials), `ssl_mode`, `tls_ca_pem`, `host`/`port` (the binding check enforces the same system identifier at start) |
| Recreate + Rebootstrap | `stream`, `slot_name`, `publication_name`, `tables`, `table_keys`, `database`, `coordinated_snapshot`, `persistent`, factory kind |
| Recreate, before first checkpoint only | `start_lsn` |
| InPlace | none until PG-03 is real |

### 3.3 Defects and functional gaps

| ID | Pri | Task | Status |
|---|---|---|---|
| PG-04 | P0 | **TRUNCATE.** Today `WalMessage::Truncate` bails (`replication/transaction.rs:172`) and the commit replays forever. Reject publications with `pubtruncate = true` at startup with guidance (`replication.rs:377-380` reads but ignores the flag). Later, a typed reset per D-9. | Done (refused at start; runtime typed failure) |
| PG-07 | P1 | **`source_schema_reader`.** Port legacy `introspect_postgres_schema` (`sources/postgres/src/lib.rs:464,902`) into `postgres-common` over the existing catalog queries (CW-44 discovery). | Done (`postgres-common::replication::schema`) |
| PG-08 | P1 | **Schema drift (D-7).** ALTER TABLE changes the binding hash (`replication.rs:395-410`), so the next start fails with a binding mismatch. A new table in a `FOR ALL TABLES` publication fails at runtime (`transaction.rs:110-113`). Implement the chosen policy with a typed `SchemaChanged` and an assess-driven Recreate. | Done (D-7 as taken) |
| PG-09 | P1 | **Retention (D-8).** Handle NULL `max_slot_wal_keep_size` on PostgreSQL ≤12 with a clear version error (`replication.rs:420-437`). Implement the chosen finite-retention mode and detect `wal_status = lost`. | Done (D-8 as taken; servers older than 13 refused with `Unsupported`) |
| PG-10 | P2 | Keyless-table decision (D-10); document it. | Documented |
| PG-11 | P0 | **Reconnect (X-06)** for all four modes. Transaction modes have none; rows mode uses a fixed delay (`component.rs:921-923`). | Done: every mode, including both transaction modes |
| PG-12 | P1 | **Activation credentials and secret modelling (X-03):** a top-level `password` secret, a non-secret `settings`, a redacting `Debug`. | Done |
| PG-13 | P1 | **Bootstrap coordination.** Adopt `supports_bootstrap_coordination` / `create_bootstrap_coordinator` (as SQLite does), so the source and bootstrapper share one configuration and secret rather than duplicating `settings` (`bootstrappers/postgres/src/lib.rs:39-45`). The element-ID and type mapping must stay shared through `postgres-common` (components/AGENTS.md). | **Open**: shared configuration and slot identity only; coordinator API not adopted (CW-102) |

### 3.4 Code quality

| ID | Pri | Task | Status |
|---|---|---|---|
| PG-20 | P2 | Replace the `Option` combination mode model (`progress`, `shared`, `rows`, `coordinated_snapshot`; `component.rs:49-62`, `777-788`) with an enum over the four modes. This removes the runtime "invalid binding" bails. | Partial: a `Mode` enum drives configuration and planning; the component keeps its progress bindings |
| PG-21 | P2 | Deduplicate: the slot-retention check (four copies: `component.rs:241-272`, `360-397`, `506-514`, `1032-1044`), `Worker` construction (three copies) and the constructors. Split `component.rs` (1,148 lines): `prepare`, `reopen`, `run_connected`. | Done: one retained-cursor check, worker split into `worker.rs` |
| PG-22 | P2 | Central SQL literal/identifier quoting helper instead of ad hoc `format!` and `replace('\'', "''")` (`replication.rs:292,369,389,396`; `component.rs:761-771`). | Done (`quote_literal`, `quote_identifier`) |
| PG-23 | P2 | Named constants for protocol offsets (`component.rs:1117,1132`), the SSLRequest bytes (`replication.rs:250`) and the PostgreSQL epoch (`connection.rs:516`). | Done |
| PG-24 | P2 | Typed errors (X-05) replacing downcast/`Elapsed`-based `transient()`. | Done (`PostgresCdcError`, `is_transient`) |
| PG-25 | P2 | Centralise row defaults; `configuration_details`; serde defaults for the currently required operational fields. | Done |

### 3.5 Legacy parity

| Legacy capability | Native | Action |
|---|---|---|
| Per-field `ConfigValue` env/secret references | Whole-object secret | X-03 / PG-12 |
| `describe_schema` | Missing | PG-07 |
| Reconnect on any error | Rows only, limited | PG-11 |
| TRUNCATE tolerated (warn) | Fatal loop | PG-04 |
| DDL and publication changes tolerated | Fatal | PG-08 |
| Final feedback on shutdown | Missing | PG-06 |
| PostgreSQL ~10+ | 13+ (coordinated mode 15+) | PG-09; document |
| Keyless tables (UUID IDs) | Rejected | D-10 |
| Slot create-or-reuse | Explicit ownership | PG-01/02 (intentional) |
| Element ID `table:k1_k2` | `pg:v1:[…]` | Intentional break; document |

### 3.6 Tests

| ID | Pri | Task | Status |
|---|---|---|---|
| PG-30 | P0 | Run the X-02 conformance kit (static and dynamic, both flavours). | Partial: the live suite runs static and built libraries; the SDK kit is not wired up (CW-102) |
| PG-31 | P0 | Erase: managed slot dropped, external slot preserved, idempotent double erase, active-slot refusal, slot-name collision refused. | Done |
| PG-32 | P0 | TRUNCATE rejection; ALTER TABLE then restart; new table in a FOR ALL TABLES publication. | Done |
| PG-33 | P1 | Reconnect for every mode with fault injection mid-transaction; classification of 55006 and 57P01 against a real server. | Partial: walsender termination (57P01) tested for transaction mode; rows and shared rows covered by existing tests; mid-transaction fault injection and 55006 not added |
| PG-34 | P1 | Pause longer than `wal_sender_timeout`; activation credentials through the dylib; schema reader; PostgreSQL ≤12 error. | Partial: long pause and schema reader tested; activation credentials through the library and the PostgreSQL 12 refusal not (CW-102) |
| PG-35 | P1 | Fix the CW-99 TLS fixtures and the crash helper. Add rows and shared modes to the CW-14 crash matrix. Strengthen the CW-90 getting-started assertions. | Partial: TLS and crash-helper tests fixed (CW-99); crash matrix and CW-90 not |

### 3.7 Documentation

| ID | Pri | Task | Status |
|---|---|---|---|
| PG-40 | P0 | `DESIGN.md`: modes, cursor and binding model, feedback, bootstrap handover, slot ownership, failure semantics, unsupported features. `CONFIGURATION.md`: every field; PostgreSQL prerequisites by mode and version (`wal_level`, REPLICATION role, publication, `max_slot_wal_keep_size`, REPLICA IDENTITY); secrets and credentials; update matrix; erase behaviour; WAL-retention and slot runbook. Fix the ABI text (README:4). The field reference must no longer live only in the legacy README. | Done |

**Related CW:** 96, 98, 44 (discovery), 41, 14, 17, 99, 90, 83, 35, 39, 18, 22, 26.

---

## 4. MySQL (`drasi-computation-source-mysql`)

### 4.1 Current state

- Seven factories over one engine in `mysql-common::native`. Configuration is
  `{stream, settings[, persistent]}`. Every `settings` field except `table_keys`
  is required, and none has a default.
- Strengths: native TLS with an optional CA (stronger than legacy),
  caching_sha2 with RSA, binding-checked cursors, binlog retention verification,
  bounded channels, and the worker joined on stop.
- Missing: reconnect, DDL tolerance, management, credentials,
  `configuration_details`, and real pause.

### 4.2 Lifecycle and management tasks

| ID | Pri | Task |
|---|---|---|
| MY-03 | P0 | **Pause (CW-96).** Return `unsupported` now. Later, a gate that stops reading frames, plus a planned disconnect and resume-from-position for pauses longer than `net_write_timeout`. Today a long pause probably drops the dump connection, and resume then fails with "replication ended" (inferred; reproduce first). |
| MY-04 | P0 | **Dispose:** retire the schema publication in `dispose` (today only in `Drop`, `source.rs:960-969`). Assert the worker is absent. Erase per D-1, since nothing server-side is owned. Add a double-dispose test. |
| MY-07 | P0 | **Management.** Assess (pure) with `update_modes = [Restart, Recreate]` plus `NativeManagement`. The binding hash (`source.rs:1058-1092`) defines the split below. |
| MY-09 | P1 | **Context-aware lifecycle:** use the start deadline and cancellation instead of `io_timeout_ms` for the whole startup (`source.rs:899-901`), and honour `StopMode`. Split `io_timeout_ms`, which today is the packet timeout, startup budget, stop join budget and history-scan budget at once, into separate settings. |

Field classification for MY-07:

| Boundary | Fields |
|---|---|
| Restart (recovery preserved) | `user`/`password` (credentials), `ssl_mode`, `tls_ca_pem`, `host`/`port` (same server UUID enforced), `server_id`, `heartbeat_interval_ms`, timeouts, `max_protocol_bytes`, `transactions.*` |
| Recreate | `database`, `tables`, `table_keys`, `start`, `stream`, `output`, `replay`, `persistent`, factory kind (positional vs FULL) |

### 4.3 Defects and functional gaps

| ID | Pri | Task |
|---|---|---|
| MY-01 | P0 | **Reconnect (X-06).** Classify errors as transient or permanent. On transient errors, re-run `prepare` from the last committed position: confirmed checkpoints for tracked modes, last commit file/offset for untracked modes. Report "retrying" status. This restores legacy behaviour (`sources/mysql/src/stream.rs:58-174`). |
| MY-02 | P0 | **DDL tolerance (D-7).** Ignore statements that do not touch selected tables, including administrative statements (`binlog.rs:595-606`). For FULL-metadata untracked modes, re-inspect the catalog on ALTER of a selected table. Tracked modes fence only selected-table changes, with typed `SchemaChanged`. |
| MY-05 | P0 | **Credentials and secret modelling (X-03):** use the existing `validate_with_activation_credentials` (`mysql-common/src/native/mod.rs:52-87`); stop marking all of `settings` secret. |
| MY-06 | P0 | **`configuration()` correctness:** include `persistent` for the shared-row factories (`source.rs:868-870`); round-trip test for all seven factories. |
| MY-08 | P1 | **Dead-peer watchdog.** No frame (heartbeats included) for k × `heartbeat_interval_ms` triggers a transient error and reconnect. Today an idle stream never times out (`connection.rs:245-256`). |
| MY-10 | P1 | **`server_id` auto-derivation** from the component UUID when omitted (legacy parity, `sources/mysql/src/descriptor.rs:239-247`). Detect duplicate-`server_id` disconnects with a clear error. |
| MY-11 | P1 | **Defaults:** `configuration_details` with legacy-equivalent defaults (port 3306, `ssl_mode`, heartbeat, transactions, `max_protocol_bytes`, timeouts, `start: end`). Warn when `tables` is empty (legacy did). |
| MY-12 | P1 | **Database-wide robustness:** when `tables` is empty, skip with a warning any table that has an unsupported name, type or charset, instead of aborting startup (`catalog.rs:167-189,493-525`). Relax identifier validation using `quote_identifier`. Use prepared statements for catalog queries (`catalog.rs:133-160,236-243`). |
| MY-13 | P1 | **Type coverage:** spatial types (WKB/WKT), VECTOR, and non-UTF-8 charsets via `encoding_rs` (already used by the positional decoder). Per-type tests against a live server. Verify the snapshot session time zone is pinned to UTC (legacy pinned it). |
| MY-14 | P1 | **Status and tracing:** spans carrying instance and component ID; status messages for connect, retry and fence (legacy had them). |
| MY-15 | P2 | **GTID-carrying checkpoints** for failover-portable resume (legacy carried `current_gtid`). |
| MY-16 | P2 | **Bootstrap coordination** via the SDK, to stop duplicating `settings` and credentials in the bootstrapper. |
| MY-21 | P2 | Document MariaDB (D-11) and the lack of mTLS client certificates. |

### 4.4 Code quality

| ID | Pri | Task |
|---|---|---|
| MY-17 | P2 | Make the seven factories in `lib.rs` table-driven (they repeat validation, deserialization and scope checks, `lib.rs:117-376`). Collapse the ~8 `MySqlSource` constructors and flag soup into a mode enum or builder (`source.rs:55-211`). |
| MY-18 | P2 | Split `Binlog::decode_inner` (~380 lines, `binlog.rs:383-766`), `prepare_connection` (~290 lines) and `catalog::read_tables`. Replace anonymous tuples with named structs. |
| MY-19 | P2 | Feature-gate `mysql_async`/rustls out of the native build and keep one TLS stack, or fix the documentation that claims rustls (`mysql-common/Cargo.toml:52-54,65-68`; `config.rs:19-32`). Track CW-66 (RSA, zstd advisories). |
| MY-20 | P2 | Typed errors (X-05); named constants; `RwLock<Option<JoinHandle>>` → plain field. |

### 4.5 Legacy parity

| Legacy capability | Native | Action |
|---|---|---|
| Reconnect with exponential backoff (10 attempts, 5–60 s) | None | MY-01 |
| DDL ignored | Fatal | MY-02 |
| Auto `server_id` | Required | MY-10 |
| Per-field defaults and `ConfigValue` | None / whole-object secret | MY-05, MY-11 |
| GTID resume | Initial start only | MY-15 |
| Lenient type decoding | Strict whitelist | MY-13 |
| Any table name | `[A-Za-z0-9_]` only | MY-12 |
| Status and tracing span | `log::error` | MY-14 |
| Lock-free bootstrap snapshot | Standalone lock-free; coordinated mode uses FTWRL | Document |

### 4.6 Tests

| ID | Pri | Task |
|---|---|---|
| MY-30 | P0 | X-02 conformance kit. Today `sources/mysql/src/tests.rs` has no lifecycle tests. |
| MY-31 | P0 | Reconnect and fault injection: proxy kill, server restart, half-open connection, long pause. |
| MY-32 | P0 | DDL scenarios: ALTER on a selected and an unselected table, TRUNCATE, ANALYZE/OPTIMIZE. Database-wide selection with an unsupported table. |
| MY-33 | P1 | Configuration validation matrix; `configuration()` round-trip ×7; credential rotation; `server_id` collision; per-type live tests. |
| MY-34 | P1 | Resolve CW-99; reduce matrix cost (CW-87); crash matrix (CW-14). |

### 4.7 Documentation

| ID | Pri | Task |
|---|---|---|
| MY-40 | P0 | `DESIGN.md`: engine, binding hash, cursor formats, snapshot handover, positional profile, retention checks, DDL policy. `CONFIGURATION.md`: field reference, privileges and server settings (`binlog_format=ROW`, `binlog_row_image`, `binlog_row_metadata=FULL`, 8.0.20+, GTID), update boundaries, credentials, lifecycle and failure behaviour. Remove the dependence on the legacy README (README:20-21) and the stray `drasi/mysql-native` identity. |

**Related CW:** 96, 98, 44, 41, 14, 17, 99, 87, 66, 83, 35, 06.

---

## 5. SQL Server (`drasi-computation-source-mssql`)

### 5.1 Current state

- One factory (`drasi.mssql/changes`), consumer-progress model.
- A pull-based CDC reader: each `next()` spawns a retained `Work` task. Paging is
  bounded, the cursor is bound to the catalog, retention is checked, and full
  before/after images are kept.
- The source owns no external state; barrier markers belong to the coordinated
  bootstrapper.
- **None of it has been qualified against a real server** (CW-78).

### 5.2 Lifecycle and management tasks

| ID | Pri | Task |
|---|---|---|
| MS-01 | P0 | **Pause/Resume:** settle or await the in-flight `Work` within the deadline, set a paused flag, and make `next()` park. Otherwise return `unsupported` (`source.rs:235-240`). |
| MS-02 | P0 | **Stop:** honour `StopMode` and the deadline. Abort fails fast with a typed "server work retained" error rather than waiting `shutdown_timeout_ms`; Tiberius cannot send ATTENTION (`work.rs:36-41`). Honour cancellation in start. |
| MS-03 | P0 | **Dispose:** retire `generation` (today only in `Drop`, `source.rs:274-280`), verify no pending `Work`, and treat Erase per D-1. Decide marker GC ownership: bootstrapper `dispose(Erase)` or a startup sweep (CW-44 orphan markers). |
| MS-04 | P0 | **Management.** InPlace (Pause): `poll_interval_ms`, `max_rows`, `max_bytes`, timeouts. Restart: password, `encryption`, `trust_server_certificate`. Recreate: `host`/`port`/`database` (hashed into the cursor binding, `catalog.rs:248-257`), `tables`, `table_keys`, `barrier_table`, `stream`, consumer membership. |
| MS-14 | P1 | **Cursor binding:** remove `host`/`port` from the binding, since `service_broker_guid` already identifies the database. Otherwise a DNS or AG-listener rename forces a query reset. |

### 5.3 Defects and functional gaps

| ID | Pri | Task |
|---|---|---|
| MS-05 | P0 | **Retry with backoff (X-06)** inside `next()`, for transient connection and timeout errors only. Retention gaps, binding changes and authentication errors stay fatal and typed. This restores legacy `stream.rs:50-130` behaviour, without its unsafe LSN fallback. |
| MS-06 | P0 | **Column types:** stop rejecting whole tables for unmapped types (`catalog.rs:218-221,283-299`). Map `json` to Json and `date`/`time` as legacy did; stream other types as string or null with a schema type of `None`. Optionally add an explicit column-exclusion list. |
| MS-07 | P0 | **Capture permission (CW-44):** native needs SELECT on `cdc.*_CT` (`paging.rs:232,287`), whereas legacy used `fn_cdc_get_all_changes_*`. Either fall back to the function with bounded LSN windows, or validate the grant at Start with a clear error and document it. |
| MS-08 | P1 | **Connection and catalog reuse:** one connection per activation with reconnect. Cache the catalog and PK discovery, re-reading on a cheap change detector (`cdc.change_tables`, `ddl_history`). Today every poll reconnects and re-reads the whole catalog, including every PK in the database (`paging.rs:91-93`, `keys.rs:62-77`). |
| MS-09 | P1 | **Throughput:** make `KEY_WINDOW = 32` (`paging.rs:15`) configurable or derived from `max_rows`. Replace the per-key `read_group` (N+1 queries, `paging.rs:285-292`) with a range fetch. Remove the `max_bytes/128` heuristic. Document the real page semantics, since the README presents `max_rows` as the page size. |
| MS-10 | P1 | **Commit time:** `DATEADD(... GETDATE() ... GETUTCDATE())` applies today's UTC offset to historical commits (`paging.rs:286`), so it is wrong across DST changes. Use a correct conversion. Schema changes and capture-instance changes follow D-7. |
| MS-11 | P1 | **Consumer resilience:** tolerate transient consumer non-readiness (`source.rs:140`) instead of failing the shared source. A membership change becomes a Recreate assessment. |
| MS-12 | P1 | **Credentials:** activation credentials, using `validate_with_bound_credentials` (`native.rs:80`, currently unused); mark only `password` secret. Plan Azure AD / managed identity: both legacy and native lack it, and it is required for Azure SQL. |
| MS-13 | P1 | **Configuration hardening:** `deny_unknown_fields` and a redacting `Debug` on `MsSqlSourceConfig` (`config.rs:150`; a misspelled password key silently becomes `""`). Fix the `EncryptionMode::NotSupported` doc comment (`config.rs:113-115`, which in fact encrypts only the login) and **default `encryption` to `On`**. Add a connect timeout (`connection.rs:80-96`). |
| MS-15 | P1 | **CDC grouping verification:** confirm split-update and `__$command_id` behaviour on a real server, and decode those cases instead of failing (`paging.rs:295-298,332-337`). |
| MS-20 | P2 | Relax identifier validation to allow quoted identifiers (`config.rs:58-64`); preserve decimals for `decimal`/`money` (currently mapped to Float). |
| MS-21 | P2 | State the rationale for `consumer_replay_retention = LOCAL_PROCESS_RESTART`, or move it to `LOCAL_POWER_LOSS`. Consider persisting the activation `origin` (CW-14). |

### 5.4 Code quality

| ID | Pri | Task |
|---|---|---|
| MS-16 | P2 | Add a codec helper that encodes before images, replacing the synthetic-Insert rebuild with `operations()[0]` indexing (`processing.rs:42-75`). |
| MS-17 | P2 | Rename `cleanup`/`initialized` and replace generation parity with an explicit state enum (`source.rs:39,68-69,257`). |
| MS-18 | P2 | Introduce a poller trait seam so the `next()` state machine is unit-testable without a database. |
| MS-19 | P2 | Remove the unused `#[allow(dead_code)]` lifecycle helpers or use them (`lib.rs:131-174`); typed errors (X-05). |

### 5.5 Legacy parity

| Legacy capability | Native | Action |
|---|---|---|
| Reconnect/backoff with error classification | None | MS-05 |
| Unknown types streamed as string, `json` → Json | Whole table rejected | MS-06 |
| `fn_cdc_get_all_changes_*` permission model | SELECT on CT tables | MS-07 |
| Dynamic subscribe/unsubscribe | Consumer set fixed | MS-11 (Recreate assessment) |
| Per-field `ConfigValue` | Whole-object secret | X-03 |
| Invalid LSN silently falls back to `start_position` | Explicit reset error | Intentional correction; document |
| Label normalisation (`normalize_table_label`) | Raw `table.name` | Verify and align |

### 5.6 Tests and documentation

| ID | Pri | Task |
|---|---|---|
| MS-30 | P0 | X-02 conformance kit; `next()` state-machine tests via MS-18. |
| MS-31 | P0 | Obtain the CW-78 environment and run the 8 Host and 2 Server real cases. Port the legacy 2.7k-line integration suite (CW-41). |
| MS-32 | P1 | Unsupported column types; split-update grouping; least-privilege permission; orphan markers; backlog performance; retention-gap and binding-change errors end to end. |
| MS-40 | P0 | `DESIGN.md`: CDC paging algorithm, cursor format and binding, source/bootstrapper handover, failure model. `CONFIGURATION.md`: field table, required grants and CDC setup (the legacy README has the user-facing CDC setup), encryption guidance, update matrix, secrets. Fix the ABI text in the source and bootstrapper READMEs. |

**Related CW:** 44 (permission, orphan markers), 96, 98, 17, 78, 41, 14, 66, 83, 47.

---

## 6. SQLite (`drasi-computation-source-sqlite`)

### 6.1 Current state

- Four factories. The source owns the database connection: writes enter through
  the REST API, or the Rust-only `handle()`, and capture uses preupdate and
  commit hooks.
- The replay and shared modes keep a journal in `__drasi_native_*` tables
  inside the user's database file.
- Source/bootstrap coordination is implemented. Bounded channels throughout,
  which improves on legacy.
- Host test coverage is good for the data path.
- `SqliteConfig` has no defaults at all, and `settings`/`rest_api` are published
  as opaque `Object`s.

### 6.2 Lifecycle and management tasks

| ID | Pri | Task |
|---|---|---|
| SQ-01 | P0 | **Stop ordering (reproduce first).** Test: output queue full, plus an accepted HTTP write, then Stop. Fix: on Stop, reject queued commands and let the worker exit even when output is blocked (replay modes may drop output because the journal retains it). Send DB shutdown before or alongside REST drain. `StopMode::Abort` shuts down immediately. Volatile committed-but-unemitted output becomes a typed, documented outcome rather than a `log::warn!` (`worker.rs:1209-1211`). |
| SQ-02 | P0 | **Pause:** gate REST and handle admission (503 or typed error), park the worker after its current transaction, and reopen on Resume. Otherwise return `unsupported`. Add bounded timeouts to REST/handle waits (`handle.rs:161-190`). Ask the CW-96 owners to list SQLite. |
| SQ-03 | P0 | **Erase:** declare `erases_owned_state`. In `dispose(Erase)`, via `spawn_blocking`, run `DROP TABLE IF EXISTS __drasi_native_{outbox,consumers,bootstrap,state}_v1` in one transaction. Never touch the user file or application tables. Idempotent when the file is missing; retry-safe when locked. Test: remove, then recreate on the same file (today this fails at `worker.rs:250-258`). |
| SQ-04 | P0 | **Context-aware start:** `select!` on cancellation and deadline around `wait_ready()` (`component.rs:242,259`) and `runtime.start`. |
| SQ-06 | P1 | **Management.** Restart: numeric bounds and `rest_api` (non-coordinated sources; `start_activation` already re-reads configuration). Recreate: `stream`, `path`, `tables`, `table_keys`, `output`, `replay` presence, `coordinated_snapshot`. Coordinated sources must match the coordinator byte for byte (`coordinator.rs:151-172`), so they are Recreate only. |
| SQ-09 | P1 | **Readiness/status:** use the `ControlSender`, which is dropped today in every `create*`, for REST or worker failure. |

### 6.3 Defects and functional gaps

| ID | Pri | Task |
|---|---|---|
| SQ-05 | P0 | **SQL/DDL handle parity for loaded plugins (D-6).** Until resolved, reject `path: null` (in-memory) in the dynamic factories: no DDL route exists, so in-memory mode is unusable when loaded. |
| SQ-07 | P1 | **Configuration:** `configuration_details` and nested schemas; publish defaults for every bound; secret/env references for REST host/port if the X-03 mechanism supports them (legacy used `ConfigValue`). |
| SQ-08 | P1 | **REST hardening:** optional authentication, bearer or API key via a secret reference (shared with NET-04); body-size limit and request timeout; refuse non-loopback binding without authentication or an explicit opt-in. Paginate `list_rows`, which today errors past `transactions.max_changes`; legacy returned every row (`rest_api.rs:180-190`). |
| SQ-15 | P2 | Opt-in `journal_mode`/WAL setting for non-replay file mode. Document the DDL and PRAGMA restriction (the authorizer denies them, `worker.rs:281-297`). |

### 6.4 Code quality

| ID | Pri | Task |
|---|---|---|
| SQ-10 | P1 | Move progress preparation (`component.rs:238-278`, also duplicated in legacy `native.rs:281-341`) into `sqlite-common`. Collapse the three identical `ensure_active` functions and the repeated `Single`/`Shared` dispatch in `coordinator.rs`. Pass the validated `Configuration` into `Coordinator` instead of re-parsing it (`coordinator.rs:96,184`). Remove the duplicated `cleanup_required`. |
| SQ-11 | P2 | Fix the wrong error text in `create()` for Replay: it says "consumer progress", should say source progress (`lib.rs:181-184`). Document or hide the public Rust-only `handle()`/`with_rest_api()`/`bound_rest_address()`. Name the progress-handler constant (`worker.rs:400`). |
| SQ-12 | P2 | Tracing spans and counters (commits, rollbacks, journal depth, REST requests). |

### 6.5 Legacy parity

| Legacy capability | Native | Action |
|---|---|---|
| Arbitrary SQL, DDL and transaction callbacks (handle / `SqliteClientResource`) | Rust-static only | D-6 / SQ-05 |
| DDL with schema-cache refresh | Denied | Document (SQ-15), or include in D-6 |
| REST `list_rows` returns every row | Bounded error | SQ-08 |
| `ConfigValue` for REST host/port | Plain strings | SQ-07 |
| Defaults for every field | None | SQ-07 |
| Erase refused (host-linked) | Ignored | SQ-03 |
| Element ID `table:id` | `sqlite:v1:…` | Intentional; document |

### 6.6 Tests and documentation

| ID | Pri | Task |
|---|---|---|
| SQ-30 | P0 | X-02 kit; stop-deadlock reproduction; Erase twice and recreate on the same file. |
| SQ-31 | P1 | Pause/resume; Abort vs Drain; start cancellation; assess/apply; in-memory via the loaded plugin (rejected or working per D-6); `list_rows` over the bound; non-loopback REST; SIGKILL matrix for the journal (CW-14); CW-82 mixed pipeline. |
| SQ-40 | P0 | `DESIGN.md`: worker and REST architecture, hooks and authorizer, journal tables and binding, coordinator lease, stop ordering, durability profile. `CONFIGURATION.md`: every field with defaults, an example per factory, the update matrix, Erase semantics, the REST security posture, the in-memory caveat. The README must stop deferring the field reference to the legacy README. Fix the ABI text. |

**Related CW:** 96 (add SQLite), 98, 17, 44, 41, 14, 82, 97.

---

## 7. Shared network runtime (`common/network`)

HTTP and gRPC share `SourceRuntime`/`Ingress`, so fix these once.

**Status (2026-11-14):** the P0 items are implemented, as are most of the P1
items. Status values: **Done**, **Partial** (the remaining work is stated) and
**Open**. Evidence is in the `common/network` unit tests, the HTTP and gRPC
crate tests, and the Host SDK `native_network_loopback` and `native_network_wire`
targets run against freshly built libraries.

| ID | Pri | Status | Task |
|---|---|---|---|
| NET-01 | P0 | **Done** | **Pause gate in `Ingress`** (D-13: reject fast). Pause closes admission: 503 with `Retry-After` on HTTP, `UNAVAILABLE` on gRPC, `Busy` for durable admission. Open gRPC streams end when paused. Pause waits for in-flight admissions, honouring cancellation and the deadline; a cancelled pause reopens admission. Volatile admission blocked on a full queue is interrupted by Pause. Health reports `paused` (HTTP) or `Degraded` (gRPC). |
| NET-02 | P0 | **Done** | **StopMode-aware stop.** Drain joins through `join_owned_worker_gracefully`, bounded by `ctx.deadline()`; a timeout is retryable and keeps ownership. Abort aborts the listener. HTTP has a header-read timeout and a connection cap; gRPC has a connection cap, stream idle timeout and HTTP/2 keepalive. |
| NET-03 | P1 | **Done** | The default host is `127.0.0.1`. A non-loopback listener needs authentication, or `allowUnauthenticated: true` (named `allowInsecureNetwork` in the original plan). It is refused at construction otherwise. |
| NET-04 | P0 | **Done** | Bearer tokens (a primary and a secondary for rotation) as top-level secret fields, with a minimum length of 16 bytes and constant-time comparison. Optional TLS and mutual TLS (rustls with the ring provider). A per-client token-bucket rate limit that tracks at most 65,536 clients and fails closed. A connection limit (default 1,024). The byte budget is `ingressCapacity × maxMessageBytes`, documented in each `CONFIGURATION.md` rather than separately capped (see NET-11). |
| NET-05 | P1 | **Done** | **Server-sequenced durable mode (D-5).** The source lazily registers a producer named by its own component UUID, resumes from `producer_status`, and acknowledges only after commit. It serves the unchanged standard routes, webhooks and legacy gRPC RPCs. A resync drops the session. Delivery is at-least-once: a retry after a lost acknowledgement is admitted again. The producer-session protocol refuses the source's own UUID. |
| NET-06 | P1 | **Open** | The volatile path still holds the `sequence` mutex across `sender.reserve().await` for a whole batch, keeping each batch contiguous at the cost of serializing requests. The new implicit durable path is serialized in the same way (NET-12). |
| NET-07 | P2 | **Partial** | Done: `eprintln!` is replaced by the `diag!` macro, which uses `log` when a host logger is installed and otherwise writes warnings and errors to stderr. Errors are typed (`IngressError`/`IngressErrorKind`), so the name collision is gone. The `out` PortId is cached. Adaptive validation is deduplicated. `common/network/README.md` is rewritten. Open: `tracing` spans need a native logging bridge (NET-13), and `ControlSender` is still not wired for listener death. |
| NET-08 | P1 | **Open** | Confirm and test host erasure of durable admission state on removal (D-12). HTTP and gRPC declare `erases_owned_state` with a no-op erase (D-1), so the QoS producer registry and journal depend on host-side cleanup. |
| NET-09 | P1 | **Done** | `check_source`, producer naming and implicit-session handling are in `common/network/durable.rs`, used by both transports. |
| NET-10 | P1 | **Open** | Depth limits for JSON, XML, YAML and protobuf `Struct` (CW-19). Size limits exist (`maxMessageBytes`, Content-Length pre-check, 413); nesting depth is not bounded. |
| NET-11 | P2 | **Open** (new) | Optional total in-flight byte budget across all requests, independent of `ingressCapacity × maxMessageBytes` (CW-29). |
| NET-12 | P1 | **Open** (new) | Implicit durable admission is serialized by one mutex per source, so durable throughput is limited to one commit at a time. Measure it and, if needed, pipeline consecutive sequences while keeping their order. |
| NET-13 | P1 | **Open** (new) | **Native logging bridge.** The native SDK has no way to send a dynamically loaded plugin's log records to the host, so `diag!` falls back to stderr. This needs SDK and Host work (record the gap under CW-24). |
| NET-14 | P2 | **Open** (new) | Metrics (CW-26): requests, admissions, rejections by kind, rate-limited clients, queue occupancy, durable commit latency. |

---

## 8. HTTP (`drasi-computation-source-http`)

**Status (2026-11-14):** every P0 is done. HT-09 remains open, and HT-04 is
partial. `DESIGN.md`, `CONFIGURATION.md` and a short `README.md` exist.

### 8.1 Lifecycle, management and security tasks

| ID | Pri | Status | Task |
|---|---|---|---|
| HT-01 | P0 | **Done** | Adopts NET-01 and NET-02. Tests cover pause refusal with `Retry-After` and paused health, resume, and a Drain stop that a client which never sends headers cannot hold. |
| HT-02 | P0 | **Partial** | **Done:** authentication protects the standard, batch and `/admission/v1/*` routes; `/health` stays open. Tests check the 401 and `WWW-Authenticate` response and the rate-limit 429. **Open:** a real-library assertion that rejected requests produce no graph changes (CW-41). |
| HT-03 | P1 | **Done** | Management is implemented for all three boundaries. `assess` classifies every field; `apply` re-assesses before changing anything. InPlace: `timeoutMs`, `maxMessageBytes`, `maxBatchEvents`, auth tokens, rate limits and webhook routes. Restart: host, port, `ingressCapacity`, `maxConnections`, `headerReadTimeoutMs`, TLS, adaptive settings, webhook mode and CORS. Recreate: `stream`. Durability follows the host admission binding, not configuration. Adaptive fields publish their defaults only as descriptions. Assess-matrix and live-apply conformance tests pass. |
| HT-04 | P1 | **Partial** | **Done:** an authentication failure is always a generic 401, whatever `errorBehavior` is set. `initialize` checks that the webhook secret environment variables are present. **Open:** move webhook secrets to secret references, which needs nested references or a flattened secret map (D-2); add timestamp-tolerance replay protection; add an option to disallow HMAC-SHA1. |
| HT-05 | P1 | **Done** | TLS and mutual TLS, with PEM material in top-level secret fields. A test covers both. |
| HT-06 | P1 | **Done** | `source_schema_reader` derives the schema from webhook mappings (`schema.rs`) and is refreshed on apply. |
| HT-07 | P1 | **Done** | Standard and webhook routes are transparently durable through NET-05. A durable webhook never acknowledges a partly mapped request. Explicit-session webhook headers require `effectiveFrom` on every mapping. A Host SDK test covers commits through the server-sequenced producer. |
| HT-08 | P2 | **Partial** | **Done:** `maxBatchEvents` is a plain `usize`; the `Deref` composition is removed; 413 is returned only for length errors, 408 for body timeouts and 400 for bad input; router rejections and 404s use JSON; webhook routes cannot use `/health` or `/sources/`; health reports mode and admission; unused dev-dependencies are removed. **Decided:** adaptive prefetch stays disabled by default (documented). **Open:** the `webhooks` schema is still not published as a nested schema; the bound address for port `0` appears only in the log; there is no CORS preflight test on the fallback route. |
| HT-09 | P2 | **Open** | Port legacy `webhook_e2e.rs` and `webhook_integration.rs` to native construction. Add unit tests for each `errorBehavior` and the durable-header paths in `webhook.rs`, and for `durable_http.rs`. |

### 8.2 Legacy parity

| Legacy capability | Native | Action |
|---|---|---|
| Durable (WAL-before-ack) on standard routes, batch and webhooks with unchanged clients | **Restored** through the server-sequenced producer (at-least-once) | Done (HT-07 / NET-05) |
| Schema from webhook mappings | **Restored** | Done (HT-06) |
| `ConfigValue` for every field | Top-level secret references for tokens and TLS material; webhook secrets still use environment variables | HT-04 / D-2 |
| Adaptive batching enabled by default | Disabled by default; a deliberate, documented difference | Decided (HT-08) |
| Tracing spans, status, adaptive metrics | `diag!` logging and a health route | NET-13, NET-14 |
| Routes, webhooks, templates, content types, HMAC/bearer auth, CORS, error behaviours | Present (shared `http-common`) | Kept; hardened by HT-04 |

### 8.3 Tests and documentation

| ID | Pri | Status | Task |
|---|---|---|---|
| HT-30 | P0 | **Partial** | **Done:** the X-02 kit runs on both Tokio flavours: lifecycle, cancelled start, erase twice, configuration round-trip, assess matrix and live apply. Further tests cover auth, rate limit, 413/400, pause, Drain with a stalled client, the connection limit, TLS/mTLS, the webhook generic 401, a missing webhook secret, and refusal of unauthenticated non-loopback listeners. **Open:** a CORS preflight test, port-0 discovery (blocked on HT-08), and running the kit through the dynamic library. |
| HT-40 | P0 | **Done** | `DESIGN.md` and `CONFIGURATION.md` written, and the `NETWORK.md` ABI text corrected. |

**Related CW:** 96, 44, 41, 14, 17, 19, 24, 26, 29, 21, 84, 67, 97.

---

## 9. gRPC (`drasi-computation-source-grpc`)

**Status (2026-11-14):** every P0 is done. `DESIGN.md`, `CONFIGURATION.md` and a
short `README.md` exist.

| ID | Pri | Status | Task |
|---|---|---|---|
| GR-01 | P0 | **Done** | Adopts NET-01 and NET-02. Open streams end when paused, and the stream's response reports cumulative counts. |
| GR-02 | P0 | **Done** | TLS and mutual TLS (tonic `tls-ring`). An interceptor applies authentication and rate limiting to every RPC on `SourceService` and `AdmissionService`, including register, retire and receipt. Tests cover this. `HealthCheck` also requires the token when one is set (GR-08). |
| GR-03 | P1 | **Done** | The transport timeout is `timeoutMs + max(1 s, timeoutMs / 4)`, so the typed admission error always wins. A test covers it. |
| GR-04 | P1 | **Done** | Management is implemented for all three boundaries. `assess` classifies every field (`timeoutMs` is Restart because it sets the transport timeout). Assess-matrix and live-apply conformance tests pass. |
| GR-05 | P1 | **Done** | Unchanged `SubmitEvent`/`StreamEvents` clients are durable through NET-05. |
| GR-06 | P1 | **Partial** | **Done:** `streamIdleTimeoutMs` (default 300,000) is separate from `timeoutMs`; streams have their own `maxConcurrentStreams` limit (default 64), so they cannot starve unary calls; HTTP/2 keepalive and the stream limit are configured. **Open:** an optional `grpc.health.v1` service, which needs `tonic-health`, a new dependency not yet approved. |
| GR-07 | P2 | **Partial** | **Done:** the `adaptiveEnabled` stub is deleted; the logging side effect in `From<AdmissionError>` is removed; per-message rate limiting on streams is added. **Open:** further deduplication of the four unary admission handlers; optional throttled stream progress for legacy clients. |
| GR-08 | P2 | **Open** (new) | Decide whether `HealthCheck` should bypass the bearer token, as HTTP `/health` does, so that unauthenticated liveness probes work. |
| GR-30 | P0 | **Partial** | **Done:** the X-02 kit runs on both Tokio flavours, plus tests for the deadline margin, auth, rate limit, separate stream slots, idle timeout, paused health, TLS/mTLS and refusal of unauthenticated non-loopback listeners. **Open:** malformed, oversize and deeply nested `Struct` input (CW-19, NET-10); the `durable_grpc` error-kind mapping table; native gRPC in the CW-36 completion profile; running the kit through the dynamic library. |
| GR-40 | P0 | **Done** | `DESIGN.md` and `CONFIGURATION.md` written, and the `NETWORK.md` `common.proto` text reconciled. |

Legacy parity: `RequestBootstrap` returned an empty stream in legacy and returns
`Unimplemented` in native; this is kept and documented. Invalid streamed input
used to be logged and skipped; it now terminates the stream, which is
intentional and documented. Durability is restored by GR-05. Tokens and TLS
material use top-level secret references (X-03).

**Related CW:** 96, 44 (TLS), 41, 36, 14, 17, 19, 24, 26, 29, 67, 97.

---

## 10. Bridge (`drasi-computation-source-bridge`)

### 10.1 Current state

- Implements the in-process `drasi_lib::computation::v1::ComputationComponent`/`EnvelopeSource`/`ComponentFactory`,
  not the plugin SDK.
- Both the crate and `common/bridge` depend on `drasi-lib`
  (`sources/bridge/Cargo.toml:13`, `common/bridge/Cargo.toml:13`). Static-only;
  registered only by `instance-host`.
- No legacy equivalent exists; the bridge originated natively.
- Strong points: mTLS with an exact leaf pin, hostile-input codec hardening,
  bounded queues, owned workers.
- **Zero logging.** `LinkStatus` is not surfaced.

### 10.2 Tasks

| ID | Pri | Task |
|---|---|---|
| BR-01 | P0 | **Pause:** add a gate in `Receiver::admit` returning retryable `Busy`/`Paused` while receipt and control lanes stay live and dequeuing stops; Resume reopens. Otherwise return `unsupported`. Define the sender's retry budget for a paused receiver; the sink's fixed 16 × 5 s cap means a long pause fails the sender. |
| BR-02 | P0 | **StopMode:** Drain completes the in-flight frame's admit and receipt within the deadline. Today cancellation drops the connection mid-commit (`receiver.rs:899-903`), which can fence the durable channel. Abort keeps the current behaviour. Add a test proving the journal is not fenced. |
| BR-03 | P0 | **Erase semantics:** the link registers a single producer in its journal keyed by the contract fingerprint (`receiver.rs:294-302`). A recreated link with a different contract fails with "already bound to another bridge contract" (`receiver.rs:357-364`). Either own the journal and erase the producer idempotently, or document reprovisioning. Replace the prose `ensure` refusal with a typed error. Coordinate with D-12. |
| BR-04 | P0 | **Observability:** structured logging for accept and handshake failures (silently swallowed today with a 10 ms retry, `receiver.rs:881,895-897`; persistent `EMFILE` spins at about 100 Hz), generation changes, `Busy`, `AcceptanceUnknown` and worker exits. Surface `LinkStatus` through graph diagnostics. |
| BR-05 | P0 | Fix the docs: wire version 1 → 2 (`common/bridge/README.md:26-27`; `protocol.rs:16`) and the non-UUID endpoint examples (README:36-39). |
| BR-06 | P1 | **Hosting (D-3):** port to the plugin SDK (`NativeComponent`, `create_with_admission`, TLS via `TlsResourceRecipe` in configuration, seal/drain via control or management), remove the `drasi-lib` dependency, and add `export_computation_plugin!`. This unblocks Server use and CW-41. |
| BR-07 | P1 | **Management:** split `Limits` into wire-contract fields and local settings (`timeout_ms`, `retry_*`, `queue_capacity`). Today every limit is part of the fingerprint, and `LinkContract::check` requires exact equality (`config.rs:346-410`). Local settings become InPlace; contract changes are Recreate, refused when the peer would diverge. |
| BR-08 | P1 | `specification_from_config`, and a full configuration schema: descriptions, enum constraints for `delivery`/`profile` (which also have inconsistent casing), defaults, nested `limits`. Remove the duplicated `receiver_endpoint` UUID, which must equal the graph-assigned ID (`factory.rs:118-121`). |
| BR-09 | P1 | Honour the start deadline and cancellation; `wait_active` on the admission mailbox before readiness (required by `lib/AGENTS.md`). Use `admit_wait` in durable mode instead of fail-fast `Busy` (`receiver.rs:552-556`). Add exponential backoff with jitter on the sink side. |
| BR-10 | P2 | Remove `expect` from lifecycle paths (`receiver.rs:296,301,373,584,614,624`; `config.rs:227,333,399`; `component.rs:207`), the index-based worker roles (`component.rs:215`), the magic constants and the cause-dropping `to_string()`. Replace the 5 ms `wait_drained` poll with event-driven waiting (`boundary.rs:117-126`). |
| BR-11 | P2 | Hardening: TCP keepalive and an idle timeout after hello; per-role control slots, since two observer clients today lock out sender Hello (`receiver.rs:878`); TLS handshake outside the serial accept. |
| BR-12 | P2 | TLS: multiple pins for rotation overlap; optional PEM; reload path. Protocol: documented upgrade path or min/max version negotiation; consider windowed frames (v3) to lift the stop-and-wait throughput limit. |
| BR-30 | P0 | Source lifecycle tests (none exist): restart on the same object, pause/resume, Erase retry, stop timeout and retry, factory negative validation, accept-error resilience, protocol-version mismatch. After BR-06, the X-02 kit plus real-library tests. Prepared-peer partition tests (CW-91). |
| BR-40 | P0 | Split `common/bridge/README.md` (446 lines, covering both endpoints) into a source `DESIGN.md` (topology, v2 state machine, generations and sessions, receipts, cuts, durability ownership, TLS, failure semantics) and `CONFIGURATION.md` (every field, contract vs local flag, resources, update matrix, journal provisioning, Erase). Add rustdoc. |

**Related CW:** 96, 98, 17, 41, 75, 91, 05, 21, 97.

---

## 11. Counter (`drasi-computation-source-counter`)

Counter is the only source with management and `configuration_details`. Plugin
authors will copy it, so it must be exemplary.

| ID | Pri | Task |
|---|---|---|
| CT-01 | P0 | **Mock parity (D-4).** Recommended: a separate native mock plugin (MK-01 below), so counter stays minimal. |
| CT-02 | P0 | **Erase:** resolve per D-1 and test it. Today removal with erase always fails in the host. |
| CT-03 | P1 | **`apply` bug:** it requires `self.running` (`component.rs:183-186`), so a stopped or never-started object cannot be updated. Return `ApplyError::Refused` (not `Failed`) for plan, identity or configuration mismatch. |
| CT-04 | P1 | **Pure assessment:** `Configuration::parse` calls `Instant::now()` (`component.rs:76-80`) inside `assess`. Replace it with a fixed `interval_ms` maximum and publish it in the schema. |
| CT-05 | P1 | Cancellation-safe, drift-free interval (`tokio::time::interval` with `MissedTickBehavior::Delay`). Today it sleeps per call, which drifts and holds pause and stop for long intervals. |
| CT-06 | P1 | Tests: configuration validation (overflow, unknown field, defaults), control handler (unknown kind, payload), sequence monotonicity, timing with paused Tokio time, apply on a stopped object, Erase, and the X-02 kit. |
| CT-07 | P1 | A static `source_schema_reader` (`Counter {value: Integer}`) as the reference pattern for schema publication. |
| CT-08 | P2 | InPlace updates for `count` increase and `paused`. Turn control kinds and the annotation key into `pub const`s. Remove the `expect`s in metadata. Add rustdoc. |
| CT-09 | P2 | Document the two pause concepts (host Pause vs the `drasi.counter.pause` control). Handle a dropped control channel while paused (`component.rs:209-211`). Optionally set `effective_from` to wall-clock milliseconds, since it is currently the sequence number (`component.rs:234`). |
| CT-40 | P0 | `DESIGN.md`: purpose, emission model, sequencing, pause concepts, volatility, the patterns to copy. `CONFIGURATION.md`: fields, defaults, ranges, controls, update matrix, Erase. |

### 11.1 New: native mock source (if D-4 chooses a separate plugin)

| ID | Pri | Task |
|---|---|---|
| MK-01 | P0 | New crate `computation-plugins/sources/mock` (`drasi.standard/mock`) implementing the generators from `components/sources/mock`: `counter`, `sensor_reading {sensor_count, mesh}` (Insert, then Update; `CONNECTED_TO` relations with strength updates and rewiring) and `generic`. It needs a `timestamp` property, unbounded run, interval default and validation, `source_schema_reader` (port `describe_schema`, `mock.rs:190-275`), and a paired bootstrap factory (10 deterministic nodes) via `PluginDefinition::new_with_bootstrap`. Provide an ABI 4 equivalent of mock's FFI bootstrap-regression role (issue #602). Event injection (`inject_event`) can be a test-support feature. Full X-01/X-02/X-09 compliance from day one. |

---

## 12. Sequencing

Breaking changes are allowed. To avoid churning each plugin twice, do the work
in this order:

**Phase 0: decisions and scaffolding (blocking).**
D-1, D-2, D-3, D-4, D-5, D-6, D-7, D-12, D-13. X-01 (standard), X-02 (test
kit), X-09 (doc templates), and the X-03 mechanism.

**Phase 1: safety-critical P0s, in parallel per family.**

- PostgreSQL: PG-01, 02, 04, 03, 11. **Done 2026-11-15**, together with the
  PostgreSQL Phase 2 and 3 items except PG-13; see
  [PostgreSQL implementation status](#postgresql-implementation-status-2026-11-15).
- MySQL: MY-01, 02, 03.
- SQLite: SQ-01, 03, 02.
- SQL Server: MS-06, 05, 07, 01–03.
- Network: NET-01, 02, 04, then HT-02 and GR-02. **Done 2026-11-14**, together
  with the HTTP/gRPC Phase 2 and 3 items. The remainder is listed in
  [Implementation status](#implementation-status-2026-11-14).
- Bridge: BR-01–05.
- Counter: CT-02.

**Phase 2: lifecycle, management and configuration P0s.**
Apply X-03, X-04 and the context-aware lifecycle to every plugin (PG-05/06/12,
MY-04–07/09, MS-04, SQ-04/06/07, HT-03, GR-04, CT-03/04). Run the X-02 kit for
every plugin in CI.

**Phase 3: parity and capability.**
Schema readers (PG-07, HT-06), transparent durability (NET-05, HT-07, GR-05),
schema-evolution policy (PG-08, MY-02 follow-ups, MS-10), bootstrap coordination
(PG-13, MY-16), the native mock (MK-01), the bridge SDK port (BR-06–09), type
coverage (MY-13, MS-06 follow-ups), REST and TLS hardening (SQ-08, HT-04/05,
GR-06).

**Phase 4: quality and qualification.**
Typed errors (X-05), observability (X-07), hygiene and refactors (X-08 and the
per-plugin P2s), the CW-41/14/78/85/99 qualification runs, DESIGN and
CONFIGURATION docs finalised against shipped behaviour.

Each phase ships per plugin as a separate reviewable change, following the
remaining-work convention.

---

## 13. Definition of done (per plugin)

A native source is production ready when:

1. All six hooks meet X-01. Pause is real or `unsupported`; stop honours
   `StopMode` and deadlines; dispose implements its declared intents
   idempotently.
2. `Factory::assess` covers every configuration field with a tested boundary,
   and `NativeManagement` exists wherever a Pause or Stop boundary is declared.
3. Configuration publishes complete nested schemas, `configuration_details` and
   exact defaults. Secrets are individual secret fields or activation
   credentials; `configuration()` round-trips.
4. Every legacy capability in its parity table is present or covered by an
   accepted decision in §2.
5. The X-02 conformance kit passes through the linked and dynamic library on
   both Tokio flavours. The plugin's real-system suite runs in a named CI job
   (CW-85), with no skipped case counted as a pass. Its CW-14 crash matrix
   passes where it claims recovery.
6. Errors are typed and classified. Transient failures retry with backoff.
   Status, readiness and metrics are reported (CW-22, CW-26).
7. `DESIGN.md` and `CONFIGURATION.md` exist, match the code, and the README and
   version text is current.
8. No `expect`/`unwrap` on FFI-reachable paths, no `drasi-lib` dependency, and
   `make clippy`, `make check-native-dependencies` and `cargo fmt` are clean.
9. The related CW entries are updated to reflect what this work closed.

---

## Implementation status (2026-11-14)

**Delivered:** the shared network runtime, the conformance harness, and the
HTTP and gRPC sources, per the status columns in §7–§9.

The changes stayed inside `components/`, apart from dependency edges in
`Cargo.lock`. Configuration is a breaking change, with no migration:

- the host defaults to `127.0.0.1`;
- gRPC `adaptiveEnabled` is removed;
- new fields cover auth, TLS, rate limits, connection limits and stream limits;
- HTTP `maxBatchEvents` is a plain integer.

Verification:

| Target | Result |
|---|---|
| `drasi-computation-network-common` | 18 tests |
| `drasi-computation-plugin-sdk` | 105 tests |
| HTTP source | 24 tests |
| gRPC source | 21 tests |
| Host SDK `native_network_loopback` | 34 passed, 2 ignored (CW-94); run against freshly built libraries |
| Host SDK `native_network_wire` | 8 tests |

Clippy with `-D warnings`, `cargo fmt` and `check-native-dependencies` are clean
for the changed crates.

**Not run at the time:** the Host SDK `native_postgres_http_bootstrap`
container tests. *They were run and pass on 2026-11-15, after fixing stale
component IDs, the HTTP route and topology names in the test.*

**New issues found during implementation** (also recorded in the remaining-work
document):

| ID | Issue | CW |
|---|---|---|
| NET-11 | No total in-flight byte budget; the bound is `ingressCapacity × maxMessageBytes`. | 29 |
| NET-12 | Implicit durable admission is serialized per source, one commit at a time. | 32 |
| NET-13 | No native logging bridge: dynamically loaded plugins' logs fall back to stderr. | 24 |
| NET-14 | No HTTP/gRPC metrics. | 26 |
| HT-04 (remainder) | Webhook secrets cannot use secret references, because the host resolves references only on top-level fields. | 44 |
| HT-08 (remainder) | Port `0` reports its bound address only in the log. Nested `webhooks` schema not published. | 44 |
| GR-06 (remainder) | No `grpc.health.v1`; it needs the `tonic-health` dependency. | 44 |
| GR-08 | gRPC `HealthCheck` requires the bearer token when one is set. | 44 |
| X-02 (remainder) | The kit runs only through the linked plugin, not yet through the dynamic library. | 41 |

**Remaining HTTP/gRPC work, in suggested order:**

1. NET-08/D-12.
2. CW-41 rejection-produces-no-changes coverage through the loaded library.
3. NET-10 depth limits.
4. HT-04 webhook secret references.
5. NET-13 logging bridge.
6. HT-09 webhook suites.
7. GR-06/GR-08 health.
8. NET-06/NET-12 throughput.
9. NET-14 metrics.

## PostgreSQL implementation status (2026-11-15)

**Delivered:** §3, per its status column, with decisions D-7 to D-10. The
changes stayed inside `components/`; `Cargo.lock` is unchanged.

- **Configuration (breaking, no migration):** one flat camelCase object shared
  by the source and its coordinated and shared bootstrap providers; operational
  fields have defaults; `password` is the only secret. Removed:
  `auto_create_slot`, `start_from_slot`, `reconnect_attempts`,
  `reconnect_delay_ms`, the nested `settings`/`connection` objects and the
  `drasi_slot`/`drasi_publication` defaults. Existing cursors do not match the
  narrowed binding and are reported as `SchemaChanged`; existing sources must
  be removed and re-added.
- **Shared code:** `postgres-common` gained `replication::{settings, error,
  schema}`, slot management (`ensure_slot`, `drop_owned_slots`), a single
  retained-cursor check and `start_streaming`. The update planner moved from
  `common/network` into the SDK (`update_planning`), re-exported by the network
  crate.
- **Docs:** `DESIGN.md` and `CONFIGURATION.md` (field table, prerequisites,
  erase and failure runbook, migration table); the source and bootstrapper
  READMEs were rewritten for the new configuration.

Verification:

| Target | Result |
|---|---|
| `drasi-postgres-common` | 107 tests |
| PostgreSQL source | 32 tests |
| PostgreSQL bootstrapper | 3 tests |
| `drasi-source-postgres --test native_transactions` (live, Podman, PostgreSQL 16) | 60 passed, static and built libraries; includes 7 new tests (erase, missing slot and TRUNCATE refusal, schema drift, all-tables join, reconnect, long pause, schema reader) |
| Host SDK `native_postgres_http_bootstrap` (live) | 2 passed |

Clippy with `-D warnings`, `cargo fmt --check` and `check-native-dependencies`
are clean for the changed crates.

Before this work the live suite had 50 of 53 passing; the three failures were
test bugs (CW-99), now fixed.

**Remaining PostgreSQL work** (recorded as CW-102): PG-13 coordination, the
D-9 typed reset and D-10 opt-in full-row identity, the SDK conformance kit,
the activation-credential and PostgreSQL 12 live tests, the CW-14 crash
matrix for rows and shared modes, metrics, and converting the Server
consumers listed in the source's `DESIGN.md`.

---

## Appendix: remaining-work cross reference

| Component | Applicable CW IDs |
|---|---|
| postgres | 102, 96, 98, 44 (discovery), 41, 14, 17, 99, 90, 83, 35, 39, 18, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 46, 77, 85, 87, 88, 95, 97 |
| mysql | 96, 98, 44 (discovery), 41, 14, 17, 99, 87, 66, 83, 35, 39, 18, 22, 23, 25, 26, 63, 72, 73, 03, 05, 06, 08, 10, 37, 46, 77, 85, 88, 95, 97 |
| mssql | 96, 98, 44 (permission, orphan markers, discovery), 78, 41, 14, 17, 47, 66, 83, 35, 39, 18, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 46, 85, 87, 88, 95, 97 |
| sqlite | 96 (not listed; should be), 98, 44, 82, 41, 14, 17, 35, 39, 18, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 46, 77, 85, 88, 95, 97 |
| http | 96, 98, 44 (auth, limits), 41, 14, 36, 17, 18, 19, 21, 29, 84, 67, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 39, 46, 85, 88, 95, 97 |
| grpc | 96, 98, 44 (TLS), 41, 14, 36, 17, 18, 19, 21, 29, 67, 22, 23, 25, 26, 63, 72, 73, 03, 05, 08, 10, 37, 39, 46, 85, 88, 95, 97 |
| bridge | 96, 98, 17, 41, 75, 91, 05, 18, 19, 21, 22, 23, 25, 26, 29, 63, 72, 73, 03, 08, 10, 14, 36, 37, 39, 46, 85, 88, 95, 97 |
| counter | 98, 17, 22, 23, 25, 26, 63, 72, 73, 03, 08, 10, 37, 41, 46, 85, 88, 95, 97 |
| common/network | 96, 44, 17, 19, 25, 26, 29, 67, 05, 08, 14, 36, 41 |
| common/bridge | 05, 17, 75, 91, 08, 41 |

Notes:

- **CW-95** (configuration version ranges and migration) is user-deferred. When
  it is picked up, every factory here will need `configuration_versions` and a
  `MigrationChain`. Starting all of them at configuration version 1 after this
  breaking change keeps that future work clean.
- **CW-97** (Server ABI 4 port) must rebuild every source library. Server
  examples referenced by these READMEs (for example `drasi-server/examples/native-sqlite.rs`)
  depend on it.
- **Recorded in the remaining-work document (2026-11-15):** PostgreSQL
  progress under CW-96 (pause), CW-98 (erase), CW-44 (discovery) and CW-99
  (TLS and crash-helper tests resolved), and the follow-ups as CW-102.
- **Recorded in the remaining-work document (2026-11-14):** HTTP/gRPC progress
  under CW-96, CW-98, CW-44 and CW-41; MySQL, SQL Server and SQLite pause gaps
  under CW-96; the native logging bridge under CW-24; the network-source
  follow-ups under CW-100.
- **New items still to record in the remaining-work document:** the bridge
  `drasi-lib` dependency (D-3); the mock parity gap under CW-44 (D-4); the
  PostgreSQL slot leak/adoption and TRUNCATE loop; MySQL reconnect and DDL
  fencing; SQL Server type rejection and throughput. Record each when its
  plugin's work starts.
