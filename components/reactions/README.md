# Drasi Reaction Plugin Developer Guide

## Purpose

This document is a guide for software developers that explains how to create and
maintain custom **Reaction plugins** in Rust for `drasi-lib`.

Reactions are the component of Drasi that act when Continuous Query results change.

It is through Reactions that Drasi achieves the flexibility to integrate with any
external downstream system — for example, by making an HTTP request, writing to a
message broker or database, appending to a file, or updating a UI.

---

## Table of Contents

1. [Conventions: MUST, SHOULD, MAY](#1-conventions-must-should-may)
2. [Overview & Mental Model](#2-overview--mental-model)
    1. [A reaction in context](#a-reaction-in-context)
3. [Quickstart: A Working Reaction in 15 Minutes](#3-quickstart-a-working-reaction-in-15-minutes)
4. [Core Concepts](#4-core-concepts)
    1. [The Component Lifecycle](#41-the-component-lifecycle)
    2. [Runtime-Owned Query Subscriptions](#42-runtime-owned-query-subscriptions)
    3. [The Priority Queue & Backpressure](#43-the-priority-queue--backpressure)
    4. [The Runtime Context](#44-the-runtime-context)
5. [The `Reaction` Trait](#5-the-reaction-trait)
    1. [Required methods](#51-required-methods)
    2. [Lifecycle — the runtime call sequence](#52-lifecycle--the-runtime-call-sequence)
    3. [Optional capability overrides](#53-optional-capability-overrides)
    4. [Error handling](#54-error-handling)
6. [`ReactionBase` — Reference](#6-reactionbase--reference)
7. [Packaging as a Dynamic Plugin](#7-packaging-as-a-dynamic-plugin)
8. [Secrets, Environment Variables, and Identity](#8-secrets-environment-variables-and-identity)
9. [The Query Result Data Contract](#9-the-query-result-data-contract)
10. [Default Output Structure (SHOULD)](#10-default-output-structure-should)
11. [Output Templating (SHOULD)](#11-output-templating-should)
12. [Optional Capabilities](#12-optional-capabilities)
    1. [Deprovisioning](#121-deprovisioning)
    2. [Durable State & Checkpoints](#122-durable-state--checkpoints)
    3. [Recovery Policy](#123-recovery-policy)
    4. [Bootstrap on Fresh Start](#124-bootstrap-on-fresh-start)
    5. [Adaptive Batching](#125-adaptive-batching)
13. [Testing](#13-testing)
14. [Conformance Checklist](#14-conformance-checklist)
15. [Anti-Patterns](#15-anti-patterns)
16. [Appendix A — Full TypeSpec Schemas](#appendix-a--full-typespec-schemas)
17. [Appendix B — Glossary](#appendix-b--glossary)

---

## 1. Conventions: MUST, SHOULD, MAY

This guide uses [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) keywords
in capital letters to distinguish requirement levels.

| Keyword                  | Meaning                                                                                                                                  |
|--------------------------|------------------------------------------------------------------------------------------------------------------------------------------|
| **MUST**, **REQUIRED**   | A hard contract. Violating it causes compile errors, startup failures, test failures, or silent data corruption.                          |
| **MUST NOT**             | An explicit prohibition. Same consequences as MUST.                                                                                      |
| **SHOULD**, **RECOMMENDED** | A strong convention shared by every well-behaved reaction. Deviating is allowed but the burden of justification is on you.            |
| **SHOULD NOT**           | Strongly discouraged. Reach for an alternative first; deviate only with a good reason.                                                   |
| **MAY**, **OPTIONAL**    | A capability the `drasi-lib` runtime supports but does not require. Use only when your reaction genuinely needs it.                      |

You will see callouts throughout this guide, for example:

> 🔴 **MUST** — Implement the `Reaction` trait.  
> 🟡 **SHOULD** — Support output templating to enable the plugin to
> integrate with downstream systems with different schema requirements.  
> 🔵 **MAY** — Override `default_recovery_policy()` to opt into automatic gap-skipping.

---

## 2. Overview & Mental Model

A Reaction is created, configured, and added to a `drasi-lib` instance,
at which time it is subscribed to one or more Continuous Queries.
When Continuous Queries' results change, the `drasi-lib` instance forwards a `QueryResult` to
the reaction. The reaction's job is to take those changes, optionally
format them, and dispatch them to an external system — for example, by
making an HTTP request, writing to a message broker or database,
appending to a file, or updating a UI.

```
 ┌──────────┐    ┌────────┐    ┌────────────┐    ┌──────────────────┐
 │ Source 1 │───►│ Query  │───►│ Reaction   │───►│ External system  │
 │ Source 2 │───►│ engine │───►│ (your code)│    │ (HTTP, broker,   │
 │   ...    │    │        │    │            │    │  DB, file, UI…)  │
 └──────────┘    └────────┘    └────────────┘    └──────────────────┘
```

### Key invariants

1. **Reactions are passive consumers.** A reaction's job is to (a) accept
   `QueryResult` values pushed at it, (b) optionally format them, and
   (c) dispatch them to the configured external system. Anything else
   (subscription management, ordering, backpressure, lifecycle events) 
   is handled by `ReactionBase`.

2. **The `drasi-lib` instance owns the wiring.** Queries are subscribed
   by the runtime on the reaction's behalf. A reaction receives
   `QueryResult` values through a single async method
   (`enqueue_query_result`) — it never calls
   `subscribe()`.

3. **One reaction instance, many queries.** A single reaction is
   configured with a list of query IDs. Results arrive interleaved in
   **timestamp order** through a single per-reaction priority queue.

4. **Ownership transfers to the `drasi-lib` instance.** When you
   instantiate a reaction and pass it to a `drasi-lib` instance, that
   instance takes ownership of the reaction.

5. **FFI safety matters.** When compiled as a dynamic plugin
   (`cdylib`), every trait method is dispatched across a stable C ABI
   vtable. Panics at the boundary are caught and turned into
   transient errors — but you SHOULD NOT rely on this.

### A reaction in context

The Quickstart below builds `MyReaction`. In a real application, the
reaction is one piece of a `drasi-lib` instance alongside at least one
source and one query. The application creates those pieces, transfers
ownership to `DrasiLib::builder()`, then starts the runtime.

The example below uses the published mock source crate so the moving
parts are visible without an external database. To run it, add
`drasi-source-mock = "0.2"` to your application crate's dependencies
alongside `drasi-lib`.

```rust,ignore
use drasi_lib::{DrasiLib, Query};
use drasi_source_mock::{DataType, MockSource, MockSourceConfig};
use my_reaction::{MyReaction, MyReactionConfig};

async fn run() -> anyhow::Result<()> {
    let source = MockSource::new(
        "mock-source",
        MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 1_000,
        },
    )?;

    let query = Query::cypher("counter-query")
        .query("MATCH (c:Counter) RETURN c.value AS value")
        .from_source("mock-source")
        .build();

    let reaction = MyReaction::builder("my-reaction")
        .from_query("counter-query")
        .with_config(MyReactionConfig {
            endpoint: "https://example.test/webhook".to_string(),
            timeout_ms: Some(5_000),
        })
        .build()?;

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;
    Ok(())
}
```

This guide focuses on the reaction plugin itself. The source and query
above are context: they show where the reaction fits once you have
built it.

---

## 3. Quickstart: A Working Reaction in 15 Minutes

This walkthrough produces a complete reaction crate that logs every
query result to stdout. You will then expand it in later sections.

### Prerequisites

> ✅ A recent stable Rust toolchain (`rustc --version` ≥ 1.83).  
> ✅ Familiarity with async Rust and the `tokio` ecosystem.  
> ✅ An internet connection so `cargo` can resolve `drasi-lib` and
>    its transitive dependencies from crates.io.  

You do **not** need a clone of the Drasi source tree. Your reaction
lives in its own crate and depends on the
published `drasi-lib` and `drasi-plugin-sdk` crates.

### Step 1: Create the crate

In a fresh directory of your choice:

```bash
cargo new --lib my-reaction
cd my-reaction
```

Lay out the source as follows:

```
my-reaction/
├── Cargo.toml
└── src/
    ├── lib.rs        # Public exports
    ├── config.rs     # Typed configuration struct
    └── myreaction.rs # The Reaction implementation
```

### Step 2: `Cargo.toml`

```toml
[package]
name = "my-reaction"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[lib]
crate-type = ["lib", "cdylib"]

[dependencies]
# Pin to a single drasi-lib version across your project. The version
# below was current at the time this guide was written; check
# https://crates.io/crates/drasi-lib for the latest.
drasi-lib = "0.8"
anyhow = "1"
async-trait = "0.1"
log = "0.4"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = "0.4"
```

> 🔴 **MUST** — Depend on `drasi-lib` from crates.io and pin a single
> version across your project. The dynamic plugin loader must use the
> matching SDK version; mismatched `drasi-lib` versions cause
> link-time failures and incompatible FFI vtables.

> 🟡 **SHOULD** — Declare `crate-type = ["lib", "cdylib"]` even if you
> only need the static `lib` path today. Adding `cdylib` later requires
> a rebuild but no source changes.

### Step 3: `config.rs` — typed configuration

```rust
// src/config.rs
use serde::{Deserialize, Serialize};

/// The runtime configuration for `MyReaction`.
///
/// This struct holds **resolved primitive values** — plain Rust types,
/// not `ConfigValue` envelopes. The descriptor in §7 is responsible
/// for resolving any `ConfigValue<T>` fields in the JSON DTO down to
/// plain values before constructing this struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MyReactionConfig {
    /// Endpoint URL this reaction would dispatch to. The quickstart
    /// example just logs it; later sections of this guide show how to
    /// use it for real HTTP, broker, or file dispatch.
    pub endpoint: String,

    /// Optional dispatch timeout in milliseconds. `None` means use a
    /// reaction-specific default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
```

### Step 4: `myreaction.rs` — the reaction itself

```rust
// src/myreaction.rs
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info};
use std::collections::HashMap;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use crate::config::MyReactionConfig;

pub struct MyReaction {
    // `base` is `pub(crate)` so other modules in this crate — notably
    // the descriptor in `descriptor.rs` (§7) — can call
    // `set_raw_config` on it after construction. External callers go
    // through `base_mut()`.
    pub(crate) base: ReactionBase,
    config: MyReactionConfig,
}

impl MyReaction {
    /// Convenience constructor. Most callers should use
    /// [`MyReaction::builder`] instead.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MyReactionConfig,
    ) -> Self {
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Start building a `MyReaction` with the fluent builder.
    pub fn builder(id: impl Into<String>) -> MyReactionBuilder {
        MyReactionBuilder::new(id)
    }

    /// Mutable access to the underlying `ReactionBase`. The dynamic-
    /// plugin descriptor uses this to call `set_raw_config` after
    /// construction; reaction-internal code rarely needs it.
    pub fn base_mut(&mut self) -> &mut ReactionBase {
        &mut self.base
    }
}

/// Fluent builder for [`MyReaction`].
///
/// Setter conventions follow the contract described in §6 (Builder
/// convention reference). Builds always validate before returning.
#[derive(Default)]
pub struct MyReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: MyReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl MyReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: MyReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Subscribe to multiple queries at once (replaces any previously-set list).
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Subscribe to one additional query.
    pub fn with_query(mut self, q: impl Into<String>) -> Self {
        self.queries.push(q.into());
        self
    }

    /// Alias of [`with_query`](Self::with_query) for readability at call sites.
    pub fn from_query(mut self, q: impl Into<String>) -> Self {
        self.queries.push(q.into());
        self
    }

    /// Override the priority queue capacity (default: 10000).
    pub fn with_priority_queue_capacity(mut self, n: usize) -> Self {
        self.priority_queue_capacity = Some(n);
        self
    }

    /// Set auto-start behavior (default: `true`).
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Replace the entire runtime config in one call.
    pub fn with_config(mut self, config: MyReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the endpoint field directly (convenience over `with_config`).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }

    /// Set the timeout in milliseconds.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.config.timeout_ms = Some(ms);
        self
    }

    /// Validate and construct.
    pub fn build(self) -> Result<MyReaction> {
        // Validation goes here. Cross-field invariants, URL parsing,
        // template compilation, etc. The quickstart has no required
        // fields so this is a no-op; §11 (Output Templating) shows the
        // validation pattern in detail.
        let mut params = ReactionBaseParams::new(self.id, self.queries)
            .with_auto_start(self.auto_start);
        if let Some(c) = self.priority_queue_capacity {
            params = params.with_priority_queue_capacity(c);
        }
        Ok(MyReaction {
            base: ReactionBase::new(params),
            config: self.config,
        })
    }
}

#[async_trait]
impl Reaction for MyReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "myreaction"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(ComponentStatus::Starting, Some("starting".into()))
            .await;

        // Snapshot what the spawned task needs.
        let id = self.base.id.clone();
        let endpoint = self.config.endpoint.clone();
        let queue = self.base.priority_queue.clone();
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Run a dequeue → handle loop until shutdown.
        let task = tokio::spawn(async move {
            info!("[{id}] dispatching to endpoint='{endpoint}'");
            loop {
                let arc_result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{id}] shutdown received");
                        break;
                    }
                    r = queue.dequeue() => r,
                };

                for diff in &arc_result.results {
                    match diff {
                        ResultDiff::Add { data, .. } => {
                            info!("[{id}] ADD {data}");
                        }
                        ResultDiff::Update { before, after, .. } => {
                            info!("[{id}] UPDATE {before} -> {after}");
                        }
                        ResultDiff::Delete { data, .. } => {
                            info!("[{id}] DELETE {data}");
                        }
                        ResultDiff::Aggregation { after, .. } => {
                            info!("[{id}] AGGREGATION {after}");
                        }
                        ResultDiff::Noop => {}
                    }
                }
            }
        });

        self.base.set_processing_task(task).await;

        self.base
            .set_status(ComponentStatus::Running, Some("started".into()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }
}
```

### Step 5: `lib.rs` — public exports

```rust
// src/lib.rs
pub mod config;
pub mod myreaction;

pub use config::MyReactionConfig;
pub use myreaction::{MyReaction, MyReactionBuilder};
```

### Step 6: Build it

From your crate's directory:

```bash
cargo build
```

> ✅ **Check your work** — At this point the crate should compile
> cleanly. You have:
>
> - A typed runtime configuration struct (`MyReactionConfig`).
> - A `Reaction` implementation backed by `ReactionBase`, with a
>   `pub(crate) base` field and a `base_mut()` accessor that §7 will
>   call from the descriptor.
> - A fluent `MyReactionBuilder` exposing the standard setters
>   (`with_query`, `from_query`, `with_priority_queue_capacity`,
>   `with_auto_start`, `with_config`, plus reaction-specific
>   conveniences like `with_endpoint` and `with_timeout_ms`).
> - A processing loop that drains the priority queue and exits cleanly
>   on shutdown.
> - Status transitions wired through `ReactionBase`.

You now have a buildable reaction scaffold. It demonstrates the trait,
builder, lifecycle, and descriptor shape, but it does not implement a
real transport. Later sections show the default output, templating,
dynamic loading, durable state, snapshots, identity, batching, and
tests you apply when replacing the log statements with actual dispatch.

---

## 4. Core Concepts

### 4.1 The Component Lifecycle

Every reaction follows the same state machine:

```
                 ┌─────────────┐
                 │   Stopped   │  (initial state)
                 └──────┬──────┘
                        │ start()
                 ┌──────▼──────┐
                 │   Starting  │
                 └──────┬──────┘
        ┌───────────────┼───────────────┐
        │               │               │
        │ success       │ failure       │
        │               │               │
 ┌──────▼──────┐ ┌──────▼──────┐        │
 │   Running   │ │    Error    │◄───────┘ (any uncaught failure)
 └──────┬──────┘ └──────┬──────┘
        │               │
        │ stop()        │ stop()
        │               │
 ┌──────▼──────┐        │
 │  Stopping   │◄───────┘
 └──────┬──────┘
        │
 ┌──────▼──────┐
 │   Stopped   │
 └─────────────┘
```

#### Who calls what

| Call            | Caller         | Frequency                                |
|-----------------|----------------|------------------------------------------|
| `initialize`    | Host           | Exactly once per reaction lifetime.      |
| `start`         | Host           | Once after `initialize`, then again after every `stop`. |
| `stop`          | Host           | At shutdown or when reconfiguring.       |
| `enqueue_query_result` | Host    | Many times while `Running`.              |
| `status`        | Host / API     | Anytime, including before `start`.       |
| `properties`    | Host           | On config snapshot.                      |
| `deprovision`   | Host           | At most once, when removed with `cleanup=true`. |
| `bootstrap`     | Host           | At most once per `start`, only if `needs_snapshot_on_fresh_start()` is `true`. |

> 🔴 **MUST** — `stop()` MUST be safe to call without a prior `start()`,
> and safe to call repeatedly. Always use `ReactionBase::stop_common()`
> which is idempotent.

> 🔴 **MUST** — `start()` MUST return promptly. If you need long-running
> work, spawn a tokio task and store it via
> `ReactionBase::set_processing_task()`. A blocking `start()` will hold
> up the entire `drasi-lib` instance startup.

### 4.2 Runtime-Owned Query Subscriptions

**Reactions do NOT subscribe to queries.** The `drasi-lib` instance inspects the list
returned by `query_ids()` and, after `start()` succeeds,
subscribes on your behalf. Each `QueryResult` is then handed to you by
calling `enqueue_query_result(result)`.

> 🔴 **MUST** — Implement `enqueue_query_result` (or rely on
> `ReactionBase`'s implementation). The default trait method is a
> no-op; **if you forget to delegate to `ReactionBase`, you will
> silently drop every result.**

> 🔴 **MUST NOT** — Call any subscription API yourself. There is no
> `QueryProvider` for reactions. Do not try to obtain `Query` instances
> directly; the `drasi-lib` instance owns that wiring.

### 4.3 The Priority Queue & Backpressure

`ReactionBase` owns a `PriorityQueue<QueryResult>` and forwards every
inbound result into it. The queue:

- Orders events by `QueryResult::timestamp` (monotonic per source).
- Bounds in-flight events to `priority_queue_capacity` (default 10000).
- Provides backpressure to the `drasi-lib` instance: when the queue is full, the
  blocking `enqueue_wait` strategy slows incoming events instead of
  dropping them.

You consume the queue by calling `priority_queue.dequeue().await` in
your processing task. The standard pattern is:

```rust
let arc_result = tokio::select! {
    biased;
    _ = &mut shutdown_rx => break,
    r = queue.dequeue() => r,
};
```

> 🔴 **MUST** — Use `tokio::select!` with `biased;` and the shutdown
> receiver. Without it your reaction will not stop promptly when the
> runtime calls `stop()`, and you will time out and be force-aborted.

> 🟡 **SHOULD** — Tune `priority_queue_capacity` only when you have
> measured a problem. The default of 10,000 is correct for the vast
> majority of reactions. Increase it for slow downstream systems that
> burst; decrease it to fail fast and surface backpressure.

### 4.4 The Runtime Context

When the `drasi-lib` instance adds your reaction it calls `initialize(context)` with a
`ReactionRuntimeContext` that contains everything you can possibly need
from the runtime:

| Field                | Type                                            | Always present?      |
|----------------------|-------------------------------------------------|----------------------|
| `instance_id`        | `String`                                        | Yes                  |
| `reaction_id`        | `String`                                        | Yes                  |
| `update_tx`          | mpsc sender for status updates                  | Yes                  |
| `state_store`        | `Option<Arc<dyn StateStoreProvider>>`           | If configured        |
| `identity_provider`  | `Option<Arc<dyn IdentityProvider>>`             | If configured        |
| `snapshot_fetcher`   | `Option<Arc<dyn SnapshotFetcher>>`              | If query is loaded   |

> 🔴 **MUST** — Implement `initialize` by delegating to
> `ReactionBase::initialize()`. It wires the status handle to the graph
> update channel and stashes the state store and identity provider
> internally for later use. Reading these values without delegating
> first will return `None`.

> 🟡 **SHOULD** — Do not capture the context fields directly into your
> processing task. Read them through `ReactionBase` (e.g.
> `self.base.state_store().await`) so that programmatic overrides
> (e.g. an identity provider set after construction) take effect.

---

## 5. The `Reaction` Trait

The full trait, declared in `drasi_lib::Reaction`:

```rust
#[async_trait]
pub trait Reaction: Send + Sync {
    fn id(&self) -> &str;
    fn type_name(&self) -> &str;
    fn properties(&self) -> HashMap<String, serde_json::Value>;
    fn query_ids(&self) -> Vec<String>;
    fn auto_start(&self) -> bool { true }

    async fn initialize(&self, context: ReactionRuntimeContext);
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;

    async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> { Ok(()) }
    async fn deprovision(&self) -> Result<()> { Ok(()) }
    async fn set_identity_provider(&self, _provider: Arc<dyn IdentityProvider>) {}

    fn is_durable(&self) -> bool { false }
    fn needs_snapshot_on_fresh_start(&self) -> bool { false }
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy { ReactionRecoveryPolicy::Strict }
    async fn bootstrap(&self, _ctx: BootstrapContext) -> Result<()> { Ok(()) }
}
```

The methods fall into three groups:

- **§5.1 Required** — methods every reaction MUST implement.
- **§5.2 Lifecycle** — methods the `drasi-lib` instance calls in a defined order, and
  the contract they impose on you at each call.
- **§5.3 Optional capability overrides** — methods you only touch when
  your reaction needs the capability they unlock. §12 describes each
  capability in depth; this section is the index.

### 5.1 Required methods

Every reaction MUST implement these. The right-hand columns describe
what the runtime calls each method for and what `ReactionBase` provides
when you delegate; doing so is SHOULD throughout.

| Method | Returns | What the runtime calls it for | What `ReactionBase` provides |
|---|---|---|---|
| `id` | `&str` | Per-message log routing; reaction lookup by id. | Returns `&self.base.id`. |
| `type_name` | `&str` | Type identification in inspection APIs and runtime logs. | None — return a stable lowercase kebab-case literal that matches your `ReactionPluginDescriptor::kind()`. |
| `properties` | `HashMap<String, serde_json::Value>` | Persistence hook — snapshotted by the `drasi-lib` instance. | `self.base.properties_or_serialize(&dto)` returns the raw config JSON for descriptor-built reactions, otherwise serializes the DTO. |
| `query_ids` | `Vec<String>` | The `drasi-lib` instance subscribes to these queries on the reaction's behalf after `start()` returns. | Returns `self.base.queries.clone()`. |
| `initialize` | `()` | Single dependency-injection call: wires the status handle to the runtime's graph channel; stashes state-store and identity-provider references. | `self.base.initialize(context).await` does the full wiring. |
| `start` | `Result<()>` | Begin processing. Returning `Err` puts the reaction into `Error`. | Provides `create_shutdown_channel`, `priority_queue`, `set_processing_task`, `set_status`. |
| `stop` | `Result<()>` | Graceful teardown. Must be idempotent. | `self.base.stop_common().await` sends the shutdown signal, waits up to 2 s, aborts otherwise, drains the queue, and sets status to `Stopped`. |
| `status` | `ComponentStatus` | Anytime status read, including before `start`. | Returns `self.base.get_status().await`. |
| `enqueue_query_result` | `Result<()>` | The runtime's push entry-point. The trait default is a **no-op that silently drops results** — you MUST override it. | `self.base.enqueue_query_result(result).await` pushes onto the priority queue with the appropriate backpressure strategy. |

Per-method details and the contracts that go beyond the table:

#### `id` — **MUST**

Return the unique identifier passed in at construction. SHOULD delegate
to `&self.base.id`.

#### `type_name` — **MUST**

Return a stable, lowercase, kebab-case string identifying the **kind**
of reaction. This MUST match the `kind()` of your
`ReactionPluginDescriptor` when packaged as a dynamic plugin.

#### `properties` — **MUST**

Return **all** configuration properties for this reaction, **including
secrets**.

> 🔴 **MUST** — Include sensitive values (passwords, tokens, etc.).
> `properties()` is the **persistence hook** used by the `drasi-lib` instance to write
> the live component graph back to disk. There is no separate config
> cache — the live instance is the single source of truth. Any key you
> omit will be **lost** on the next save and the reaction will fail to
> restart.
>
> This method is not an external API. The application embedding `drasi-lib` is responsible for
> protecting the config file at rest.

> 🟡 **SHOULD** — If you take a descriptor-built reaction, call
> `ReactionBase::properties_or_serialize(&dto)`. It returns the
> original raw config JSON when set (the descriptor path), and
> serializes a typed DTO otherwise (the embedded path). This preserves
> `ConfigValue` envelopes for lossless round-trips.

#### `query_ids` — **MUST**

Return the list of query IDs this reaction is configured for. Used by the
runtime to set up subscriptions after `start()` returns. SHOULD return
`self.base.queries.clone()`.

#### `initialize(context)` — **MUST**

Called exactly once by the `drasi-lib` instance when adding the reaction. SHOULD
delegate to `self.base.initialize(context).await`. Do not perform
expensive work here; this method is synchronous from the runtime's
perspective.

#### `start()` — **MUST**

Start your processing loop. **Must return promptly.** Long-running
work belongs in a spawned task whose handle is stored via
`ReactionBase::set_processing_task()`. The standard recipe:

1. Set status to `Starting`.
2. Create a shutdown receiver with `create_shutdown_channel().await`.
3. Snapshot whatever the spawned task needs (queue handle, config, id).
4. `tokio::spawn` the processing loop.
5. Call `set_processing_task(handle).await`.
6. Set status to `Running`.

If start-up genuinely fails (cannot connect, invalid config detected
late), set status to `Error` and return an `Err`. The `drasi-lib` instance will surface
this to the operator.

#### `stop()` — **MUST**

Stop processing. SHOULD delegate to `self.base.stop_common().await`,
which sends the shutdown signal, waits up to 2 seconds for graceful
exit, aborts the task otherwise, drains the queue, and sets status to
`Stopped`.

> 🔴 **MUST** — Be idempotent. Calling `stop()` twice or before
> `start()` MUST NOT panic.

#### `status()` — **MUST**

Return the current status. SHOULD delegate to
`self.base.get_status().await`.

#### `enqueue_query_result(result)` — **MUST**

This is the runtime's push entry-point. The trait's default
implementation is a **no-op that silently drops results**. You MUST
override it (or inherit the override from a wrapper) — typically by
delegating to `self.base.enqueue_query_result(result).await`, which
pushes the result into the priority queue with the appropriate
backpressure strategy.

Returning `Ok(())` from `enqueue_query_result` means the reaction has
accepted the event for processing. It does **not** mean the event has
already been dispatched downstream; for the standard `ReactionBase`
path, dispatch happens later when the processing task drains the
priority queue.

### 5.2 Lifecycle — the runtime call sequence

The `drasi-lib` instance calls the reaction's methods in a defined order. Knowing the
order tells you what state each method can assume.

```
construction          MyReaction::builder(id)...build()
                      └── you hold the reaction, drasi-lib does not
                          ─────────────────────────────────────
add to drasi-lib      drasi.add_reaction(reaction)
                      └── drasi-lib calls initialize(ctx)
                          ├── ReactionBase wires the status handle
                          │   to the component graph
                          └── ReactionBase stashes state_store and
                              identity_provider (if present)
                          ─────────────────────────────────────
optional auto-start   if reaction.auto_start() { drasi-lib calls start() }
                      └── reaction sets status Starting → Running
                      └── reaction spawns its processing task
                      └── reaction returns Ok(())
                          ─────────────────────────────────────
runtime subscriptions drasi-lib subscribes to each query in query_ids()
                      └── drasi-lib begins forwarding results via
                          enqueue_query_result(qr)
                          ─────────────────────────────────────
steady state          drasi-lib pushes results via enqueue_query_result(qr)
                      reaction's processing task drains its priority
                      queue and dispatches to the external system
                          ─────────────────────────────────────
shutdown              drasi-lib calls stop()
                      └── ReactionBase::stop_common()
                          ├── sends the shutdown signal
                          ├── waits up to 2 s for graceful exit
                          ├── aborts the processing task on timeout
                          └── drains the priority queue
                      └── status: Stopped
                          ─────────────────────────────────────
optional teardown     drasi-lib calls deprovision()
                      (only if removed with cleanup=true)
```

Key invariants the lifecycle imposes:

- `initialize` runs **before** `start`. You can read context-injected
  state (state store, identity provider) starting in `start`, but not
  in `new` or `builder().build()`.
- `start` runs **before** the `drasi-lib` instance subscribes to any query. The first
  `enqueue_query_result` call you receive arrives *after* `start`
  returns `Ok(())`, never during it.
- `stop` may run with or without a prior `start`. It MUST be safe in
  both cases. `ReactionBase::stop_common` is.
- `bootstrap`, if you opt in via
  [`needs_snapshot_on_fresh_start`](#53-optional-capability-overrides),
  runs **between** `start` and the first `enqueue_query_result`.

#### Who calls what — quick reference

| Call            | Caller         | Frequency                                |
|-----------------|----------------|------------------------------------------|
| `initialize`    | `drasi-lib` instance | Exactly once per reaction lifetime. |
| `start`         | `drasi-lib` instance | Once after `initialize`, then again after every `stop`. |
| `stop`          | `drasi-lib` instance | At shutdown or when reconfiguring. |
| `enqueue_query_result` | `drasi-lib` instance | Many times while `Running`. |
| `status`        | `drasi-lib` instance / API | Anytime, including before `start`. |
| `properties`    | `drasi-lib` instance | On config snapshot. |
| `deprovision`   | `drasi-lib` instance | At most once, when removed with `cleanup=true`. |
| `bootstrap`     | `drasi-lib` instance | At most once per `start`, only if `needs_snapshot_on_fresh_start()` is `true`. |

### 5.3 Optional capability overrides

Override these only if you need the capability they unlock. Each entry
forward-references the section in §12 (Optional Capabilities) that
covers the capability in depth.

| Method | Default | What overriding it does | See |
|---|---|---|---|
| `auto_start` | `true` | Returning `false` makes the `drasi-lib` instance *not* auto-start the reaction; the operator or application must start it explicitly. | This page. |
| `deprovision` | no-op | Lets you clean up **external** state (output topics, webhook registrations, etc.) when the reaction is removed with `cleanup=true`. | [§12.1 Deprovisioning](#121-deprovisioning) |
| `is_durable` | `false` | Returning `true` tells the `drasi-lib` instance you need a durable `StateStoreProvider`; volatile in-memory state stores will be rejected. | [§12.2 Durable State & Checkpoints](#122-durable-state--checkpoints) |
| `default_recovery_policy` | `Strict` | Choose `AutoSkipGap` to accept data loss on a checkpoint gap, or `AutoReset` to force a fresh bootstrap. | [§12.3 Recovery Policy](#123-recovery-policy) |
| `needs_snapshot_on_fresh_start` | `false` | Returning `true` causes the `drasi-lib` instance to call `bootstrap()` on first start (and after `AutoReset`). | [§12.4 Bootstrap on Fresh Start](#124-bootstrap-on-fresh-start) |
| `bootstrap(ctx)` | no-op | The handler the `drasi-lib` instance calls when bootstrap is needed; use `ctx.fetch_snapshot()` and `ctx.write_checkpoint()`. | [§12.4 Bootstrap on Fresh Start](#124-bootstrap-on-fresh-start) |
| `set_identity_provider(p)` | no-op | Lets the runtime inject a `dyn IdentityProvider` after construction; takes precedence over context-injected providers. | [§8 Secrets, Environment Variables, and Identity](#8-secrets-environment-variables-and-identity) |

Per-method notes:

#### `auto_start` — **MAY**

Default `true`. Override to return `false` if your reaction should
only start when the operator explicitly requests it (e.g. you need
configuration injected at runtime before processing can begin).
Delegate to `self.base.get_auto_start()` if you want operators to
control it through `MyReactionBuilder::with_auto_start(...)`.

#### `deprovision` — **MAY**

Default no-op. Override only if removing your reaction from the
system requires cleaning up **external** state. The `drasi-lib` instance calls this
when the reaction is removed with `cleanup=true`. Errors are logged
but do not block removal.

If you implement it and use the state store, SHOULD call
`self.base.deprovision_common().await` first to clear the reaction's
state-store partition.

#### `set_identity_provider(provider)` — **MAY**

Override if your reaction authenticates to an external system using
named identity providers from declarative config. SHOULD delegate to
`self.base.set_identity_provider(provider).await`. Providers set this
way take precedence over any context-injected provider.

#### `is_durable` — **MAY**

Default `false`. Return `true` if your reaction persists checkpoints
to a state store. The `drasi-lib` instance validates at startup that a **durable**
`StateStoreProvider` is configured when this is `true`; if only a
volatile in-memory store is present, the reaction will not be allowed
to start.

#### `needs_snapshot_on_fresh_start` — **MAY**

Default `false`. Return `true` if your reaction requires a full
snapshot of every subscribed query on first start (e.g. it
materializes a view that needs to be initialized before incremental
updates can be applied). The `drasi-lib` instance will then call `bootstrap()` with a
`BootstrapContext`.

#### `default_recovery_policy` — **MAY**

Default `ReactionRecoveryPolicy::Strict` — fail startup if the outbox
cannot serve the requested checkpoint position. The other two policies
are:

- `AutoSkipGap` — accept potential data loss; resume from the latest
  available outbox entry. Appropriate for fire-and-forget reactions
  (logs, dashboards, metrics) where missing a window is preferable to
  refusing to start.
- `AutoReset` — wipe the checkpoint and re-bootstrap from a full
  snapshot. Appropriate for reactions that materialize a derived state
  that can be cheaply rebuilt.

The default can be overridden per-instance via
`ReactionBaseParams::with_recovery_policy()`.

#### `bootstrap(ctx)` — **MAY**

Called only when `needs_snapshot_on_fresh_start()` returns `true`, or
when recovery requires a reset. Use `ctx.fetch_snapshot()` to stream
the query's current result set, then call `ctx.write_checkpoint()` to
mark the starting sequence.

> 🔴 **MUST NOT** — Retain the `BootstrapContext` past the call. The
> backend may contain raw FFI pointers that are invalidated when
> `bootstrap()` returns. Storing the context and using it later is
> undefined behavior.

### 5.4 Error handling

Drasi's reaction error model is intentionally minimal: methods return
`anyhow::Result<()>` and the `drasi-lib` instance classifies failures by **when** they
happen, not by what type they are.

| Where the error happens             | Host behavior                                                                |
|-------------------------------------|------------------------------------------------------------------------------|
| `Reaction::new` / builder           | Caller's problem — surfaced directly to the operator.                        |
| Descriptor `create_reaction`        | Surfaced via the management API. The reaction is not added.                  |
| `start()` returns `Err`             | Reaction status set to `Error`. Operator must intervene.                     |
| `enqueue_query_result` returns `Err`| Logged. Backpressure may apply. The `drasi-lib` instance does not retry.     |
| Inside your processing task         | Your choice. The `drasi-lib` instance has no visibility once you have spawned the task. |
| `bootstrap` returns `Err`           | Treated as a fatal start failure.                                            |

#### Inside the processing task

> 🔴 **MUST NOT** — Let a panic escape your processing task. Catch and
> log any panic from `tokio::spawn`-wrapped code, or rely on
> `JoinHandle::abort` semantics in your shutdown.

> 🟡 **SHOULD** — Differentiate transient and permanent failures in
> your code:
>
> - **Transient** (network blip, broker temporarily unavailable, rate
>   limit): log, sleep with backoff, retry. Do not advance the
>   checkpoint.
> - **Permanent** (4xx with body, schema mismatch, encoding failure):
>   log at error level with the offending payload, advance the
>   checkpoint (or route to a dead-letter destination if you support
>   one), continue.

> 🔴 **MUST** — When a template render fails for a specific row, do
> not abort the whole batch. Log the offending row, fall back to the
> default output structure for that row, and continue.

---

## 6. `ReactionBase` — Reference

`ReactionBase` (`drasi_lib::reactions::common::base::ReactionBase`)
implements every cross-cutting concern the trait reference in §5
referred to. It owns the reaction `id` and `queries`, a
`PriorityQueue<QueryResult>` for ordered backpressured ingestion, a
status handle that broadcasts to the component graph, the
shutdown channel used by your processing loop, optional state-store
and identity-provider references injected from the runtime context,
and optional raw-config JSON for lossless persistence round-trips.

### Public surface

Methods you call from your `Reaction` implementation, alphabetical:

| Method | Purpose |
|---|---|
| `create_shutdown_channel()` | Allocate the oneshot receiver passed into your processing loop. |
| `deprovision_common()` | Clear the reaction's state-store partition (use from `Reaction::deprovision`). |
| `enqueue_query_result(result)` | Push an inbound result into the priority queue. |
| `get_auto_start()` | Read the auto-start flag set via `ReactionBaseParams::with_auto_start`. |
| `get_status()` | Read the current `ComponentStatus`. |
| `identity_provider()` | Lazy accessor for the `IdentityProvider` injected via context or `set_identity_provider`. |
| `initialize(context)` | Wire the status handle to the component graph and stash injected services. Call from `Reaction::initialize`. |
| `new(params)` | Construct from `ReactionBaseParams`. |
| `properties_or_serialize(&dto)` | Build the `properties()` map (raw config if descriptor-built, DTO serialization otherwise). |
| `read_all_checkpoints()` | Bulk-read every checkpoint for this reaction (state store required). |
| `read_checkpoint(query_id)` | Read one checkpoint (state store required). |
| `run_standard_loop(rx, ckpts, handler)` | Optional convenience: dedup-aware dequeue → handler → checkpoint loop. |
| `set_identity_provider(provider)` | Stash a programmatically-supplied identity provider; takes precedence over the context one. |
| `set_processing_task(handle)` | Register your `tokio::spawn` handle so `stop_common` can join or abort it. |
| `set_raw_config(json)` | Stash the raw config JSON (used by descriptor `create_reaction`). |
| `set_status(status, msg)` | Set local status AND notify the component graph. |
| `state_store()` | Lazy accessor for the `StateStoreProvider` injected via context. |
| `status_handle()` | Clone a `ComponentStatusHandle` into spawned tasks. |
| `stop_common()` | Send shutdown, await for up to 2 s, abort otherwise, drain the queue, set status to `Stopped`. |
| `write_checkpoint(query_id, ckpt)` | Persist a checkpoint (state store required). |

Public fields you may read or pass to spawned tasks: `id`, `queries`,
`priority_queue`.

### `ReactionBaseParams`

```rust
let params = ReactionBaseParams::new(id, queries)
    .with_priority_queue_capacity(20_000)                       // optional, default 10000
    .with_auto_start(false)                                     // optional, default true
    .with_recovery_policy(ReactionRecoveryPolicy::AutoSkipGap); // optional
```

> 🟡 **SHOULD** — Always compose with `ReactionBase` rather than
> re-implementing these patterns. Behavior diverges silently when
> reactions roll their own status handling, queue draining, or
> shutdown coordination.

### Config and Builder conventions

`ReactionBase` handles the cross-cutting plumbing; you still own the
shape of your reaction's typed config and how callers construct
instances. The conventions below keep every reaction's surface
predictable.

#### Typed config struct

> 🔴 **MUST** — Define a single owned `Config` struct that derives
> `serde::{Serialize, Deserialize}` and `Default`. The struct
> represents one validated instance of your reaction. Fields hold
> resolved primitive values; the `ConfigValue<T>` envelopes used in
> the JSON DTO live only in the descriptor module (see
> [§7](#7-packaging-as-a-dynamic-plugin)).

> 🟡 **SHOULD** — Implement a `validate(&self) -> anyhow::Result<()>`
> method on the config struct for cross-field invariants (URL
> parsing, mutually exclusive fields, etc.) and call it from your
> builder.

#### Builder convention

> 🟡 **SHOULD** — Provide a builder accessed via
> `MyReaction::builder(id) -> MyReactionBuilder`, with fluent setters
> and a `build() -> anyhow::Result<MyReaction>` method that validates
> before returning.

Setter names are not arbitrary. Use these conventions so every
reaction's builder looks like every other reaction's builder:

| Setter | Purpose |
|---|---|
| `with_queries(Vec<String>)` | Replace the subscribed-query list. |
| `with_query(impl Into<String>)` | Append one subscribed query. |
| `from_query(impl Into<String>)` | Alias of `with_query`; reads naturally at call sites (e.g. `…from_query("orders")…`). |
| `with_priority_queue_capacity(usize)` | Override the default queue capacity (10 000). |
| `with_auto_start(bool)` | Toggle auto-start (default `true`). |
| `with_config(Config)` | Replace the entire runtime config in one call. |
| `with_recovery_policy(ReactionRecoveryPolicy)` | Override the reaction's default recovery policy per-instance. See [§12.3 Recovery Policy](#123-recovery-policy). |
| `with_<field>(…)` | One convenience setter per top-level config field, so common cases are a single fluent call. |

The Quickstart in §3 shows a complete implementation of this contract.

> 🟡 **SHOULD** — Validate everything at `build()` time (templates,
> required fields, route coverage). It is far better to fail at
> construction than at dispatch.

---

## 7. Packaging as a Dynamic Plugin

A reaction can be loaded by a dynamic plugin loader at runtime from a
`cdylib`-compiled shared library. The packaging is built on three
pieces:

1. A **configuration DTO** that derives `utoipa::ToSchema` so the
   loader can validate user-supplied JSON before constructing the
   reaction.
2. A **`ReactionPluginDescriptor`** that tells the loader how to build
   an instance from validated config JSON.
3. The **`drasi_plugin_sdk::export_plugin!`** macro that emits the
   stable C ABI entry points.

### Cargo.toml additions

```toml
[lib]
crate-type = ["lib", "cdylib"]

[dependencies]
# Pin the same versions of drasi-lib and drasi-plugin-sdk that the
# dynamic plugin loader you intend to load this plugin into was built against.
drasi-lib = "0.8"
drasi-plugin-sdk = "0.8"
utoipa = { version = "4", features = ["chrono"] }
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[features]
dynamic-plugin = []
```

The `dynamic-plugin` feature is conventional but optional. It lets you
gate the `export_plugin!` call so the same crate works both as an
embedded library and as a `cdylib`.

### The configuration DTO

> 🔴 **MUST** — Define a DTO struct separate from your runtime
> `Config` struct. The DTO uses `serde(rename_all = "camelCase")` and
> `utoipa::ToSchema`; your runtime config is whatever shape is most
> ergonomic for Rust code. Mapping happens in `create_reaction`.

```rust
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::myreaction::MyReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct MyReactionConfigDto {
    /// The outbound endpoint URL (supports env-var and secret resolution).
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    /// Request timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(MyReactionConfigDto)))]
struct MyReactionSchemas;
```

> 🟡 **SHOULD** — Wrap fields that should support environment-variable
> or secret resolution in `ConfigValue<T>`. Users can then write
> `"${MY_ENV}"`, `{"kind":"EnvironmentVariable","name":"MY_ENV"}`, or
> `{"kind":"Secret","name":"my-secret"}` and the SDK will resolve them
> at construction time. Use `#[schema(value_type = ConfigValueString)]`
> (or `ConfigValueU16` etc.) so the generated OpenAPI is accurate.

> 🟡 **SHOULD** — Schema-name your DTO with the namespace
> `reaction.<kind>.<TypeName>` to avoid collisions when many plugins
> contribute to the same OpenAPI document.

### The descriptor

```rust
// src/descriptor.rs
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;

// Bring our crate's runtime types into scope.
use crate::config::MyReactionConfig;
use crate::myreaction::MyReactionBuilder;

pub struct MyReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for MyReactionDescriptor {
    fn kind(&self) -> &str { "myreaction" }

    fn config_version(&self) -> &str { "1.0.0" }

    fn config_schema_name(&self) -> &str { "reaction.myreaction.MyReactionConfig" }

    fn config_schema_json(&self) -> String {
        let api = MyReactionSchemas::openapi();
        serde_json::to_string(
            &api.components.as_ref().expect("openapi components").schemas,
        )
        .expect("schema serialization")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        // 1. Deserialize JSON → DTO (with `ConfigValue` envelopes).
        let dto: MyReactionConfigDto = serde_json::from_value(config_json.clone())?;

        // 2. Resolve `ConfigValue` envelopes down to plain values.
        //    See §8 for how `Secret` resolution actually works.
        let mapper = DtoMapper::new();
        let endpoint  = mapper.resolve_string(&dto.endpoint).await?;
        let timeout_ms = dto.timeout_ms;

        // 3. Construct the runtime config (plain primitive values).
        let cfg = MyReactionConfig { endpoint, timeout_ms };

        // 4. Build the reaction through its builder.
        let mut reaction = MyReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_config(cfg)
            .build()?;

        // 5. Preserve raw config for lossless properties() round-trips.
        //    The reaction exposes `base_mut()` so we can reach the
        //    ReactionBase without making the field itself fully public.
        reaction.base_mut().set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}
```

Note the split: the runtime `MyReactionConfig` (§3) carries resolved
primitive values; the `MyReactionConfigDto` (above) carries `ConfigValue`
envelopes. The descriptor's `create_reaction` is the only place these
two shapes meet — it deserializes the DTO, resolves each `ConfigValue`,
and constructs the runtime config.

> 🔴 **MUST** — `kind()` MUST return the same string as your
> `Reaction::type_name()`. Mismatches can let the dynamic plugin
> loader load the library but fail when resolving the reaction by kind.

> 🔴 **MUST** — `config_version()` MUST follow semver. Bump the major
> on any breaking DTO change, the minor for additive optional fields,
> and the patch for documentation-only edits.

> 🔵 **MAY** — Override `display_name()`, `display_description()`, and
> `display_icon()` on the descriptor to improve schema-driven UIs or
> plugin catalogs. The defaults are safe (`display_name()` and
> `display_icon()` return `kind()`, `display_description()` returns an
> empty string).

> 🟡 **SHOULD** — After building, call
> `reaction.base_mut().set_raw_config(config_json.clone())`. This
> preserves the user's original `ConfigValue` envelopes so
> `properties()` round-trips faithfully when the `drasi-lib` instance
> snapshots its configuration.

### The `export_plugin!` macro

In `lib.rs`:

```rust
pub mod config;
pub mod descriptor;
pub mod myreaction;

pub use config::MyReactionConfig;
pub use myreaction::{MyReaction, MyReactionBuilder};

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "myreaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version  = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors   = [],
    reaction_descriptors = [descriptor::MyReactionDescriptor],
    bootstrap_descriptors = [],
);
```

This macro generates two C ABI symbols (`drasi_plugin_init` and
`drasi_plugin_metadata`) that the dynamic plugin loader looks up at
load time, along with a per-plugin tokio runtime and lifecycle hooks. A single crate
MAY export multiple descriptor types in one call.

### Build

From your crate's directory:

```bash
cargo build --features dynamic-plugin
```

The resulting shared library is at `target/debug/` (or `target/release/`
if you build with `--release`), named after your crate. For a crate
named `my-reaction` the file is, depending on OS:

```
target/debug/libmy_reaction.so      # Linux
target/debug/libmy_reaction.dylib   # macOS
target/debug/my_reaction.dll        # Windows
```

Note that Cargo lowercases the package name and replaces hyphens with
underscores when forming the library file name.

Copy the artifact into the plugin directory of the process that loads
Drasi plugins. Discovery is automatic on next startup. The dynamic
plugin loader checks SDK version compatibility before loading.

### FFI compatibility — what you need to know

The `Reaction` trait crosses FFI as a vtable
(`drasi_plugin_sdk::ffi::ReactionVtable`). Every method is dispatched
through a function pointer in this table. Two consequences:

- **Both sides MUST be built with the exact same `drasi-plugin-sdk`
  version** (and a compatible Rust toolchain). Mismatched versions are
  rejected at load time.
- **Panics in your trait methods are caught at the boundary** and
  reported as transient errors. This is a safety net, not a license to
  panic — your reaction SHOULD never panic in normal operation.

---

## 8. Secrets, Environment Variables, and Identity

The plugin SDK gives you three ways to inject sensitive or environment-
dependent values into a reaction:

### `ConfigValue<T>` (most common)

Wrap any DTO field in `ConfigValue<T>` to accept either a static value,
an environment-variable reference, or a named secret reference:

```rust
#[derive(utoipa::ToSchema, Serialize, Deserialize)]
pub struct MyDto {
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub api_token: ConfigValue<String>,
}
```

Resolve with `DtoMapper`:

```rust
let mapper = DtoMapper::new();
let endpoint  = mapper.resolve_string(&dto.endpoint).await?;
let api_token = mapper.resolve_string(&dto.api_token).await?;
```

Accepted user formats:

```yaml
endpoint:  "https://example.com"                # Static
endpoint:  "${MY_ENDPOINT:-http://localhost}"   # EnvironmentVariable with default
endpoint:  { kind: EnvironmentVariable, name: MY_ENDPOINT, default: "http://localhost" }
api_token: { kind: Secret, name: my-api-token } # Resolved via the secret-store provider
```

### How `Secret` values are actually resolved

> 🔴 **MUST** — Understand the resolver model before writing code that
> uses `ConfigValue::Secret`. A `DtoMapper` constructed via
> `DtoMapper::new()` resolves `Static` and `EnvironmentVariable`
> values out of the box, but **does not resolve `Secret` values on
> its own** — `Secret` resolution requires that a `SecretResolver`
> has been registered into the SDK's global resolver registry before
> the descriptor's `create_reaction` runs.

In a well-configured application or Drasi server process, the
application is responsible for registering a `SecretResolver`
(typically backed by its secret store) at process startup, via:

```rust,ignore
drasi_plugin_sdk::register_secret_resolver(my_resolver);
```

Reaction authors do **not** normally call `register_secret_resolver`
themselves — the application owns the secret store and decides which
resolver your dynamic plugin gets to use. If a `ConfigValue::Secret`
reaches your descriptor and no `SecretResolver` has been registered, the
`DtoMapper::resolve_*` call returns a `ResolverError` and the
reaction fails construction.

If your reaction genuinely needs to control its own secret
resolution — for example because it ships with a built-in resolver
for a specific store and refuses to fall back to whatever the
application provides — construct the mapper through `DtoMapper::with_resolver(...)`
and pass an instance of `ValueResolver`. The prelude re-exports
`ValueResolver`, `SecretResolver`, `EnvironmentVariableResolver`,
`ResolverError`, and `register_secret_resolver` for this purpose.

> 🟡 **SHOULD** — Document in your reaction's README which forms of
> `ConfigValue` it expects. If your reaction is only ever deployed
> with `Static` or `EnvironmentVariable` values, say so explicitly so
> operators do not have to discover the constraint through a runtime
> error.

### `IdentityProvider`

For OAuth-style flows, managed identities, or per-cloud credential
acquisition. Provided to your reaction either via the runtime context
or programmatically via `set_identity_provider`. Use it via
`self.base.identity_provider().await`.

> 🟡 **SHOULD** — Reactions that authenticate to external systems
> using a credential that must be refreshed or rotated at runtime
> SHOULD wire identity through `set_identity_provider`. Reactions
> that authenticate with a static credential (an API key, a bearer
> token) supplied directly in their config can ignore the identity
> path entirely.

> 🟡 **SHOULD** — Prefer `ConfigValue<String>` for simple static
> credentials. Reach for `IdentityProvider` when the credential must
> be acquired or refreshed at runtime.

### `properties()` and persistence

The `drasi-lib` instance calls `properties()` to snapshot the reaction's state to disk.

> 🔴 **MUST** — Return the **raw config JSON** when constructed via a
> descriptor (`ReactionBase::properties_or_serialize()` handles this).
> This preserves the `ConfigValue` envelopes so secrets are not
> materialized on disk — only references to them.

> 🔴 **MUST NOT** — Filter secrets out of `properties()`. Doing so
> breaks the persistence round-trip. The application embedding `drasi-lib` is responsible for
> protecting the file at rest; you are responsible for round-tripping
> the configuration faithfully.

---

## 9. The Query Result Data Contract

Every result your reaction receives is a `QueryResult`. Its shape is
the **contract** between Drasi and you; do not try to reinterpret it.

### Rust types

```rust
pub struct QueryResult {
    pub query_id: String,
    pub sequence: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<ResultDiff>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub profiling: Option<ProfilingMetadata>,
}

pub enum ResultDiff {
    Add    { data: serde_json::Value,                                            row_signature: u64 },
    Delete { data: serde_json::Value,                                            row_signature: u64 },
    Update { data: serde_json::Value, before: serde_json::Value, after: serde_json::Value,
             grouping_keys: Option<Vec<String>>, row_signature: u64 },
    Aggregation { before: Option<serde_json::Value>, after: serde_json::Value,    row_signature: u64 },
    Noop,
}
```

### Semantics

- `query_id` identifies which subscribed query produced the result. It
  may be a bare ID or dotted (`namespace.source.query`).
- `sequence` is a monotonic per-query identifier of this emission.
  Reactions that checkpoint MUST persist this value.
- `timestamp` orders events globally in the priority queue.
- `results` is a batch of diffs; it may be empty (treat as a no-op).
- `metadata` is reserved for source/query-supplied context. It MAY be
  empty.
- `profiling` is `Some` only when profiling is enabled by the `drasi-lib` instance.

> 🔴 **MUST** — Treat `ResultDiff::Noop` and empty `results` as
> "nothing to do" and skip them silently.

> 🔴 **MUST** — Match on every `ResultDiff` variant. Adding new
> variants is a breaking change in `drasi-lib` — but failing to handle
> existing ones (especially `Aggregation`) is a silent data-loss bug.

For the canonical JSON shape — including how variants are tagged on
the wire — see [Appendix A](#appendix-a--full-typespec-schemas).

---

## 10. Default Output Structure (SHOULD)

The previous section described what your reaction *receives*: an
in-memory `QueryResult`, with a `sequence: u64` field set by the
runtime. This section describes what your reaction *sends* — the
canonical JSON envelope on the wire. The two are related but not
identical; in particular the in-memory `sequence` field is rendered
as `sequenceId` on the wire to fit the camelCase convention used
throughout the default envelope.

A reaction that does not support templating, or whose user has not
configured one, still needs a payload to send. To keep behavior
consistent and predictable across reactions, there is a canonical
default shape your reaction SHOULD use.

> 🟡 **SHOULD** — When templating is absent or no template applies,
> your reaction SHOULD emit one of the documented envelopes below in
> JSON form, with `application/json` content type.

> 🟡 **SHOULD** — Use camelCase JSON field names for every key your
> reaction emits in the default envelope. The names below are the
> defaults; deviating from them defeats the point of having a default.

### The envelope shape

The default envelope mirrors the structure of the underlying
`QueryResult` (see [§9](#9-the-query-result-data-contract)).
Each emitted item carries:

- `queryId` — the originating query id (as received).
- `sequenceId` — the monotonic per-query sequence number identifying
  this emission.
- `timestamp` — the RFC3339 event timestamp.
- `operation` — `"ADD"`, `"UPDATE"`, or `"DELETE"`.
- `after` — the post-change row. Present on `ADD` and `UPDATE`.
- `before` — the pre-change row. Present on `UPDATE` and `DELETE`.
- `metadata` — optional source/query metadata; SHOULD be omitted when
  empty.

The presence rules for `before` / `after` deliberately mirror the
shape carried by `QueryResult` and its `ResultDiff` variants:

| `operation` | `after`             | `before`            |
|-------------|---------------------|---------------------|
| `ADD`       | the new row         | (omitted)           |
| `UPDATE`    | the post-change row | the pre-change row  |
| `DELETE`    | (omitted)           | the deleted row     |

This shape is captured by the `ReactionEnvelope` TypeSpec model
([Appendix A](#appendix-a--full-typespec-schemas)).

### The three dispatch patterns

A `QueryResult` carries a batch of diffs in its `results` array. Your
reaction has three reasonable ways to dispatch that data downstream;
the right one depends on what your transport rewards and what the
operator needs.

#### Pattern A — one message per `ResultDiff`

This is the simplest and most common default. Each individual diff
becomes one envelope on the wire.

JSON examples (ADD, UPDATE, DELETE respectively):

```json
{
  "queryId": "active-users",
  "sequenceId": 42,
  "timestamp": "2026-06-09T19:06:44.123Z",
  "operation": "ADD",
  "after": { "id": "u1", "name": "Ada" }
}
```

```json
{
  "queryId": "active-users",
  "sequenceId": 43,
  "timestamp": "2026-06-09T19:06:45.000Z",
  "operation": "UPDATE",
  "before": { "id": "u1", "name": "Ada" },
  "after":  { "id": "u1", "name": "Ada Lovelace" }
}
```

```json
{
  "queryId": "active-users",
  "sequenceId": 44,
  "timestamp": "2026-06-09T19:06:46.000Z",
  "operation": "DELETE",
  "before": { "id": "u1", "name": "Ada Lovelace" }
}
```

This is a good default for streaming transports (SSE, websockets,
topic-per-event brokers, file lines, etc.).

#### Pattern B — one message per `QueryResult`

Preserves the "this batch of diffs came from one query emission"
grouping. The envelope is a single object whose `results` field is an
array of the same per-diff items as Pattern A.

```json
{
  "queryId": "active-users",
  "sequenceId": 42,
  "timestamp": "2026-06-09T19:06:44.123Z",
  "results": [
    { "operation": "ADD",    "after":  { "id": "u1", "name": "Ada" } },
    { "operation": "UPDATE", "before": { "id": "u2", "name": "Bob" },
                             "after":  { "id": "u2", "name": "Robert" } },
    { "operation": "DELETE", "before": { "id": "u3", "name": "Cleo" } }
  ]
}
```

This is a good default for request/response transports where one
QueryResult naturally maps to one HTTP call or one broker publish.

#### Pattern C — batched envelopes

For high-throughput reactions that accumulate events before dispatch
(rate-limited HTTP APIs, bulk inserts, write-coalescing brokers,
adaptive batchers), the reaction SHOULD send a batch container whose
single field `batch` is an array of envelope instances of Pattern A
**or** Pattern B (pick one shape and stay with it).

Pattern C batching Pattern A items:

```json
{
  "batch": [
    { "queryId": "users",   "sequenceId": 100, "timestamp": "...",
      "operation": "ADD",    "after":  { "id": "u9" } },
    { "queryId": "users",   "sequenceId": 101, "timestamp": "...",
      "operation": "DELETE", "before": { "id": "u4" } },
    { "queryId": "orders",  "sequenceId":  17, "timestamp": "...",
      "operation": "UPDATE", "before": { "id": "o7", "state": "pending" },
                             "after":  { "id": "o7", "state": "paid" } }
  ]
}
```

Pattern C batching Pattern B items is identical, except each batch
member carries its own `results` array.

The `batch` field SHOULD always be a JSON array, even if the reaction
flushed only a single item — this lets downstream consumers parse
batches without special-casing the singleton.

### How to choose

| Situation                                                  | Default to     |
|------------------------------------------------------------|----------------|
| Streaming transport (SSE, websocket, topic-per-event)      | Pattern A      |
| One request per emission (most HTTP webhooks, most brokers)| Pattern B      |
| Throughput-bound dispatch with explicit batching window    | Pattern C      |

If your reaction supports more than one pattern, expose the choice as
a typed configuration enum so operators can pick. The pattern SHOULD
default to whichever one matches the natural cardinality of the
transport.

### Implementation guidance

A canonical Pattern A item builder:

```rust
fn build_item(
    qr: &QueryResult,
    diff: &ResultDiff,
) -> serde_json::Value {
    let mut item = serde_json::Map::new();
    item.insert("queryId".into(),    serde_json::json!(qr.query_id));
    item.insert("sequenceId".into(), serde_json::json!(qr.sequence));
    item.insert("timestamp".into(),  serde_json::json!(qr.timestamp));
    match diff {
        ResultDiff::Add { data, .. } => {
            item.insert("operation".into(), serde_json::json!("ADD"));
            item.insert("after".into(), data.clone());
        }
        ResultDiff::Update { before, after, .. } => {
            item.insert("operation".into(), serde_json::json!("UPDATE"));
            item.insert("before".into(), before.clone());
            item.insert("after".into(), after.clone());
        }
        ResultDiff::Delete { data, .. } => {
            item.insert("operation".into(), serde_json::json!("DELETE"));
            item.insert("before".into(), data.clone());
        }
        // Aggregation / Noop are reaction-specific; the canonical
        // approach is to skip Noop entirely and emit Aggregation as
        // an UPDATE-shaped item using its before/after pair.
        _ => {}
    }
    if !qr.metadata.is_empty() {
        item.insert("metadata".into(),
            serde_json::Value::Object(qr.metadata.clone().into_iter().collect()));
    }
    serde_json::Value::Object(item)
}
```

Wrapping for Pattern B and Pattern C is a straightforward composition:
Pattern B nests an array of items under `results` alongside the
shared `queryId`/`sequenceId`/`timestamp`; Pattern C nests an array
of complete envelopes under `batch`.

> 🔵 **MAY** — If your transport genuinely requires a different
> default shape (for example you wrap in a CloudEvents envelope, or
> write a custom binary format), document the alternative as your
> reaction's normative behavior and SHOULD also provide a
> configuration switch back to the canonical envelope so operators
> who care about consistency across reactions retain that option.

---

## 11. Output Templating (SHOULD)

The previous section described the canonical default output shape.
Templating is the SHOULD-recommended way for an operator to override
that default when the downstream system the reaction is dispatching
to needs a different shape.

### Why templating exists

The reaction trait is written once; the situations a reaction is
deployed into are many. A single reaction binary will be configured
against:

- **Different queries**, with different schemas and different field
  names — `{ user_id, total }` for one query, `{ device, temperature,
  status }` for another.
- **Different environments** — dev, staging, prod; on-prem vs cloud;
  per-tenant deployments. The "correct" payload shape and even the
  "correct" destination can differ per environment.
- **Different downstream systems** with different schema requirements
  — one operator points the reaction at a system that expects a flat
  `{ id, value }`, another points it at a system that expects a
  nested `{ event: { entity: { id }, value } }` envelope, a third
  needs a custom header.

A reaction without templating must hard-code one shape, so each new
deployment situation forces either a new variant of the plugin or an
upstream transformation step. **Templating exists so that a single
reaction plugin remains useful across all of these situations through
configuration alone.**

> 🟡 **SHOULD** — If your reaction would otherwise have to hard-code
> the shape or destination of what it sends downstream — such that
> different deployments, queries, or downstream schemas would force a
> code change or a separate reaction kind — your reaction SHOULD
> support output templating.

> 🔴 **MUST** — If your reaction supports templating, it MUST follow
> the conventions in this section. Inconsistent templating across
> reactions is a major source of operator confusion.

### What "templating" means here

A user configures one or more Handlebars templates per reaction. At
dispatch time the reaction renders each configured string against a
standard context built from the inbound `QueryResult` and the diff.

The reaction-kind decides **which strings are templatable**:

- The **body template** (the payload shape) is templatable on every
  reaction that emits a user-facing payload.
- Any **reaction-kind-specific strings** that vary per deployment (for
  example a URL on a request-style transport, a routing key on a
  message broker, a path or topic on a topic-based sink) are
  templatable too. These live in the reaction's *extension* type —
  see below — so they can vary by query and by operation just like
  the body.

The goal throughout is reusability: the same plugin code, with the
same binary, configured by the user to fit each situation it is
deployed into.

### The required pieces

Templating support is built from four shared types in
`drasi_lib::reactions::common`:

```rust
use drasi_lib::reactions::common::{
    OperationType,    // Add | Update | Delete
    TemplateSpec,     // a single template + optional reaction-specific extension
    QueryConfig,      // templates for the three operations
    TemplateRouting,  // trait: routes(&self) -> &HashMap, default_template(&self) -> Option<&_>
};
```

#### `TemplateSpec<Ext>`

```rust
pub struct TemplateSpec<T = ()>
where T: Default
{
    pub template: String,  // Handlebars body template; empty = use default format
    #[serde(flatten, default)]
    pub extension: T,      // your reaction's extra per-template fields
}
```

The `template` field carries the **body template**. The `extension`
type carries any **reaction-kind-specific configuration** the user
needs to be able to vary alongside the body — fields whose value
depends on the deployment situation or on the data being dispatched.
Any string field in the extension MAY itself be a Handlebars template
that the reaction renders against the standard context at dispatch
time.

Typical examples of fields that live in the extension because they
need to vary per deployment / per query / per operation:

- A request-style transport: `{ url, method, headers }`. Different
  deployments hit different endpoints; some send custom headers per
  query.
- A message broker: `{ routing_key, headers }`. Different deployments
  use different routing topologies; the routing key may need to
  include a row field.
- A topic-based sink: `{ path }` (or `{ topic }`). Different
  environments write to different paths or topic prefixes.

These fields are about **reusability**, not about a single reaction
instance fanning out: the same reaction binary works for every
deployment because every situation-dependent string is configurable,
and configurable strings can be templated when they need to depend on
row data.

If your reaction has no extra per-template fields and only varies the
body, use the default `TemplateSpec` (i.e. `TemplateSpec<()>`).

> 🟡 **SHOULD** — Treat every templated string in the extension
> exactly like the body template: render it through Handlebars with
> the standard context, fail validation at construction time if it
> doesn't compile, and on runtime render failure fall back to a
> documented default (see "Render-failure behavior" below) rather than
> dropping the event.

#### `QueryConfig<Ext>`

Three optional templates, one per operation type:

```rust
pub struct QueryConfig<T = ()> {
    pub added:   Option<TemplateSpec<T>>,
    pub updated: Option<TemplateSpec<T>>,
    pub deleted: Option<TemplateSpec<T>>,
}
```

If an operation slot is `None`, the reaction MUST fall back to the
default template if any, then the default output structure described
in [§10](#10-default-output-structure-should).

Splitting by operation gives the user control over both shape and
extension fields per ADD / UPDATE / DELETE — important because the
downstream contract is often different for each (a deletion may want
just an id where an add wants the full record, an audit endpoint may
want a different URL than a primary write endpoint, etc.).

#### Reaction configuration

Your reaction config SHOULD look like this:

```rust
pub struct MyReactionConfig {
    /// Per-query template overrides.
    pub routes: HashMap<String, QueryConfig<MyExt>>,
    /// Template applied to any query not in `routes`.
    pub default_template: Option<QueryConfig<MyExt>>,
    // ... your other config ...
}

impl TemplateRouting<MyExt> for MyReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig<MyExt>> { &self.routes }
    fn default_template(&self) -> Option<&QueryConfig<MyExt>> { self.default_template.as_ref() }
}
```

The combination of **per-query routes** + **per-operation templates**
+ **per-template extension fields** is what lets a single reaction
binary serve any of the deployment situations described at the top of
this section — each operator points the same plugin at their own
queries, environment, and downstream system shape using only
configuration.

#### Template resolution order

For every inbound result, your reaction MUST resolve the template in
this order:

1. `routes[full_query_id]` for the matching operation.
2. `routes[last_dotted_segment_of_query_id]` for the matching
   operation. (This lets users write `routes.my_query` even when the
   wire id is `source.my_query`.)
3. `default_template` for the matching operation.
4. The default output structure (see [§10](#10-default-output-structure-should)).

`TemplateRouting::get_template_spec(query_id, operation)` already
implements steps 1 and 3 for you. Most reactions add a small wrapper
for step 2 (a one-line `rsplit('.').next()` lookup).

### The required template context

Whenever you render a template — body **or** any templated extension
string — you MUST build the context with these fields. Keys are stable;
do not rename them.

| Key           | Type            | Operations | Description                                        |
|---------------|-----------------|------------|----------------------------------------------------|
| `query_name`  | string          | all        | The query id as received (`query_result.query_id`). |
| `query_id`    | string          | all        | Alias of `query_name` for symmetry with other apis. |
| `operation`   | string          | all        | `"ADD"`, `"UPDATE"`, or `"DELETE"`.                |
| `timestamp`   | string (RFC3339)| all        | Result timestamp.                                  |
| `metadata`    | object          | all        | The result's `metadata` map.                       |
| `after`       | object          | ADD, UPDATE| The post-change row.                               |
| `before`      | object          | UPDATE, DELETE | The pre-change row (or the row being deleted). |
| `data`        | object          | UPDATE     | The raw `data` payload of an Update diff.          |

> 🟡 **SHOULD** — Provide both `query_id` and `query_name` (they hold
> the same value). Users have come to expect both keys; the small
> duplication is worth the consistency.

### The Handlebars engine

> 🔴 **MUST** — Use the [`handlebars`](https://crates.io/crates/handlebars)
> crate from crates.io (5.x is the current minor at the time of
> writing). Do not use a different template engine.

> 🟡 **SHOULD** — Register a `{{json arg}}` helper on every Handlebars
> instance you build, so users can embed arbitrary JSON values into
> string templates without crafting them by hand. The canonical
> implementation:
>
> ```rust
> handlebars.register_helper(
>     "json",
>     Box::new(|h: &handlebars::Helper,
>               _: &handlebars::Handlebars,
>               _: &handlebars::Context,
>               _: &mut handlebars::RenderContext,
>               out: &mut dyn handlebars::Output|
>               -> handlebars::HelperResult {
>         if let Some(v) = h.param(0) {
>             let s = serde_json::to_string(&v.value()).unwrap_or_else(|_| "null".into());
>             out.write(&s)?;
>         }
>         Ok(())
>     }),
> );
> ```

### Validation

> 🔴 **MUST** — Compile every template at **construction time**, not
> at dispatch time. This applies to **body templates and every
> templated string in the extension**. Reject invalid templates from
> your builder with a clear `anyhow::Error`. The standard recipe:
>
> ```rust
> fn validate_template(t: &str) -> anyhow::Result<()> {
>     if t.is_empty() { return Ok(()); }
>     handlebars::Template::compile(t)
>         .map_err(|e| anyhow::anyhow!("invalid template: {e}"))?;
>     Ok(())
> }
> ```

> 🔴 **MUST** — Validate that every key in `routes` corresponds to a
> subscribed query id (or to the last dotted segment of one). Reject
> unknown route keys with a clear error rather than silently ignoring
> them at runtime.

### Render-failure behavior

> 🟡 **SHOULD** — Handle runtime render failures per template kind:
>
> - **Body template** — log the error and fall back to the built-in
>   default output structure for the event, rather than dropping it.
> - **Extension template** (URL, routing key, path, header, …) — log
>   the error and fall back to the reaction's documented default for
>   that field if one exists; otherwise drop the event for that target
>   and continue. Do not silently substitute an unrelated value.

### Putting it all together

The structure every templated reaction produces in its config has the
same shape, regardless of what the reaction does downstream. The
examples below assume a hypothetical reaction whose `Ext` carries one
extra string field, `severity`, that the user wants tagged onto every
emitted message. Real reactions carry whatever extra fields the
transport requires (URLs, routing keys, paths, headers, …); the
*structure* is the same.

In the common case the user configures the body template to match a
single downstream system's schema:

```yaml
my-reaction:
  default_template:
    added:
      template: '{"id":"{{after.id}}","status":"created"}'
      severity: info
    updated:
      template: '{"id":"{{after.id}}","status":"updated"}'
      severity: info
    deleted:
      template: '{"id":"{{before.id}}","status":"deleted"}'
      severity: warn
```

The same plugin, deployed against a different query whose downstream
system expects a different schema, needs no code change — only a
different configuration:

```yaml
my-reaction:
  default_template:
    added:
      template: '{"event":"insert","payload":{{json after}}}'
      severity: info
    updated:
      template: '{"event":"change","payload":{{json after}}}'
      severity: info
    deleted:
      template: '{"event":"remove","payload":{{json before}}}'
      severity: warn
```

When two queries on the same instance need different shapes (the less
common but supported case), the user provides per-query overrides in
`routes`. Routes inherit nothing from `default_template`; supply a
full `QueryConfig` for each routed query.

```yaml
my-reaction:
  default_template:
    added:
      template: '{"id":"{{after.id}}"}'
      severity: info
  routes:
    important-query:
      added:
        template: '{"alert":"{{after.id}}","severity":"high"}'
        severity: alert
```

---

## 12. Optional Capabilities

### 12.1 Deprovisioning

Implemented by overriding `Reaction::deprovision`.

Override `deprovision()` if removing your reaction from the system
requires cleaning up external state. Typical examples: deleting an
output topic that was created by the reaction; removing a webhook
registration; deleting a downstream materialized view.

```rust
async fn deprovision(&self) -> Result<()> {
    self.base.deprovision_common().await?;  // clears state-store partition
    // ... your external cleanup ...
    Ok(())
}
```

Errors are logged but do not block removal.

### 12.2 Durable State & Checkpoints

Activated by returning `true` from `Reaction::is_durable`.

If your reaction needs to remember where it left off across restarts
(e.g. for exactly-once delivery, or to skip already-processed events
after a crash), use the state-store integration.

1. Return `true` from `is_durable()`. The `drasi-lib` instance will refuse to start
   your reaction unless a **persistent** `StateStoreProvider` is
   configured.

2. After processing a batch successfully, call
   `self.base.write_checkpoint(query_id, &ReactionCheckpoint {
   sequence, config_hash }).await?`. The `sequence` is the largest
   `QueryResult::sequence` you have processed for that query.

3. At start, read the existing checkpoints via
   `self.base.read_all_checkpoints().await?` and skip any inbound
   result whose `sequence <= checkpoint.sequence`.

4. The `config_hash` field is used by the `drasi-lib` instance to detect breaking
   configuration changes and force a fresh start. Reactions that don't
   model this SHOULD set it to `0` and preserve whatever value was on
   the previous checkpoint.

`ReactionBase::run_standard_loop(shutdown_rx, initial_checkpoints,
handler)` implements this entire pattern in one call. Use it unless
you need custom batching or multi-query coordination.

### 12.3 Recovery Policy

Selected by overriding `Reaction::default_recovery_policy` or by
setting `ReactionBaseParams::with_recovery_policy(...)` for a specific
reaction instance.

When a reaction restarts and its checkpoint is older than the oldest
entry in the query's outbox (the gap window has been pruned), the `drasi-lib` instance
consults `default_recovery_policy()`:

| Policy        | Behavior                                                                        |
|---------------|---------------------------------------------------------------------------------|
| `Strict`      | Fail start with an error. Manual intervention required. Favors correctness.     |
| `AutoSkipGap` | Resume from the latest available outbox entry. May lose events. Favors uptime.  |
| `AutoReset`   | Wipe checkpoint and re-bootstrap from full snapshot. Favors uptime; expensive.  |

`Strict` is the default. Override on the trait or per-instance via
`ReactionBaseParams::with_recovery_policy()`.

### 12.4 Bootstrap on Fresh Start

Activated by returning `true` from
`Reaction::needs_snapshot_on_fresh_start` and implemented in
`Reaction::bootstrap`.

If your reaction must see the full current result set of every
subscribed query before processing incremental updates (e.g. you
materialize a derived view that requires a base snapshot to apply
diffs against), return `true` from `needs_snapshot_on_fresh_start()`.

The `drasi-lib` instance will call `bootstrap(ctx)` on first start (and after any
reset), where `ctx` exposes:

- `ctx.query_id` — the query being bootstrapped.
- `ctx.is_reset` — `true` if we're discarding a prior checkpoint.
- `ctx.fetch_snapshot()` — returns a `SnapshotStream` yielding rows.
- `ctx.fetch_outbox(after_sequence)` — returns an `OutboxStream`.
- `ctx.read_checkpoint()` / `ctx.write_checkpoint(cp)` — checkpoint
  helpers.

Typical pattern: stream the snapshot into your downstream system, then
`write_checkpoint(ReactionCheckpoint { sequence: 0, config_hash })`.
The `drasi-lib` instance then begins delivering live events.

### 12.5 Adaptive Batching

Configured by adding an `AdaptiveBatchConfig` field to your reaction's
runtime config and using it in your processing loop.

Some reactions dispatch to a remote system whose round-trip cost is
**per-call** rather than per-byte — HTTP webhooks with overhead per
request, message brokers that confirm each publish, bulk-insert APIs
with a fixed per-request cost. For these reactions, dispatching one
event at a time wastes most of the available throughput.

The shared type `drasi_lib::reactions::common::AdaptiveBatchConfig`
exists so that every reaction that wants to batch can be configured
the same way. It carries four fields:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `adaptive_min_batch_size` | `usize` | `1` | Lower bound on items per batch during idle / low-traffic windows. |
| `adaptive_max_batch_size` | `usize` | `100` | Upper bound on items per batch; flushed as soon as this is reached. |
| `adaptive_window_size` | `usize` | `10` | Throughput-observation window in 100 ms units (10 = 1 s, 50 = 5 s). Larger windows are more stable; smaller windows react faster. |
| `adaptive_batch_timeout_ms` | `u64` | `1000` | Maximum wait between the first item entering a batch and the batch being flushed, even if it has not reached `adaptive_max_batch_size`. |

> 🟡 **SHOULD** — Expose these fields in your reaction's runtime
> config (and DTO) so operators can tune them per deployment.
> Reactions whose transport does not benefit from batching SHOULD
> omit the config entirely.

> 🟡 **SHOULD** — Reach for adaptive batching only after measuring
> that single-event dispatch is the bottleneck. Most reactions are
> dispatch-bound, not CPU-bound, but not every dispatch-bound
> reaction benefits from batching — confirm the assumption with a
> measurement first.

#### How an adaptive batch loop fits into a reaction

Adaptive batching **wraps** the inner dequeue loop introduced in §3;
it does not replace it. Instead of pulling one `Arc<QueryResult>` out
of the priority queue and processing it immediately, you pull items
into a `Vec` until either:

- the batch reaches `adaptive_max_batch_size`, **or**
- `adaptive_batch_timeout_ms` has elapsed since the first item entered
  the batch (whichever happens first).

At that point you flush the batch to the downstream system, clear the
buffer, and start over. Through all of this you keep the standard
`tokio::select! { biased; ... }` shutdown receiver so the `drasi-lib` instance can
stop the reaction promptly.

A complete, buildable implementation:

```rust
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{debug, error};
use tokio::sync::oneshot;

use drasi_lib::channels::QueryResult;
use drasi_lib::reactions::common::AdaptiveBatchConfig;
use drasi_lib::reactions::common::base::ReactionBase;

/// Run a batched processing loop over the reaction's priority queue.
///
/// Calls `flush` whenever the in-progress batch reaches
/// `cfg.adaptive_max_batch_size` items or `cfg.adaptive_batch_timeout_ms`
/// elapses since the first item entered the batch. Returns when
/// `shutdown_rx` fires; flushes any partial batch on the way out.
pub async fn run_batched_loop<F, Fut>(
    base: &ReactionBase,
    cfg: AdaptiveBatchConfig,
    mut shutdown_rx: oneshot::Receiver<()>,
    flush: F,
) -> Result<()>
where
    F: Fn(Vec<Arc<QueryResult>>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let queue = &base.priority_queue;
    let timeout = Duration::from_millis(cfg.adaptive_batch_timeout_ms);
    let max = cfg.adaptive_max_batch_size.max(1);

    let mut batch: Vec<Arc<QueryResult>> = Vec::with_capacity(max);
    let mut batch_started_at: Option<Instant> = None;

    loop {
        // Empty batch: block on either shutdown or the next item.
        if batch.is_empty() {
            let next = tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                r = queue.dequeue() => r,
            };
            batch.push(next);
            batch_started_at = Some(Instant::now());
            continue;
        }

        // Partial batch: race shutdown, the next item, and the
        // remaining timeout window.
        let remaining = batch_started_at
            .map(|t| timeout.saturating_sub(t.elapsed()))
            .unwrap_or(timeout);

        let outcome = tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            r = queue.dequeue() => Some(r),
            _ = tokio::time::sleep(remaining) => None,
        };

        match outcome {
            Some(item) => {
                batch.push(item);
                if batch.len() >= max {
                    let drained = std::mem::take(&mut batch);
                    batch_started_at = None;
                    if let Err(e) = flush(drained).await {
                        error!("batched flush failed: {e:#}");
                    }
                }
            }
            None => {
                // Timeout fired with a non-empty batch — flush.
                let drained = std::mem::take(&mut batch);
                batch_started_at = None;
                if let Err(e) = flush(drained).await {
                    error!("batched flush failed: {e:#}");
                }
            }
        }
    }

    // Final drain after shutdown so we do not lose buffered events.
    if !batch.is_empty() {
        debug!("flushing {} pending events on shutdown", batch.len());
        if let Err(e) = flush(batch).await {
            error!("batched final flush failed: {e:#}");
        }
    }

    Ok(())
}
```

The reaction wires it into `start()` exactly the way the quickstart's
single-event loop is wired in §3, but spawning the batched loop in
place of the per-item match:

```rust,ignore
async fn start(&self) -> Result<()> {
    self.base
        .set_status(ComponentStatus::Starting, Some("starting".into()))
        .await;

    let cfg = self.config.batching.clone();              // your reaction's AdaptiveBatchConfig field
    let endpoint = self.config.endpoint.clone();
    let base = self.base.clone_shared();
    let shutdown_rx = self.base.create_shutdown_channel().await;

    let task = tokio::spawn(async move {
        let _ = run_batched_loop(&base, cfg, shutdown_rx, |events| {
            let endpoint = endpoint.clone();
            async move {
                // Send the entire batch in one call. Implementation
                // depends on the transport — one HTTP POST with a JSON
                // array body, one broker publish with multiple
                // messages, one bulk-insert call, etc.
                dispatch_batch(&endpoint, events).await
            }
        })
        .await;
    });

    self.base.set_processing_task(task).await;
    self.base
        .set_status(ComponentStatus::Running, Some("started".into()))
        .await;
    Ok(())
}
```

#### A note on `adaptive_min_batch_size` and `adaptive_window_size`

The example above does **bounded-size + timeout** batching, which is
the simplest pattern and covers the great majority of needs. The two
remaining fields — `adaptive_min_batch_size` and `adaptive_window_size`
— enable a richer pattern in which the batch size and the flush
timeout adapt to observed throughput: under sustained high traffic the
batcher grows the working batch toward `adaptive_max_batch_size`;
under idle conditions it shrinks toward `adaptive_min_batch_size`.
The throughput observation window is sized in 100 ms units by
`adaptive_window_size`.

The simple worked snippet above uses only `adaptive_max_batch_size`
and `adaptive_batch_timeout_ms`. A throughput-adaptive variant of the
same loop adjusts the working `max` per window based on
`batch_count / window_seconds`. The interface to your reaction is the
same in both cases — operators see the same four-field config — so
you can ship the simple version first and replace the loop body later
without breaking your config schema.

#### Backpressure semantics

The priority queue still bounds how much in-flight work a reaction can
hold; adaptive batching only controls how items are *grouped* on the
way out. If your flush function blocks (because the downstream is
slow, throttled, or unavailable) the priority queue continues to
accept inbound events up to its capacity (default `10000`), and then
the `drasi-lib` instance begins blocking on the inbound side. Adaptive batching does
not eliminate the need to size `priority_queue_capacity` for your
expected burst tolerance — it only spends time more efficiently on
the dispatch side.

---

## 13. Testing

### Unit tests

> 🔴 **MUST** — Place unit tests in `#[cfg(test)] mod tests { ... }`
> alongside the code they test. Use `#[tokio::test]` for async tests.

> 🟡 **SHOULD** — Test at minimum:
>
> 1. Builder + config validation (good config builds; bad config fails
>    with the right error).
> 2. The full `Reaction` trait surface (`id`, `type_name`,
>    `properties`, `query_ids`, `auto_start`, `status`).
> 3. Template compilation (every template you support compiles).
> 4. Template rendering (each operation type produces the expected
>    output for a representative input).
> 5. The full start → enqueue → process → stop cycle.

A minimal example covering items 1 and 5:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tokio::sync::mpsc;

    use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
    use drasi_lib::component_graph::ComponentUpdate;
    use drasi_lib::context::ReactionRuntimeContext;

    #[tokio::test]
    async fn builder_returns_valid_reaction() {
        let r = MyReaction::builder("test")
            .from_query("q1")
            .with_endpoint("https://example.test/webhook")
            .build()
            .expect("should build");
        assert_eq!(r.id(), "test");
        assert_eq!(r.query_ids(), vec!["q1"]);
        assert_eq!(r.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn full_lifecycle() {
        // Build the reaction.
        let r = MyReaction::builder("test")
            .from_query("q1")
            .with_endpoint("https://example.test/webhook")
            .build()
            .unwrap();

        // Wire up a runtime context. The mpsc channel carries
        // ComponentUpdate values; we don't observe them in this test.
        let (update_tx, _rx) = mpsc::channel::<ComponentUpdate>(16);
        let ctx = ReactionRuntimeContext::new("inst", "test", None, update_tx, None);
        r.initialize(ctx).await;

        // Start, push an event, then stop.
        r.start().await.unwrap();
        assert_eq!(r.status().await, ComponentStatus::Running);

        let qr = QueryResult::new(
            "q1".to_string(),
            1,
            Utc::now(),
            vec![ResultDiff::Add { data: serde_json::json!({"id":"x"}), row_signature: 0 }],
            Default::default(),
        );
        r.enqueue_query_result(qr).await.unwrap();

        // Give the processing task a moment to drain.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        r.stop().await.unwrap();
        assert_eq!(r.status().await, ComponentStatus::Stopped);
    }
}
```

### Integration tests

> 🟡 **SHOULD** — Add an integration test that exercises your reaction
> end-to-end through `DrasiLib::builder().with_reaction(...).build()`
> for at least one happy path. Use `cargo test` defaults. When a test
> needs an external service, provision it with the Rust `testcontainers`
> crate instead of manually starting Docker containers, shell scripts, or
> `docker compose` harnesses. This keeps service lifecycle, readiness,
> ports, logs, and cleanup owned by the test.

> 🟡 **SHOULD** — Avoid `.unwrap()` outside of test code. Use
> `anyhow::Result` propagation in production code paths; reserve
> `unwrap()` for tests where panics are an acceptable failure mode.

---

## 14. Conformance Checklist

Walk this list before publishing your reaction.

#### Cargo

- [ ] `Cargo.toml` depends on `drasi-lib` (and, if packaging as a
      dynamic plugin, `drasi-plugin-sdk` and `utoipa`) from crates.io
      with a single pinned version.
- [ ] `crate-type = ["lib", "cdylib"]` if you intend to ship as a
      dynamic plugin.
- [ ] If shipping as a dynamic plugin, the dynamic plugin loader you
      target was built against the **same** `drasi-plugin-sdk` version.

#### Reaction trait

- [ ] `id`, `type_name`, `properties`, `query_ids` are implemented.
- [ ] `initialize` delegates to `ReactionBase::initialize`.
- [ ] `start` returns promptly and stores its task handle via
      `set_processing_task`.
- [ ] `stop` delegates to `ReactionBase::stop_common`.
- [ ] `status` delegates to `ReactionBase::get_status`.
- [ ] `enqueue_query_result` accepts events for processing. Most
      reactions do this by delegating to
      `ReactionBase::enqueue_query_result`; see §5.1 for why the
      trait default is not acceptable.

#### Processing loop

- [ ] Uses `tokio::select!` with `biased;` and the shutdown receiver.
- [ ] Handles every `ResultDiff` variant.
- [ ] Treats `ResultDiff::Noop` and empty `results` as no-ops.

#### Configuration

- [ ] Typed `Config` struct deriving `Serialize`, `Deserialize`,
      `Default`.
- [ ] Builder with `with_queries`, `with_query`/`from_query`,
      `with_priority_queue_capacity`, `with_auto_start`, and any
      reaction-specific setters.
- [ ] Builder's `build()` validates config and returns
      `anyhow::Result<MyReaction>`.

#### Templating (if applicable)

- [ ] Decided whether templating is warranted: would the same plugin
      otherwise have to be forked for different queries, environments,
      or downstream schemas?
- [ ] Uses the `handlebars` crate from crates.io.
- [ ] Uses `TemplateSpec`, `QueryConfig`, `TemplateRouting` from
      `drasi_lib::reactions::common`.
- [ ] Reaction-specific fields that must vary per deployment live in a
      flattened `extension` struct; any of those fields that depend on
      row data are rendered through Handlebars at dispatch.
- [ ] Body templates **and** every templated string in the extension
      are compiled at construction.
- [ ] Route keys are validated against the subscribed query list.
- [ ] Resolution order: per-query → last dotted segment → default →
      built-in default envelope.
- [ ] Standard context keys are populated for every render:
      `query_id`, `query_name`, `operation`, `timestamp`, `metadata`,
      `before`, `after`, `data` (as applicable).
- [ ] `{{json arg}}` helper is registered.
- [ ] Render failure handling is documented: body → default envelope;
      extension → documented per-field default or drop, never silently
      substitute an unrelated value.

#### Default output (if applicable)

- [ ] Picked a dispatch pattern (A: per-diff item, B: per-QueryResult,
      C: batched envelopes) appropriate to the transport.
- [ ] Emitted fields use camelCase and include `queryId`, `sequenceId`,
      `timestamp`, `operation`, and `before`/`after` per the operation
      type (`ADD` → `after`; `UPDATE` → `before` + `after`;
      `DELETE` → `before`).
- [ ] Empty `metadata` is omitted from the output.
- [ ] Or: documents an alternative default and provides a configuration
      switch back to the canonical envelope.

#### Dynamic plugin packaging (if applicable)

- [ ] DTO struct derives `utoipa::ToSchema` and is `camelCase`.
- [ ] Schema name follows `reaction.<kind>.<TypeName>`.
- [ ] `ConfigValue<T>` is used for fields supporting secrets or env vars.
- [ ] Descriptor implements `kind`, `config_version`,
      `config_schema_name`, `config_schema_json`, `create_reaction`.
- [ ] `create_reaction` calls `reaction.base_mut().set_raw_config(...)` after
      construction.
- [ ] `export_plugin!` invoked with the reaction descriptor.
- [ ] `kind()` matches `Reaction::type_name()`.
- [ ] Optional descriptor display methods are implemented if the plugin
      will appear in a UI or catalog.

#### Testing

- [ ] At least one unit test for the builder.
- [ ] At least one unit test for the full lifecycle.
- [ ] `cargo test` passes from your crate's directory.

#### Documentation

- [ ] Crate-level rustdoc on `lib.rs` describes the reaction's purpose
      and configuration.
- [ ] Each public type has rustdoc.

---

## 15. Anti-Patterns

These mistakes have all happened more than once. Avoid them.

### "I'll subscribe to the queries myself"

There is no `QueryProvider` for reactions. The `drasi-lib` instance
owns subscriptions. If you try to obtain a `Query` instance from
somewhere, you are working around the architecture and will hit FFI
safety problems the moment your reaction is loaded dynamically.

### "I'll leave `enqueue_query_result` at the default"

The default trait method is a **no-op**. If you do not implement it,
every result pushed to the reaction is silently dropped. See §5.1 for
the required semantics.

### "I'll do all the work in `start()`"

`start()` is called by the `drasi-lib` instance as part of a sequenced
startup. If you block, you stall the entire runtime startup including
every other reaction and source. Always spawn a tokio task and return
immediately.

### "I'll filter secrets out of `properties()`"

`properties()` is the **persistence hook**, not an external API. The
`drasi-lib` instance writes its return value back to the configuration
file so the reaction can be restored on the next startup. Filtering
secrets silently corrupts the configuration. The application embedding
`drasi-lib` is responsible for protecting the file at rest.

### "I'll skip validation and let the rendering layer find the errors"

Compile every template at construction time. Validate that every route
key matches a subscribed query. Validate cross-field invariants in your
config. Failing at startup is far better than failing for the operator
on the first inbound event.

### "I'll use a different template engine"

Don't. The shared `TemplateSpec` / `QueryConfig` / `TemplateRouting`
types assume Handlebars syntax. Mixing engines across reactions makes
the user-facing experience inconsistent and breaks any tooling that
inspects template content.

### "I'll invent my own default output structure"

If your reaction emits JSON and the user has not configured a template,
emit one of the documented envelope patterns from
[§10](#10-default-output-structure-should). Users wire multiple
reactions together and expect a consistent shape.

### "I'll skip `tokio::select!` and poll the queue with a timeout"

You will time out and be force-aborted by `stop_common`. The shutdown
receiver and `biased;` are not decorations — they are the runtime's only
way to interrupt your loop promptly.

### "I'll re-implement what `ReactionBase` does"

You will get it subtly wrong. The status handle's graph wiring, the
priority queue's backpressure strategy, the shutdown channel
plumbing, the checkpoint helpers — all of these are battle-tested.
Compose with `ReactionBase`.

### "I'll panic if I see invalid input"

Panics in a `cdylib`-loaded reaction are caught at the FFI boundary
and reported as transient errors. This is a safety net; relying on it
makes your reaction unpredictable when used embedded. Return an
`Err(anyhow!(...))` instead.

### "I'll keep the `BootstrapContext` for later use"

The context's backend may hold raw FFI pointers that are invalidated
when `bootstrap()` returns. Storing it and calling
`fetch_snapshot()` later is undefined behavior.

---

## Appendix A — Full TypeSpec Schemas

These schemas use [TypeSpec](https://typespec.io) with the
`@typespec/json-schema` emitter. They are reproduced here in full so
the schema contract is self-contained in this document.

### Common types

```typespec
import "@typespec/json-schema";

using TypeSpec.JsonSchema;

namespace Drasi.Reactions;

/**
 * Operation type for a single diff in a QueryResult.
 */
@jsonSchema
enum OperationType {
  add: "ADD",
  update: "UPDATE",
  delete: "DELETE",
  aggregation: "aggregation",
  noop: "noop",
}

/**
 * A single diff in a QueryResult batch.
 *
 * Variants carry different fields. Wire form is a tagged union with the
 * "type" discriminator; the discriminator values match the Rust enum's
 * serde rename attributes exactly.
 */
@jsonSchema
union ResultDiff {
  add:         AddDiff,
  update:      UpdateDiff,
  delete:      DeleteDiff,
  aggregation: AggregationDiff,
  noop:        NoopDiff,
}

@jsonSchema
model AddDiff {
  type: "ADD";
  data: unknown;
  rowSignature?: uint64;
}

@jsonSchema
model UpdateDiff {
  type: "UPDATE";
  data: unknown;
  before: unknown;
  after: unknown;
  groupingKeys?: string[];
  rowSignature?: uint64;
}

@jsonSchema
model DeleteDiff {
  type: "DELETE";
  data: unknown;
  rowSignature?: uint64;
}

@jsonSchema
model AggregationDiff {
  type: "aggregation";
  before?: unknown;
  after: unknown;
  rowSignature?: uint64;
}

@jsonSchema
model NoopDiff {
  type: "noop";
}
```

### QueryResult (what reactions consume)

```typespec
/**
 * The full QueryResult value forwarded to reactions by the `drasi-lib` instance.
 * Reactions do not produce this; they consume it via
 * Reaction::enqueue_query_result.
 */
@jsonSchema
model QueryResult {
  /** Id of the query that produced this result. */
  queryId: string;
  /** Monotonic per-query sequence number. */
  sequence: uint64;
  /** Event timestamp (RFC3339). */
  timestamp: utcDateTime;
  /** The batch of diffs. May be empty. */
  results: ResultDiff[];
  /** Optional source/query metadata. */
  metadata?: Record<unknown>;
}
```

### Default output envelopes (what reactions emit)

```typespec
/**
 * One emitted item — the canonical Pattern A payload, and the same
 * shape used inside Pattern B's `results` array and inside the
 * Pattern C `batch` array.
 *
 * Field presence mirrors QueryResult / ResultDiff semantics:
 *
 *   operation = "ADD"    -> `after` present, `before` absent
 *   operation = "UPDATE" -> `before` AND `after` present
 *   operation = "DELETE" -> `before` present, `after` absent
 */
@jsonSchema
model ReactionItem {
  queryId: string;
  sequenceId: uint64;
  timestamp: utcDateTime;
  operation: "ADD" | "UPDATE" | "DELETE";
  /** Post-change row. Present on ADD and UPDATE. */
  after?: unknown;
  /** Pre-change row. Present on UPDATE and DELETE. */
  before?: unknown;
  /** Optional source/query metadata; omitted when empty. */
  metadata?: Record<unknown>;
}

/**
 * Pattern A — one envelope per ResultDiff.
 */
@jsonSchema
model ReactionItemEnvelope is ReactionItem;

/**
 * Pattern B — one envelope per QueryResult, carrying all diffs from
 * one emission. The `results` array members carry the per-diff
 * (operation, after?, before?) fields without repeating the
 * envelope-level queryId/sequenceId/timestamp.
 */
@jsonSchema
model ReactionEnvelope {
  queryId: string;
  sequenceId: uint64;
  timestamp: utcDateTime;
  results: ReactionResultItem[];
  metadata?: Record<unknown>;
}

/**
 * The per-diff payload inside a Pattern B envelope. Same field
 * presence rules as ReactionItem, minus the envelope-level fields.
 */
@jsonSchema
model ReactionResultItem {
  operation: "ADD" | "UPDATE" | "DELETE";
  after?: unknown;
  before?: unknown;
}

/**
 * Pattern C — a batch container holding many envelope instances.
 * The `batch` members are either all ReactionItemEnvelope or all
 * ReactionEnvelope; pick one shape per reaction and stay with it.
 */
@jsonSchema
model ReactionBatchEnvelope {
  batch: (ReactionItemEnvelope | ReactionEnvelope)[];
}
```

### Template context (what is available inside a template)

```typespec
/**
 * The standard context object passed to a rendered template.
 *
 * Reactions MUST populate the fields below when rendering. Field
 * availability depends on the operation type — see §11 of the
 * developer guide.
 */
@jsonSchema
model TemplateContext {
  /** The originating query id, as received. */
  queryId: string;
  /** Alias of queryId for backward compatibility. */
  queryName: string;
  /** "ADD" | "UPDATE" | "DELETE". */
  operation: string;
  /** Event timestamp (RFC3339). */
  timestamp: string;
  /** Source/query metadata; may be empty. */
  metadata: Record<unknown>;
  /** Row state after change. Present for ADD and UPDATE. */
  after?: unknown;
  /** Row state before change. Present for UPDATE and DELETE. */
  before?: unknown;
  /** Raw data field on UPDATE diffs. Present only for UPDATE. */
  data?: unknown;
}
```

---

## Appendix B — Glossary

| Term                     | Definition                                                                                                                  |
|--------------------------|-----------------------------------------------------------------------------------------------------------------------------|
| **Reaction**             | The output component type. Subscribes to query results and dispatches them to an external system.                           |
| **`Reaction` trait**     | The Rust trait every reaction implements (`drasi_lib::Reaction`).                                                           |
| **`ReactionBase`**       | The shared implementation of every cross-cutting concern (status, queue, shutdown, checkpoints, identity).                  |
| **`ReactionPluginDescriptor`** | The factory trait used by the dynamic plugin loader to construct a reaction from validated JSON.                       |
| **DTO**                  | A `serde + utoipa::ToSchema` struct that defines the user-facing JSON shape of a reaction's configuration.                  |
| **`ConfigValue<T>`**     | A union of static value, environment-variable reference, and secret reference. Resolved at construction time.               |
| **`QueryResult`**        | The unit of consumption: one emission from one query, containing a batch of `ResultDiff` values.                            |
| **`ResultDiff`**         | A single row change: `Add`, `Update`, `Delete`, `Aggregation`, or `Noop`.                                                   |
| **Outbox**               | The per-query log of emissions that reactions can replay from a checkpoint.                                                 |
| **Checkpoint**           | A `(sequence, config_hash)` pair persisted to the state store, marking what a reaction has successfully processed.          |
| **State store**          | Pluggable backend for reaction-owned persistent state (`StateStoreProvider`). May be volatile (in-memory) or durable.       |
| **Identity provider**    | Pluggable backend that supplies credentials at runtime (`IdentityProvider`).                                                |
| **Template (Handlebars)**| A user-supplied string with `{{}}` substitutions rendered against the standard template context.                            |
| **Template extension**   | A reaction-specific struct flattened into `TemplateSpec<T>` that holds per-template fields whose value depends on the deployment situation or the row data.       |
| **Default envelope**     | Canonical JSON shape (`ReactionItemEnvelope`, `ReactionEnvelope`, or a `ReactionBatchEnvelope` of either) emitted when no template applies. See [§10](#10-default-output-structure-should). |
| **Adaptive batcher**     | Optional throughput-aware batching helper (`AdaptiveBatchConfig`) for dispatch-bound reactions.                             |
| **FFI vtable**           | The `#[repr(C)]` function pointer table used to dispatch trait methods across the dynamic plugin boundary.                  |
