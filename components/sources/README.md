# Drasi Source Plugin Developer Guide

## Purpose

This document is a guide for software developers who create and maintain custom
**Source plugins** in Rust for `drasi-lib`.

Sources are the components of Drasi that connect to external systems and make the
changes to data from those system available to Continuous Queries.

---

## Table of Contents

1. [Conventions: MUST, SHOULD, MAY](#1-conventions-must-should-may)
2. [Overview & Mental Model](#2-overview--mental-model)
    1. [Key invariants](#key-invariants)
    2. [A source in context](#a-source-in-context)
3. [Quickstart: A Working Source in 15 Minutes](#3-quickstart-a-working-source-in-15-minutes)
4. [Core Concepts](#4-core-concepts)
    1. [The Component Lifecycle](#41-the-component-lifecycle)
    2. [Runtime-Owned Query Subscriptions](#42-runtime-owned-query-subscriptions)
    3. [Dispatch, Backpressure, and Subscriber Isolation](#43-dispatch-backpressure-and-subscriber-isolation)
    4. [Bootstrap Is Separate from Streaming](#44-bootstrap-is-separate-from-streaming)
    5. [Replay, Checkpoints, and Source Positions](#45-replay-checkpoints-and-source-positions)
    6. [The Runtime Context](#46-the-runtime-context)
5. [The `Source` Trait](#5-the-source-trait)
    1. [Required methods](#51-required-methods)
    2. [Lifecycle — the runtime call sequence](#52-lifecycle--the-runtime-call-sequence)
    3. [Optional capability overrides](#53-optional-capability-overrides)
    4. [Error handling](#54-error-handling)
6. [`SourceBase` — Reference](#6-sourcebase--reference)
7. [The Source Event Data Contract](#7-the-source-event-data-contract)
8. [Resilience & Persistence Patterns](#8-resilience--persistence-patterns)
9. [Bootstrap Providers](#9-bootstrap-providers)
10. [Transforming External Data into Graph Changes](#10-transforming-external-data-into-graph-changes)
11. [Secrets, Environment Variables, and Identity](#11-secrets-environment-variables-and-identity)
12. [Packaging as a Dynamic Plugin](#12-packaging-as-a-dynamic-plugin)
13. [Testing](#13-testing)
14. [Conformance Checklist](#14-conformance-checklist)
15. [Anti-Patterns](#15-anti-patterns)
16. [Appendix A — Full Type Shapes](#appendix-a--full-type-shapes)
17. [Appendix B — Glossary](#appendix-b--glossary)

---

## 1. Conventions: MUST, SHOULD, MAY

This guide uses [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) keywords
in capital letters to distinguish requirement levels.

| Keyword | Meaning |
|---------|---------|
| **MUST**, **REQUIRED** | A hard contract. Violating it can cause compile errors, startup failures, data loss, replay gaps, or silent query corruption. |
| **MUST NOT** | An explicit prohibition. Same consequences as MUST. |
| **SHOULD**, **RECOMMENDED** | A strong convention shared by well-behaved sources. Deviating is allowed, but only with a clear reason. |
| **SHOULD NOT** | Strongly discouraged. Reach for an alternative first. |
| **MAY**, **OPTIONAL** | A capability the runtime supports but does not require for every source. Use it only when your source needs it. |

You will see callouts throughout this guide, for example:

> 🔴 **MUST** — Convert external changes into `SourceChange` values with stable
> source IDs, element IDs, labels, and millisecond `effective_from` values.  
> 🟡 **SHOULD** — Use `SourceBase` for subscriptions, bootstrap, dispatch,
> status, replay handles, and checkpoint-safe sequencing.  
> 🔵 **MAY** — Use a transient WAL for sources that need crash recovery but do
> not have an upstream replay log.  

---

## 2. Overview & Mental Model

A Source is created, configured, and added to a `drasi-lib` instance. Queries
then subscribe to it. When data in the external system changes, the Source
converts that change into Drasi `SourceChange` values and dispatches
them to all subscribed queries.

From the perspective of `drasi-lib`, Sources actively push `SourceChange` values.
But how Sources actually obtain their data is entirely up to the Source developer.

```
 ┌──────────────────┐    ┌────────────┐    ┌────────────┐    ┌────────────┐
 │ External system  │───►│ Source     │───►│ Query      │───►│ Reaction   │
 │ DB, API, stream, │    │ (your code)│    │ engine     │    │ plugins    │
 │ webhook, file... │    │            │    │            │    │            │
 └──────────────────┘    └────────────┘    └────────────┘    └────────────┘
                              │
                              ▼
                       Optional Bootstrap
                       Provider snapshot
```

### Key invariants

1. **Sources are active producers.** A Source owns the connection, listener,
   poller, consumer, or in-memory API that ingests data from an external system.
   It pushes `SourceEventWrapper` values to the runtime through `SourceBase`.

2. **Queries subscribe to Sources.** Sources do not know query text and do not
   evaluate Cypher / GQL. The runtime calls `subscribe(settings)` for each query that
   needs the Source. `settings` contains the query ID plus the node and relation
   labels allocated to that Source.

3. **The emitted shape is fixed.** Sources may transform external payloads into a
   graph model, but after that transformation they always emit the same Drasi
   data contract: `SourceChange::Insert`, `Update`, or `Delete` wrapped in
   `SourceEventWrapper`. Sources do not use reaction-style output templates.

4. **Resilience is part of the Source contract.** A Source must be explicit about
   whether it can replay from a persisted position. Native-log sources use
   upstream positions such as WAL LSNs or stream offsets. Transient ingress
   sources may use Drasi's WAL durability layer.

5. **Bootstrap and streaming are separate.** Bootstrap providers produce initial
   data for a query. The streaming Source produces live changes. The two can be
   implemented by the same crate, but the runtime treats them as separate
   capabilities.

6. **Ownership transfers to `DrasiLib`.** Once a Source is passed to
   `DrasiLib::builder().with_source(...)`, the runtime owns its lifecycle and
   calls `initialize`, `start`, `subscribe`, `stop`, and `deprovision` as needed.

7. **FFI safety matters.** Dynamic source plugins are loaded through a stable C
   ABI vtable. Panics at the boundary are caught by the plugin SDK, but source
   implementations SHOULD return errors rather than panic.

### A source in context

The Quickstart below builds `MySource`. In a real application, the source is one
piece of a `drasi-lib` instance alongside at least one query and, usually, at
least one reaction. The application creates those pieces, transfers ownership to
`DrasiLib::builder()`, then starts the runtime.

The example below uses the source you build in this guide so the moving parts are
visible without an external database, stream, or service.

```rust,ignore
use drasi_lib::{DrasiLib, Query};
use my_source::{MySource, MySourceConfig};

async fn run() -> anyhow::Result<()> {
    let source = MySource::builder("my-source")
        .with_config(MySourceConfig::default())
        .build()?;

    let query = Query::cypher("counter-query")
        .query("MATCH (c:Counter) RETURN c.value AS value")
        .from_source("my-source")
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await?;

    drasi.start().await?;
    Ok(())
}
```

This guide focuses on the source plugin itself. The query above is context: it
shows where the source fits once you have built it.

---

## 3. Quickstart: A Working Source in 15 Minutes

This walkthrough creates a small Source that emits a `Counter` node every second.
It is intentionally simple: no external database, no bootstrap provider, and no
replay. Later sections explain how to add those capabilities correctly.

### Prerequisites

> ✅ A recent stable Rust toolchain (`rustc --version` ≥ 1.83).  
> ✅ Familiarity with async Rust and the `tokio` ecosystem.  
> ✅ An internet connection so `cargo` can resolve `drasi-lib`,
>    `drasi-core`, and their transitive dependencies from crates.io.  

You do **not** need a clone of the Drasi source tree. Your source lives in its
own crate and depends on the published `drasi-lib` and `drasi-core` crates.

### Step 1: Create the crate

In a fresh directory of your choice:

```bash
cargo new --lib my-source
cd my-source
```

Lay out the source as follows:

```
my-source/
├── Cargo.toml
└── src/
    ├── lib.rs        # Public exports
    ├── config.rs     # Typed configuration struct
    └── mysource.rs   # The Source implementation
```

### Step 2: `Cargo.toml`

```toml
[package]
name = "my-source"
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[lib]
crate-type = ["lib", "cdylib"]

[dependencies]
# Pin to one drasi-lib/drasi-core pair across your project. The versions
# below were current at the time this guide was written; check crates.io
# for the latest compatible releases.
drasi-lib = "0.8"
drasi-core = "0.5"
anyhow = "1"
async-trait = "0.1"
chrono = "0.4"
log = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time"] }
```

> 🔴 **MUST** — Depend on `drasi-lib` and `drasi-core` from crates.io
> and pin compatible versions. Dynamic plugins must also use the same
> `drasi-plugin-sdk` version as the host that will load them.

> 🟡 **SHOULD** — Declare `crate-type = ["lib", "cdylib"]` even if you
> only need the static `lib` path today. Adding `cdylib` later requires
> a rebuild but no source changes.

### Step 3: `config.rs` — typed configuration

```rust
// src/config.rs
use serde::{Deserialize, Serialize};

fn default_interval_ms() -> u64 {
    1_000
}

fn default_label() -> String {
    "Counter".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySourceConfig {
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,

    #[serde(default = "default_label")]
    pub label: String,
}

impl Default for MySourceConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_interval_ms(),
            label: default_label(),
        }
    }
}
```

### Step 4: `mysource.rs` — the source itself

```rust
// src/mysource.rs
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::{
    ComponentStatus, DispatchMode, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
    SourceSubscriptionSettings, SubscriptionResponse,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::interval;

use crate::config::MySourceConfig;

pub struct MySource {
    // `base` is `pub(crate)` so descriptor.rs (§12) can preserve raw
    // configuration before returning the source to the host.
    pub(crate) base: SourceBase,
    config: MySourceConfig,
}

impl MySource {
    pub fn new(id: impl Into<String>, config: MySourceConfig) -> Result<Self> {
        Self::builder(id).with_config(config).build()
    }

    pub fn builder(id: impl Into<String>) -> MySourceBuilder {
        MySourceBuilder::new(id)
    }

    pub fn base_mut(&mut self) -> &mut SourceBase {
        &mut self.base
    }

    fn make_counter_change(source_id: &str, label: &str, value: i64) -> SourceChange {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::Integer(value));

        let effective_from = Utc::now().timestamp_millis() as u64;
        let labels: Arc<[Arc<str>]> = vec![Arc::<str>::from(label)].into();

        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, "counter"),
                labels,
                effective_from,
            },
            properties,
        };

        if value == 1 {
            SourceChange::Insert { element }
        } else {
            SourceChange::Update { element }
        }
    }
}

pub struct MySourceBuilder {
    id: String,
    config: MySourceConfig,
    dispatch_buffer_capacity: Option<usize>,
    auto_start: bool,
}

impl MySourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: MySourceConfig::default(),
            dispatch_buffer_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_config(mut self, config: MySourceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_interval_ms(mut self, interval_ms: u64) -> Self {
        self.config.interval_ms = interval_ms;
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.config.label = label.into();
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> Result<MySource> {
        if self.config.interval_ms == 0 {
            anyhow::bail!("interval_ms must be greater than zero");
        }
        if self.config.label.trim().is_empty() {
            anyhow::bail!("label must not be empty");
        }

        let mut params = SourceBaseParams::new(self.id)
            .with_dispatch_mode(DispatchMode::Channel)
            .with_auto_start(self.auto_start);

        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }

        Ok(MySource {
            base: SourceBase::new(params)?,
            config: self.config,
        })
    }
}

#[async_trait]
impl Source for MySource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "my-source"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn dispatch_mode(&self) -> DispatchMode {
        self.base.get_dispatch_mode()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn supports_replay(&self) -> bool {
        false
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status(ComponentStatus::Starting, Some("Starting my source".into()))
            .await;

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let base = self.base.clone_shared();
        let source_id = self.base.id.clone();
        let label = self.config.label.clone();
        let interval_ms = self.config.interval_ms;

        let task = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(interval_ms));
            let mut value = 0_i64;

            base.set_status(ComponentStatus::Running, Some("My source running".into()))
                .await;

            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    _ = ticker.tick() => {
                        value += 1;
                        let change = MySource::make_counter_change(&source_id, &label, value);
                        if let Err(err) = base.dispatch_source_change(change).await {
                            base.set_status(
                                ComponentStatus::Error,
                                Some(format!("Failed to dispatch change: {err}")),
                            ).await;
                            break;
                        }
                    }
                }
            }
        });

        self.base.set_task_handle(task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(ComponentStatus::Stopping, Some("Stopping my source".into()))
            .await;
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "my-source")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}
```

Key points:

1. **`supports_replay()` returns `false`.** The quickstart Source is volatile.
   The `Source` trait default is replay-capable, so volatile Sources MUST
   override this.
2. **`effective_from` is milliseconds.** Use `timestamp_millis()`, not
   nanoseconds.
3. **`subscribe()` delegates to `SourceBase`.** This applies checkpoint sequence
   settings, creates the streaming receiver, and handles optional bootstrap.
4. **The source does not template output.** It maps external data into the fixed
   Drasi `SourceChange` graph contract.

### Step 5: `lib.rs` — public exports

```rust
// src/lib.rs
pub mod config;
pub mod mysource;

pub use config::MySourceConfig;
pub use mysource::{MySource, MySourceBuilder};
```

### Step 6: Build it

From your crate's directory:

```bash
cargo build
```

> ✅ **Check your work** — At this point the crate should compile
> cleanly. You have:
>
> - A typed runtime configuration struct (`MySourceConfig`).
> - A `Source` implementation backed by `SourceBase`.
> - A fluent `MySourceBuilder` exposing standard setters
>   (`with_auto_start`, `with_dispatch_buffer_capacity`, `with_config`)
>   plus source-specific conveniences.
> - A background task that emits graph changes and exits cleanly on shutdown.
> - Status transitions wired through `SourceBase`.
> - `supports_replay() == false`, which is correct for this volatile source.

### Step 7: Use it

```rust,ignore
use drasi_lib::{DrasiLib, Query};
use my_source::{MySource, MySourceConfig};

async fn run() -> anyhow::Result<()> {
    let source = MySource::new("my-source", MySourceConfig::default())?;

    let query = Query::cypher("counter-query")
        .query("MATCH (c:Counter) RETURN c.value AS value")
        .from_source("my-source")
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await?;

    drasi.start().await?;
    Ok(())
}
```

You now have a buildable source scaffold. It demonstrates the trait, builder,
lifecycle, subscription, dispatch, and dynamic-plugin shape. Later sections show
the event contract, bootstrap, replay, durability, identity, schema discovery,
dynamic loading, and tests you apply when replacing the counter loop with a real
external system.

---

## 4. Core Concepts

### 4.1 The Component Lifecycle

Sources follow the same lifecycle model as other Drasi components:

```
Added → Starting → Running → Stopping → Stopped
           ↓                         ↓
         Error                    Removed
```

`SourceBase::new()` creates the local status handle. `initialize(context)` wires
that handle to the component graph update channel. After that, every
`SourceBase::set_status(...)` call updates local status and notifies the runtime.

Typical Source lifecycle:

1. **Construct** — validate typed config and create `SourceBase`.
2. **Initialize** — runtime calls `initialize(SourceRuntimeContext)`.
3. **Start** — connect to external systems and spawn ingestion tasks.
4. **Subscribe** — queries call `subscribe(settings)` and receive event channels.
5. **Stream** — source dispatches live `SourceEventWrapper` values.
6. **Stop** — signal tasks, wait briefly, abort if necessary, clear stale
   channel-mode dispatchers.
7. **Deprovision** — optional permanent cleanup when a source is deleted with
   cleanup enabled.

#### Who calls what

| Method | Called by | Implementer responsibility |
|--------|-----------|----------------------------|
| `new(...)` / builder | Application or plugin descriptor | Validate config and create `SourceBase`. |
| `initialize(context)` | `DrasiLib` | Delegate to `self.base.initialize(context).await`. |
| `start()` | `DrasiLib` lifecycle or `start_source()` | Start ingestion and set status. |
| `subscribe(settings)` | Query manager | Return streaming receiver and optional bootstrap receiver. |
| `on_subscriptions_complete()` | Lifecycle after startup subscriptions | Release startup fences if the source uses them. |
| `stop()` | `DrasiLib` lifecycle or `stop_source()` | Stop tasks, release resources, clear stale dispatchers. |
| `deprovision()` | `remove_source(id, cleanup: true)` | Remove persistent state or external resources. |

### 4.2 Runtime-Owned Query Subscriptions

Queries subscribe to Sources through the runtime. A Source receives a
`SourceSubscriptionSettings` value:

```rust,ignore
pub struct SourceSubscriptionSettings {
    pub source_id: String,
    pub enable_bootstrap: bool,
    pub query_id: String,
    pub nodes: HashSet<String>,
    pub relations: HashSet<String>,
    pub resume_from: Option<Bytes>,
    pub request_position_handle: bool,
    pub last_sequence: Option<u64>,
}
```

The settings are important:

- `nodes` and `relations` let a Source or Bootstrap Provider filter to labels
  the query actually needs.
- `resume_from` contains opaque source-position bytes from a prior checkpoint.
  Only the Source knows how to interpret them.
- `last_sequence` lets `SourceBase` keep framework sequence numbers monotonic
  after restart.
- `request_position_handle` asks replay-capable sources for a shared
  `Arc<AtomicU64>` so the query can report its durably processed sequence.

> 🔴 **MUST** — If you do custom subscription logic, call
> `SourceBase::apply_subscription_settings(&settings)` at the start, or delegate
> to `subscribe_with_bootstrap()` / `subscribe_with_replay()`, which do it for
> you. Skipping this can break sequence monotonicity after restart.

### 4.3 Dispatch, Backpressure, and Subscriber Isolation

Sources support two dispatch modes.

| Mode | Behavior | Use when |
|------|----------|----------|
| `DispatchMode::Channel` (default) | Dedicated MPSC channel per subscriber. Backpressure can flow from query to source. Subscriber-specific replay filtering is supported. | Message loss is unacceptable, subscribers process at different speeds, or replay filtering matters. |
| `DispatchMode::Broadcast` | One shared broadcast channel. Lower memory and higher fanout, but lagging receivers can miss messages. Per-subscriber replay filtering is not supported. | Many subscribers need the same stream and occasional message loss is acceptable. |

Configure this through `SourceBaseParams`:

```rust,ignore
let base = SourceBase::new(
    SourceBaseParams::new("orders")
        .with_dispatch_mode(DispatchMode::Channel)
        .with_dispatch_buffer_capacity(1_000),
)?;
```

> 🟡 **SHOULD** — Use `Channel` unless you have a specific high-fanout reason to
> use `Broadcast`. Replay-capable sources usually need `Channel`.

### 4.4 Bootstrap Is Separate from Streaming

Bootstrap is initial data delivery for a query. Streaming is live change
delivery. A Source may support both, but they are separate interfaces.

```
Query subscribe(enable_bootstrap = true)
       │
       ├── SourceBase creates streaming receiver
       ├── SourceBase invokes BootstrapProvider, if configured
       └── Query processes bootstrap events before live stream handoff
```

Important rules:

- Bootstrap providers send `BootstrapEvent` values, not `SourceEventWrapper`
  values.
- `resume_from` overrides bootstrap. A resuming query already has indexed state,
  so re-bootstrapping would corrupt it.
- `BootstrapResult` carries handoff metadata (`last_sequence`,
  `sequences_aligned`, and `source_position`) so the query can persist a safe
  starting checkpoint.
- Any Source can be paired with any Bootstrap Provider, but only aligned pairs
  can safely share a sequence namespace.

### 4.5 Replay, Checkpoints, and Source Positions

Drasi persists two related positions:

| Position | Owner | Meaning |
|----------|-------|---------|
| Framework `sequence: u64` | `SourceBase` | Monotonic event sequence assigned to every dispatched event. Queries use it for ordering, gap detection, and confirmation handles. |
| `source_position: Bytes` | Source plugin | Opaque upstream position, such as a database log address, stream partition/offset set, or transient WAL sequence encoded as bytes. |

Replay-capable sources must:

1. Attach `source_position` to live events when the upstream has a durable
   position.
2. Validate `settings.resume_from` during `subscribe`.
3. Seek, rewind, replay, or restart from that position.
4. Return `SourceError::PositionUnavailable` when the requested position was
   pruned or is older than the earliest available upstream position.
5. Register a `PositionComparator` when per-subscriber filtering is needed.
6. Use position handles to compute the minimum confirmed subscriber position
   before acknowledging or pruning upstream state.

Native-log position examples:

- A single monotonic integer position can be encoded as fixed-width
  big-endian bytes and compared with `ByteLexPositionComparator`.
- A compound stream position, such as partitions plus offsets, usually needs a
  custom `PositionComparator`.
- A database log address should be encoded in the same ordering used by the
  database's replay API.

Transient ingress sources can enable Drasi WAL durability. The source writes
incoming events to a configured `WalProvider` before acknowledging the producer,
then replays retained events on restart.

### 4.6 The Runtime Context

`SourceRuntimeContext` is the runtime dependency-injection mechanism for Sources.
It is provided during `initialize()` and includes:

- `source_id`
- component graph update channel for status events
- optional state store provider
- optional WAL provider
- optional identity provider
- runtime instance metadata used for log routing

Use the context through `SourceBase`:

```rust,ignore
#[async_trait]
impl Source for MySource {
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
}

// Later:
let state_store = self.base.state_store().await;
let identity_provider = self.base.identity_provider().await;
let context = self.base.context().await;
```

---

## 5. The `Source` Trait

All Sources implement `drasi_lib::sources::Source`.

```rust,ignore
#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &str;
    fn type_name(&self) -> &str;
    fn properties(&self) -> HashMap<String, serde_json::Value>;

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    fn auto_start(&self) -> bool {
        true
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        None
    }

    async fn start(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn status(&self) -> ComponentStatus;

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse>;

    fn as_any(&self) -> &dyn std::any::Any;

    async fn deprovision(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn initialize(&self, context: SourceRuntimeContext);

    async fn set_bootstrap_provider(
        &self,
        _provider: Box<dyn BootstrapProvider + 'static>,
    ) {
    }

    async fn remove_position_handle(&self, _query_id: &str) {}

    async fn on_subscriptions_complete(&self) {}

    async fn set_identity_provider(
        &self,
        _provider: Arc<dyn IdentityProvider>,
    ) {
    }
}
```

### 5.1 Required methods

#### `id` — **MUST**

Return the unique source instance ID. This value is used in
`ElementReference.source_id`, component status, checkpoint state, WAL partitions,
and config snapshots.

#### `type_name` — **MUST**

Return the source kind, for example `database-cdc`, `webhook`, `stream`, or
`my-source`. Dynamic plugin descriptors should use the same string from
`SourcePluginDescriptor::kind()`.

#### `properties` — **MUST**

Return the full persisted configuration map for this source.

> 🔴 **MUST** — Include all configuration values, including secrets and
> connection strings. `properties()` is the persistence hook used by runtime
> configuration snapshots. Omitting values makes restart lossy.

Prefer `SourceBase::properties_or_serialize(&dto)` in dynamic plugins so raw
`ConfigValue` envelopes survive config snapshot roundtrips.

#### `start` — **MUST**

Begin ingestion. This usually means:

1. validate runtime dependencies from `SourceRuntimeContext`
2. connect to the external system
3. register WAL or upstream cursors if needed
4. spawn one or more tasks
5. store task and shutdown handles in `SourceBase`
6. set `ComponentStatus::Running`

Do not block forever inside `start()`. Spawn long-running work and return after
startup succeeds.

#### `stop` — **MUST**

Stop ingestion and clean up local resources. Use `SourceBase::stop_common()` if
your source stores a `oneshot::Sender<()>` and task handle in `SourceBase`.
Sources with custom stop logic MUST call `clear_dispatchers()` for channel mode.

#### `status` — **MUST**

Return the current `ComponentStatus`, usually `self.base.get_status().await`.

#### `subscribe(settings)` — **MUST**

Create and return a streaming receiver for a query. Most sources should delegate:

```rust,ignore
async fn subscribe(&self, settings: SourceSubscriptionSettings) -> Result<SubscriptionResponse> {
    self.base.subscribe_with_bootstrap(&settings, "my-source").await
}
```

Replay-capable sources often inspect `settings.resume_from` first, then delegate
to `subscribe_with_bootstrap()` or `subscribe_with_replay()`.

#### `as_any` — **MUST**

Return `self` for downcasting in tests.

#### `initialize(context)` — **MUST**

Delegate to `SourceBase`:

```rust,ignore
async fn initialize(&self, context: SourceRuntimeContext) {
    self.base.initialize(context).await;
}
```

### 5.2 Lifecycle — the runtime call sequence

Fresh start:

```
new / descriptor create_source
    ↓
initialize(context)
    ↓
start()
    ↓
subscribe(query A, enable_bootstrap = true, resume_from = None)
    ↓
subscribe(query B, enable_bootstrap = true, resume_from = None)
    ↓
on_subscriptions_complete()
    ↓
dispatch live SourceEventWrapper values
```

Restart with query checkpoint:

```
initialize(context)
    ↓
start()
    ↓
subscribe(query, resume_from = Some(source_position), last_sequence = Some(seq))
    ↓
source validates/rewinds/replays
    ↓
bootstrap is skipped
    ↓
live stream resumes
```

Removal with cleanup:

```
stop()
    ↓
deprovision()
    ↓
component removed
```

### 5.3 Optional capability overrides

#### `dispatch_mode` — **MAY**

Return the mode used by the Source. If you use `SourceBaseParams` to configure
the mode, delegate:

```rust,ignore
fn dispatch_mode(&self) -> DispatchMode {
    self.base.get_dispatch_mode()
}
```

#### `auto_start` — **MAY**

Return `false` if the Source should only start when `start_source(id)` is called.

#### `supports_replay` — **MUST for volatile sources**

The trait default is `true`. Sources that cannot honor `resume_from` MUST
override this and return `false`.

Return `true` only when the Source can replay from its persisted position, either
through an upstream durable log or through Drasi WAL durability.

#### `describe_schema` — **MAY**

Return a best-effort `SourceSchema` describing node and relation labels.
Database-like sources can derive this from configured tables or introspection;
webhook-style sources can derive it from route mappings.

#### `deprovision` — **MAY**

Clean up durable state or external resources:

- delete Drasi WAL data
- clear state-store partitions
- drop replication slots or cursors
- unregister consumers or leases

Use `SourceBase::deprovision_common()` for state-store cleanup.

#### `set_bootstrap_provider` — **SHOULD**

Sources backed by `SourceBase` SHOULD override this and delegate:

```rust,ignore
async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
    self.base.set_bootstrap_provider(provider).await;
}
```

#### `remove_position_handle` — **SHOULD for replay-capable sources**

Delegate to `SourceBase::remove_position_handle(query_id)` so stopped queries no
longer pin the minimum confirmed position.

#### `on_subscriptions_complete` — **MAY**

Use this for startup fences. A replay-capable source can use it to prevent
upstream acknowledgement from advancing until all startup subscribers have
registered their position handles.

#### `set_identity_provider` — **MAY**

Sources that authenticate with external systems MAY accept an
`IdentityProvider`. Delegate to `SourceBase::set_identity_provider(provider)` if
you use `SourceBase`.

### 5.4 Error handling

Source trait methods return `anyhow::Result` for implementor ergonomics. Add
context at the point where the failure is meaningful:

```rust,ignore
let connection = connect(&self.config)
    .await
    .with_context(|| format!("connecting source '{}'", self.base.id))?;
```

Use structured errors when the runtime needs to react:

```rust,ignore
return Err(SourceError::PositionUnavailable {
    source_id: self.base.id.clone(),
    requested: resume_bytes.clone(),
    earliest_available: Some(earliest.to_be_bytes().to_vec().into()),
}.into());
```

Inside spawned tasks:

- log errors with enough source ID context
- set `ComponentStatus::Error` on fatal failures
- do not panic for recoverable input or connection errors
- do not silently drop events after a durable append or upstream read succeeds

---

## 6. `SourceBase` — Reference

`SourceBase` encapsulates common source functionality. Use it unless you have a
very specific reason not to.

### Public surface

| Area | Methods |
|------|---------|
| Construction | `SourceBase::new(SourceBaseParams)` |
| Configuration persistence | `set_raw_config`, `raw_config`, `properties_or_serialize` |
| Runtime context | `initialize`, `context`, `state_store`, `identity_provider`, `set_identity_provider` |
| Status and lifecycle | `get_status`, `set_status`, `status_handle`, `set_task_handle`, `set_shutdown_tx`, `stop_common`, `clear_dispatchers`, `deprovision_common` |
| Subscription | `apply_subscription_settings`, `create_streaming_receiver`, `wait_for_subscribers`, `subscribe_with_bootstrap`, `subscribe_with_bootstrap_context`, `create_bootstrap_receiver`, `subscribe_with_replay` |
| Dispatch | `dispatch_source_change`, `dispatch_event`, `dispatch_events_batch` |
| Replay positions | `create_position_handle`, `remove_position_handle`, `compute_confirmed_position`, `compute_confirmed_source_position`, `prune_position_map`, `clear_sequence_position_map`, `set_next_sequence`, `set_position_comparator` |

### `SourceBaseParams`

```rust,ignore
pub struct SourceBaseParams {
    pub id: String,
    pub dispatch_mode: Option<DispatchMode>,
    pub dispatch_buffer_capacity: Option<usize>,
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    pub bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    pub auto_start: bool,
}
```

Builder methods:

```rust,ignore
let params = SourceBaseParams::new("source-id")
    .with_dispatch_mode(DispatchMode::Channel)
    .with_dispatch_buffer_capacity(1_000)
    .with_auto_start(true)
    .with_bootstrap_provider(provider);
```

### Subscription helpers

Use the highest-level helper that fits your source:

| Helper | Use when |
|--------|----------|
| `subscribe_with_bootstrap()` | Standard streaming source with optional bootstrap provider. |
| `subscribe_with_bootstrap_context()` | Bootstrap provider needs extra properties beyond source ID and label filters. |
| `subscribe_with_replay()` | Source uses a Drasi `WalProvider` and can replay retained transient events by sequence. |
| `create_streaming_receiver()` | Source needs fully custom bootstrap/replay behavior but still wants `SourceBase` dispatchers. |

### Dispatch helpers

Prefer:

```rust,ignore
self.base.dispatch_source_change(change).await?;
```

Use `dispatch_event()` when you need to attach `source_position` or custom
profiling metadata:

```rust,ignore
let mut wrapper = SourceEventWrapper::new(
    self.base.id.clone(),
    SourceEvent::Change(change),
    chrono::Utc::now(),
);
wrapper.set_source_position(bytes::Bytes::from(lsn.to_be_bytes().to_vec()));
self.base.dispatch_event(wrapper).await?;
```

Use `dispatch_events_batch()` when a poll cycle or database read produces many
events and all can be dispatched together.

### Position helpers

Replay-capable sources should understand these methods:

| Method | Meaning |
|--------|---------|
| `set_position_comparator` | Supplies source-specific comparison for opaque `source_position` bytes. |
| `compute_confirmed_position` | Minimum confirmed framework sequence across live subscribers. |
| `compute_confirmed_source_position` | Converts the confirmed sequence to the latest known source-native position at or before that sequence. |
| `prune_position_map` | Removes sequence-to-source-position mappings after upstream feedback succeeds. |
| `clear_sequence_position_map` | Clears stale mappings before rewinding/restarting a native replay stream. |

> 🔴 **MUST** — Do not advance an upstream cursor, commit offsets, prune WAL, or
> acknowledge irreversible state past the minimum confirmed subscriber position.

---

## 7. The Source Event Data Contract

Sources emit a fixed event structure.

### `SourceChange`

`SourceChange` is the graph mutation consumed by the query engine:

```rust,ignore
pub enum SourceChange {
    Insert { element: Element },
    Update { element: Element },
    Delete { metadata: ElementMetadata },
    Future { future_ref: FutureElementRef },
}
```

Source plugins normally emit `Insert`, `Update`, and `Delete`. `Future` is an
internal query-engine concept and SHOULD NOT be emitted by external Source
plugins.

### `Element`

```rust,ignore
pub enum Element {
    Node {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
    },
    Relation {
        metadata: ElementMetadata,
        in_node: ElementReference,
        out_node: ElementReference,
        properties: ElementPropertyMap,
    },
}
```

Node and relation identity is always source-scoped:

```rust,ignore
pub struct ElementReference {
    pub source_id: Arc<str>,
    pub element_id: Arc<str>,
}
```

Use stable element IDs. If an external record can appear in multiple tables,
topics, tenants, or partitions, include enough namespace in `element_id` to avoid
collisions inside the same Source.

### `ElementMetadata`

```rust,ignore
pub struct ElementMetadata {
    pub reference: ElementReference,
    pub labels: Arc<[Arc<str>]>,
    pub effective_from: u64,
}
```

`effective_from` is milliseconds since Unix epoch. Zero is allowed for bootstrap
data when the original effective time is unknown. Nanosecond or microsecond
timestamps will be rejected by validation in core model helpers and can corrupt
temporal behavior.

### `ElementValue`

Properties use `ElementValue`:

```rust,ignore
pub enum ElementValue {
    Null,
    Bool(bool),
    Float(OrderedFloat<f64>),
    Integer(i64),
    String(Arc<str>),
    List(Vec<ElementValue>),
    Object(ElementPropertyMap),
}
```

Map external types deliberately:

- preserve IDs as strings when they are identifiers, even if they look numeric
- normalize timestamps to integers or ISO strings consistently
- avoid lossy float conversions for values that must be exact
- avoid embedding opaque source payloads unless queries actually need them

### `SourceEventWrapper`

`SourceBase` dispatches `SourceEventWrapper` values:

```rust,ignore
pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: DateTime<Utc>,
    pub profiling: Option<ProfilingMetadata>,
    pub sequence: Option<u64>,
    pub source_position: Option<Bytes>,
}
```

`SourceBase::dispatch_event()` assigns `sequence` when it is `None`. Sources
should set `source_position` before dispatch when the event has a replayable
upstream position. `source_position` is opaque to the framework and must be no
larger than 64 KB for checkpoint persistence.

### Why there is no source templating

Reactions format query results for arbitrary downstream systems, so they support
output templating. Sources do the opposite: they normalize external data into the
Drasi graph model. The final emitted shape is always `SourceChange`.

If your source accepts arbitrary external payloads, implement or reuse a mapping
layer before creating `SourceChange`; do not add a templating system after
`SourceChange`.

---

## 8. Resilience & Persistence Patterns

Sources fall into three broad resilience categories.

### Pattern A — Native upstream replay

Use this when the external system already has a durable ordered change log:

- database commit-log or CDC positions
- message-stream topic, partition, and offset positions
- ordered file, object-store, or event-log cursors
- any other durable cursor that can be replayed after restart

Required behavior:

1. Encode the upstream position into `SourceEventWrapper.source_position`.
2. On subscribe, parse `settings.resume_from`.
3. Validate that the requested position is still available.
4. Rewind, seek, or restart the ingestion task if needed.
5. Return `SourceError::PositionUnavailable` for pruned gaps.
6. Compare positions correctly with `PositionComparator`.
7. Advance upstream feedback only after all position handles confirm progress.

Example skeleton:

```rust,ignore
async fn subscribe(&self, settings: SourceSubscriptionSettings) -> Result<SubscriptionResponse> {
    if let Some(ref resume_bytes) = settings.resume_from {
        let requested = decode_native_position(resume_bytes)?;
        let earliest = self.earliest_available_position().await?;

        if requested < earliest {
            return Err(SourceError::PositionUnavailable {
                source_id: self.base.id.clone(),
                requested: resume_bytes.clone(),
                earliest_available: Some(encode_native_position(earliest)),
            }.into());
        }

        self.ensure_stream_can_replay_from(requested).await?;
    }

    self.base.subscribe_with_bootstrap(&settings, "my-native-source").await
}
```

### Pattern B — Transient source with Drasi WAL durability

Use this when the external system pushes events to Drasi and cannot replay them
later. Webhook, RPC, and in-process sources commonly use this pattern.

Configuration:

```yaml
durability:
  enabled: true
  maxEvents: 10000
  capacityPolicy: RejectIncoming
```

`DurabilityConfig` maps to a `WalProvider` registration:

```rust,ignore
if self.config.durability.as_ref().is_some_and(|d| d.enabled) {
    let ctx = self.base.context().await.context("Context not initialized")?;
    let wal = ctx.wal_provider.context("Durability enabled but no WAL provider configured")?;
    wal.register(&self.base.id, self.config.durability.as_ref().unwrap().to_wal_config()).await?;
}
```

Rules:

- Append to WAL before acknowledging the external producer.
- Use the assigned WAL sequence as the event sequence or source position.
- On restart, call `head_sequence()` and `SourceBase::set_next_sequence(head)`.
- Use `subscribe_with_replay()` when `resume_from` is present.
- Periodically prune only up to `compute_confirmed_position()`.
- Delete WAL data in `deprovision()`.

Capacity policies:

| Policy | Behavior | Use when |
|--------|----------|----------|
| `RejectIncoming` | Reject new events when the WAL is full. Upstream should retry. | Data safety matters more than accepting every new event. |
| `OverwriteOldest` | Evict oldest retained events to accept new ones. Slow queries may hit gaps. | Availability matters more than preserving replay for slow consumers. |

### Pattern C — Volatile source

Use this only when missed events are acceptable or the source is purely for
tests/demos.

Rules:

- Override `supports_replay()` to return `false`.
- Do not claim position handles.
- Do not emit misleading `source_position` values.
- Document that persistent query recovery requires re-bootstrap or reset.

### Startup and restart safety

Sources that produce events from a background task SHOULD call
`wait_for_subscribers()` before dispatching after restart if there is a risk of
producing live events before subscribers are attached. Otherwise, a source can
dispatch to stale or nonexistent channel-mode dispatchers and advance positions
past changes no query received.

### Gap handling

When a source cannot satisfy `resume_from`, return
`SourceError::PositionUnavailable`. The query manager applies the query recovery
policy:

- strict policy: query transitions to Error
- auto-reset policy: query state is wiped and it re-subscribes from bootstrap

Do not silently start from "now" when a checkpoint requested an older position.
That creates an undetected data gap.

---

## 9. Bootstrap Providers

Bootstrap providers deliver initial data to queries. They are intentionally
separate from streaming sources.

```rust,ignore
#[async_trait]
pub trait BootstrapProvider: Send + Sync {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        settings: Option<&SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult>;
}
```

### Bootstrap flow

1. Query subscribes with `enable_bootstrap: true`.
2. Source delegates to `SourceBase::subscribe_with_bootstrap(...)`.
3. `SourceBase` creates a streaming receiver.
4. `SourceBase` starts the configured `BootstrapProvider`.
5. Provider sends `BootstrapEvent` values through the bootstrap channel.
6. Provider returns `BootstrapResult` with handoff metadata.
7. Query processes bootstrap events and then live stream events.

### `BootstrapResult`

```rust,ignore
pub struct BootstrapResult {
    pub event_count: usize,
    pub last_sequence: Option<u64>,
    pub sequences_aligned: bool,
    pub source_position: Option<Bytes>,
}
```

Use these fields carefully:

- `event_count` is the number of bootstrap events sent.
- `last_sequence` is the bootstrap snapshot boundary when the provider has a
  sequence namespace.
- `sequences_aligned` means the bootstrap sequence namespace matches the
  streaming Source's sequence namespace.
- `source_position` is the native snapshot boundary. When present, the framework
  can persist it as the initial checkpoint so crash recovery resumes without
  re-bootstrapping.

### Provider design categories

| Provider category | Use case |
|-------------------|----------|
| Native snapshot provider | Reads an initial snapshot from the same system that provides the live stream. This is the only category that should usually set `sequences_aligned = true`. |
| File or fixture provider | Loads deterministic bootstrap data for tests, demos, or offline startup. |
| Remote query provider | Pulls an initial snapshot from an existing API or service. |
| In-memory provider | Replays state accumulated by an in-process source. |
| No-op provider | Explicitly returns no initial data when a source is stream-only. |

### Mix-and-match

Any Source can use any provider:

```yaml
sources:
  - id: orders-webhook
    source_type: webhook
    bootstrap_provider:
      type: file-snapshot
      file_paths:
        - "/data/initial-orders.jsonl"
    properties:
      bind: 0.0.0.0
      port: 8080
```

This is useful when the initial dataset lives in one system but live changes
arrive through another. Only set `sequences_aligned = true` when the bootstrap
and stream positions are actually in the same ordered namespace.

### Implementing a provider for your source

Providers SHOULD:

- honor `request.node_labels` and `request.relation_labels`
- send `BootstrapEvent` values with stable IDs and millisecond timestamps
- return accurate handoff metadata
- fail loudly on malformed data or inaccessible upstream snapshots
- avoid keeping `BootstrapContext` beyond the call

---

## 10. Transforming External Data into Graph Changes

Every Source has a mapping boundary: external data enters in a system-specific
shape and leaves as Drasi graph changes.

### Mapping decisions

Decide these before coding:

| Question | Guidance |
|----------|----------|
| What is a node? | Usually a row, document, resource, message entity, or API object. |
| What is a relation? | Use relations when queries need graph traversal between nodes, not just nested data. |
| What is the stable element ID? | Include source-local namespace such as table, tenant, partition, or resource kind. |
| What labels are emitted? | Labels are query-visible API; keep them stable and document them. |
| What is `effective_from`? | Use the source event time when available; otherwise use ingestion time in milliseconds. |
| What properties are queryable? | Include only data that queries need; normalize types consistently. |
| How are deletes represented? | Deletes need metadata only; labels must match the inserted/updated element identity. |

### Database CDC

Database sources commonly map tables to node labels. A row insert/update becomes
a node `Insert` or `Update`; a delete becomes `Delete` metadata. Relation
emission depends on configuration or schema conventions.

Best practices:

- use configured key columns for stable IDs
- normalize table names into labels consistently
- include schema/table namespace when multiple tables can share keys
- attach the database log position as `source_position`
- validate resume positions before restarting replication

### Request/response and in-process ingress

Ingress sources may accept either canonical Drasi events or source-specific
payloads that are mapped into graph changes.

Best practices:

- validate request payloads before acknowledging
- if durability is enabled, append to WAL before returning success
- make batch handling atomic enough for your durability contract
- document accepted payload formats with insert/update/delete examples
- reject ambiguous element IDs or labels rather than guessing

### Message-stream sources

Stream sources usually need explicit mapping configuration because topic payloads
vary.

Best practices:

- include partition and offset in source-position encoding
- use a custom comparator if a single byte-lexicographic comparison is not valid
- avoid committing offsets past the minimum confirmed subscriber position
- document message formats and mapping configuration

### Polling API sources

Polling sources fetch snapshots or deltas from APIs. They must decide how to
detect deletes and how to produce stable positions.

Best practices:

- persist cursors, ETags, timestamps, or page tokens when the API provides them
- treat full snapshots carefully: missing records may be deletes only if the API
  contract guarantees completeness
- avoid dispatching unchanged records unless the query semantics require it
- handle rate limits without reporting success-shaped data gaps

---

## 11. Secrets, Environment Variables, and Identity

### `ConfigValue<T>`

Dynamic plugin DTOs SHOULD use `ConfigValue<T>` for values that can come from
literal config, environment variables, or secret references:

```rust,ignore
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = source::my_source::MySourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySourceConfigDto {
    #[serde(default = "default_interval_ms")]
    #[schema(value_type = ConfigValueU64)]
    pub interval_ms: ConfigValue<u64>,

    #[serde(default = "default_label")]
    #[schema(value_type = ConfigValueString)]
    pub label: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub api_key: ConfigValue<String>,
}
```

Resolve DTO values in the descriptor:

```rust,ignore
let mapper = DtoMapper::new();
let interval_ms = mapper.resolve_typed(&dto.interval_ms).await?;
let label = mapper.resolve_string(&dto.label).await?;
let api_key = mapper.resolve_string(&dto.api_key).await?;
```

### `properties()` and persistence

`properties()` is not a redacted inspection DTO. It is the live component's
configuration persistence hook.

> 🔴 **MUST** — Return all properties needed to recreate the Source, including
> passwords, tokens, connection strings, SASL credentials, and secret references.

For dynamic plugin descriptors, preserve raw config:

```rust,ignore
let mut source = MySourceBuilder::new(id)
    .with_config(config)
    .with_auto_start(auto_start)
    .build()?;

source.base_mut().set_raw_config(config_json.clone());
```

Then implement:

```rust,ignore
fn properties(&self) -> HashMap<String, serde_json::Value> {
    self.base.properties_or_serialize(&self.config)
}
```

This preserves unresolved `ConfigValue` envelopes when the server snapshots
configuration, instead of persisting only resolved secret values.

### `IdentityProvider`

Sources that authenticate through runtime identities MAY implement
`set_identity_provider()` and use `self.base.identity_provider().await` during
startup or request signing.

Programmatically-set identity providers take precedence over providers injected
through `SourceRuntimeContext`.

---

## 12. Packaging as a Dynamic Plugin

Sources can be compiled as shared libraries and loaded at runtime by the Drasi
server. The plugin SDK exposes Source instances through an FFI-safe vtable.

### Cargo.toml additions

```toml
[lib]
crate-type = ["lib", "cdylib"]

[features]
default = []
dynamic-plugin = []

[dependencies]
drasi-lib = "0.8"
drasi-core = "0.5"
drasi-plugin-sdk = "0.8"
utoipa = { version = "4", features = ["chrono"] }
```

Keeping `"lib"` alongside `"cdylib"` lets the crate be used in tests and static
embedding scenarios while still producing a plugin library.

> 🔴 **MUST** — The host and dynamic plugin must use the same
> `drasi-plugin-sdk` version. Treat the SDK version as part of the ABI.

### Configuration DTOs

Create DTOs in `descriptor.rs`. DTOs are serialized from API/config JSON and
also provide OpenAPI schemas.

```rust,ignore
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = source::my_source::MySourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySourceConfigDto {
    #[serde(default = "default_interval_ms")]
    #[schema(value_type = ConfigValueU64)]
    pub interval_ms: ConfigValue<u64>,

    #[serde(default = "default_label")]
    #[schema(value_type = ConfigValueString)]
    pub label: ConfigValue<String>,
}

fn default_interval_ms() -> ConfigValue<u64> {
    ConfigValue::Static(1_000)
}

fn default_label() -> ConfigValue<String> {
    ConfigValue::Static("Counter".to_string())
}

#[derive(OpenApi)]
#[openapi(components(schemas(MySourceConfigDto)))]
struct MySourceSchemas;
```

The `#[schema(as = ...)]` path must match `config_schema_name()` with dots
instead of Rust path separators.

### The descriptor

```rust,ignore
use crate::{MySourceBuilder, MySourceConfig};
use drasi_lib::Source;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

pub struct MySourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MySourceDescriptor {
    fn kind(&self) -> &str {
        "my-source"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.my_source.MySourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MySourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Source>> {
        let dto: MySourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = MySourceConfig {
            interval_ms: mapper.resolve_typed(&dto.interval_ms).await?,
            label: mapper.resolve_string(&dto.label).await?,
        };

        let mut source = MySourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        source.base_mut().set_raw_config(config_json.clone());

        Ok(Box::new(source))
    }
}
```

### Exporting the plugin

In `lib.rs`:

```rust,ignore
pub mod descriptor;

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "my-source",
    core_version = "0.5.3",
    lib_version = "0.8.5",
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::MySourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
```

Set `core_version` and `lib_version` to the exact `drasi-core` and `drasi-lib`
versions used to build the plugin.

If the crate also provides a bootstrap provider, add a
`BootstrapPluginDescriptor` and include it in `bootstrap_descriptors`.

### Build

```bash
cargo build --release --features dynamic-plugin
```

Typical outputs:

- Linux: `target/release/libmy_source.so`
- macOS: `target/release/libmy_source.dylib`
- Windows: `target/release/my_source.dll`

Copy the shared library into the server's plugin directory.

### FFI compatibility — what source authors need to know

The following types and traits cross the dynamic plugin boundary:

- `Source` trait methods through `SourceVtable`
- `SourceSubscriptionSettings` fields deconstructed into FFI arguments
- `SubscriptionResponse` reconstructed field-by-field
- `SourceEventWrapper`, `BootstrapEvent`, and `SourceChange` as opaque pointers
- `ComponentStatus` and `DispatchMode` as FFI enums
- `BootstrapProvider` through cross-plugin vtables

If you are only implementing a Source plugin, do not change these core types. If
you are modifying `drasi-lib`, `drasi-core`, or the plugin SDK itself, the plugin
SDK vtables, host proxies, and FFI SDK version must be updated together.

---

## 13. Testing

### Unit tests

Test the parts that are deterministic without an external system:

- config defaults and validation
- DTO mapping and `ConfigValue` resolution
- payload-to-`SourceChange` mapping
- stable element IDs and labels
- timestamp units
- `properties()` roundtrip
- `describe_schema()` output
- invalid input errors

Example mapping test:

```rust,ignore
#[test]
fn maps_order_row_to_order_node() {
    let change = map_order_row(row_json!({
        "id": "A123",
        "status": "open",
        "updated_at_ms": 1_771_000_000_000_u64
    })).unwrap();

    match change {
        SourceChange::Insert { element: Element::Node { metadata, properties } } => {
            assert_eq!(metadata.reference.element_id.as_ref(), "orders/A123");
            assert_eq!(metadata.labels[0].as_ref(), "Order");
            assert_eq!(metadata.effective_from, 1_771_000_000_000);
            assert_eq!(properties["status"], ElementValue::String("open".into()));
        }
        other => panic!("expected order insert, got {other:?}"),
    }
}
```

### Source trait tests

Use direct source instances to test lifecycle and subscription behavior:

- `start()` transitions to Running
- `stop()` transitions to Stopped and clears channel-mode dispatchers
- `subscribe()` returns a receiver
- `supports_replay()` matches configured durability or native capabilities
- `set_bootstrap_provider()` delegates to `SourceBase`

### Replay and checkpoint tests

Replay-capable sources need explicit tests for:

- malformed `resume_from` bytes
- resume from an available position
- resume from a pruned/unavailable position
- bootstrap skipped when `resume_from` is present
- `last_sequence` advances `SourceBase` sequence generation
- position handles pin upstream feedback
- dropping/removing position handles advances the minimum confirmed position
- source-position comparator filters duplicate replay events

### Bootstrap tests

Bootstrap-capable sources/providers should test:

- label filtering through `BootstrapRequest`
- event counts
- `last_sequence`, `sequences_aligned`, and `source_position`
- provider errors propagated through the bootstrap result channel
- mix-and-match provider configuration where supported

### Integration tests

Use integration tests for real external systems or protocol servers:

- database containers for CDC and snapshot bootstrap
- local protocol servers for ingress sources
- embedded brokers or test doubles for stream sources
- failure/restart tests for durability

Run your source crate's tests from its package directory:

```bash
cargo test
```

For documentation-only changes to this guide, no cargo build or test is required.

---

## 14. Conformance Checklist

#### Cargo

- [ ] Crate name follows a stable, publishable convention such as
      `drasi-source-*`.
- [ ] Pins compatible `drasi-lib`, `drasi-core`, and `drasi-plugin-sdk`
      versions from crates.io.
- [ ] Dynamic plugins include `crate-type = ["lib", "cdylib"]`.
- [ ] Optional plugin export is gated behind a `dynamic-plugin` feature when
      that improves test ergonomics.

#### Source trait

- [ ] Implements every required `Source` method.
- [ ] Delegates `initialize()` to `SourceBase`.
- [ ] `properties()` returns all persisted configuration, including secrets or
      secret references.
- [ ] Volatile sources override `supports_replay() -> false`.
- [ ] Replay-capable sources validate `resume_from`.
- [ ] `subscribe()` delegates to `SourceBase` or calls
      `apply_subscription_settings()` manually.
- [ ] `set_bootstrap_provider()` delegates to `SourceBase` if bootstrap is
      supported.
- [ ] `remove_position_handle()` delegates for replay-capable sources.

#### Event contract

- [ ] Emits stable `ElementReference { source_id, element_id }` values.
- [ ] Emits stable labels and documented properties.
- [ ] Uses millisecond `effective_from` values.
- [ ] Represents deletes with correct metadata and labels.
- [ ] Does not emit `SourceChange::Future` from an external plugin.
- [ ] Attaches `source_position` to replayable live events.
- [ ] Keeps `source_position` below 64 KB.

#### Resilience

- [ ] Native-log sources encode and compare positions correctly.
- [ ] Native-log sources return `SourceError::PositionUnavailable` for pruned
      gaps.
- [ ] Transient durable sources append to WAL before acknowledging upstream.
- [ ] WAL pruning/offset commits/flush feedback use minimum confirmed subscriber
      position.
- [ ] Startup/restart does not dispatch events before subscribers are attached
      when that would create gaps.
- [ ] Stop logic clears channel-mode dispatchers.
- [ ] Deprovision removes durable state when cleanup is requested.

#### Bootstrap

- [ ] Bootstrap provider honors node/relation label filters.
- [ ] Bootstrap result includes accurate handoff metadata.
- [ ] Source does not bootstrap when `resume_from` is present.
- [ ] `sequences_aligned` is true only for aligned sequence namespaces.

#### Dynamic plugin packaging

- [ ] DTOs use `ConfigValue<T>` for secrets/env-configurable values.
- [ ] DTOs use `#[serde(rename_all = "camelCase", deny_unknown_fields)]`.
- [ ] DTO schema path matches `config_schema_name()`.
- [ ] Descriptor resolves `ConfigValue` asynchronously through `DtoMapper`.
- [ ] Descriptor preserves raw config via `source.base_mut().set_raw_config(...)`
      or an equivalent source helper.
- [ ] `export_plugin!` lists the source descriptor and any bootstrap descriptors.

#### Testing

- [ ] Config defaults and validation are covered.
- [ ] Payload mapping is covered.
- [ ] Lifecycle and subscription behavior are covered.
- [ ] Bootstrap behavior is covered when supported.
- [ ] Replay, gap, and pruning behavior are covered when supported.
- [ ] External integration tests are documented or automated.

#### Documentation

- [ ] README documents input format and graph mapping.
- [ ] README documents replay/durability limitations.
- [ ] README includes configuration examples.
- [ ] README states whether bootstrap is supported.
- [ ] README documents operational behavior: reconnect/backoff, rate limits,
      status transitions, and shutdown semantics.

---

## 15. Anti-Patterns

### "I'll leave `supports_replay()` at the default"

The default is `true`. If your source cannot honor `resume_from`, override it to
return `false`. Claiming replay support without implementing replay can cause
persistent queries to restart with undetected gaps.

### "I'll just start from now if `resume_from` is too old"

Do not hide gaps. Return `SourceError::PositionUnavailable` and let the query's
recovery policy decide whether to fail or reset.

### "I'll re-bootstrap even though `resume_from` is set"

A resuming query already has indexed state. Re-bootstrap can duplicate inserts,
resurrect deleted records, or corrupt query state.

### "I'll filter secrets out of `properties()`"

`properties()` is used for configuration persistence. Redacting it breaks restart.
Protect the persisted config file instead.

### "I'll emit nanosecond timestamps"

`effective_from` is milliseconds. Nanosecond values are five to six orders of
magnitude too large and will fail validation.

### "I'll add templates so users can shape source output"

Sources normalize external data into `SourceChange`. If users need payload
mapping, provide mapping configuration before `SourceChange` creation. Do not add
reaction-style output templating after the graph contract.

### "I'll commit upstream offsets as soon as I read messages"

Only commit, flush, or prune past positions that all relevant subscribers have
durably processed. Use position handles and minimum confirmed positions.

### "I'll use Broadcast for a replay-capable source"

Broadcast mode cannot filter replay per subscriber. Use Channel mode unless you
have proven the source does not need subscriber-specific replay handling.

### "I'll ignore spawned task errors"

Fatal ingestion errors should be logged and reflected in `ComponentStatus::Error`.
Do not let a source appear Running after its only ingestion task failed.

### "I'll keep old channel dispatchers after stop"

Channel-mode dispatchers are per subscriber. Clear them on stop or a later
restart can dispatch events to dead receivers and silently lose data.

### "I'll change source event types without touching the plugin SDK"

Several source types cross the dynamic plugin FFI boundary. If you modify
`Source`, `SourceSubscriptionSettings`, `SubscriptionResponse`,
`SourceEventWrapper`, `BootstrapEvent`, `SourceChange`, `ComponentStatus`, or
`DispatchMode`, update the plugin SDK/host SDK mappings and bump the FFI SDK
version.

---

## Appendix A — Full Type Shapes

These shapes are for orientation. Check the source files for the authoritative
definitions.

### Source subscription settings

```rust,ignore
pub struct SourceSubscriptionSettings {
    pub source_id: String,
    pub enable_bootstrap: bool,
    pub query_id: String,
    pub nodes: HashSet<String>,
    pub relations: HashSet<String>,
    pub resume_from: Option<Bytes>,
    pub request_position_handle: bool,
    pub last_sequence: Option<u64>,
}
```

### Subscription response

```rust,ignore
pub struct SubscriptionResponse {
    pub query_id: String,
    pub source_id: String,
    pub receiver: Box<dyn ChangeReceiver<SourceEventWrapper>>,
    pub bootstrap_receiver: Option<BootstrapEventReceiver>,
    pub position_handle: Option<Arc<AtomicU64>>,
    pub bootstrap_result_receiver: Option<oneshot::Receiver<anyhow::Result<BootstrapResult>>>,
}
```

### Source event wrapper

```rust,ignore
pub enum SourceEvent {
    Change(SourceChange),
    Control(SourceControl),
}

pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: DateTime<Utc>,
    pub profiling: Option<ProfilingMetadata>,
    pub sequence: Option<u64>,
    pub source_position: Option<Bytes>,
}
```

### Graph changes

```rust,ignore
pub enum SourceChange {
    Insert { element: Element },
    Update { element: Element },
    Delete { metadata: ElementMetadata },
    Future { future_ref: FutureElementRef },
}

pub enum Element {
    Node {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
    },
    Relation {
        metadata: ElementMetadata,
        in_node: ElementReference,
        out_node: ElementReference,
        properties: ElementPropertyMap,
    },
}

pub struct ElementMetadata {
    pub reference: ElementReference,
    pub labels: Arc<[Arc<str>]>,
    pub effective_from: u64,
}

pub struct ElementReference {
    pub source_id: Arc<str>,
    pub element_id: Arc<str>,
}
```

### Bootstrap types

```rust,ignore
pub struct BootstrapRequest {
    pub query_id: String,
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    pub request_id: String,
}

pub struct BootstrapEvent {
    pub source_id: String,
    pub change: SourceChange,
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
}

pub struct BootstrapResult {
    pub event_count: usize,
    pub last_sequence: Option<u64>,
    pub sequences_aligned: bool,
    pub source_position: Option<Bytes>,
}
```

### Durability config

```rust,ignore
pub struct DurabilityConfig {
    pub enabled: bool,
    pub max_events: u64,
    pub capacity_policy: CapacityPolicy,
}

pub enum CapacityPolicy {
    RejectIncoming,
    OverwriteOldest,
}
```

---

## Appendix B — Glossary

| Term | Meaning |
|------|---------|
| Source | Drasi component that ingests external data and emits graph changes. |
| Bootstrap | Initial data snapshot for a query. |
| Streaming | Live change delivery after subscription. |
| `SourceChange` | Insert, update, or delete graph mutation consumed by the query engine. |
| `SourceEventWrapper` | Envelope around `SourceChange` with source ID, timestamp, sequence, source position, and profiling metadata. |
| Framework sequence | Monotonic `u64` assigned by `SourceBase` for ordering and query checkpoints. |
| Source position | Opaque bytes owned by the Source that identify an upstream replay position. |
| Position handle | Shared atomic used by a query to report its last durably processed framework sequence back to the Source. |
| Replay | Serving events from a prior persisted source position after restart or resubscription. |
| WAL | Write-ahead log used by transient sources for local durability and replay. |
| `PositionUnavailable` | Structured error meaning the requested replay position has been pruned or cannot be served. |
| Dispatch mode | Channel or Broadcast routing from Source to subscribed queries. |
| `ConfigValue<T>` | Plugin SDK wrapper for static, environment, or secret-backed configuration values. |
| Dynamic plugin | A Source compiled as a `cdylib` and loaded through the plugin SDK FFI boundary. |
