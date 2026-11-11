# Drasi Source Plugin Developer Guide

## Purpose

This document is a guide for software developers who create and maintain custom
**Source plugins** in Rust for `drasi-lib`.

This guide covers the **ordinary Source family**: `drasi_lib::Source`,
`SourceBase`, and `drasi-plugin-sdk`. Native source plugins instead implement
`NativeComponent` and `NativeSource` from the
[native SDK](../computation-plugin-sdk/README.md); they do not use `SourceBase`
or this family's vtables. Both families execute through ComputationGraph.

Sources are the components of Drasi that connect to external systems and make the
changes to data from those systems available to Continuous Queries.

---

## Table of Contents

1. [Conventions: MUST, SHOULD, MAY](#1-conventions-must-should-may)
2. [Overview & Mental Model](#2-overview--mental-model)
    1. [Key invariants](#key-invariants)
    2. [A source in context](#a-source-in-context)
3. [Quickstart: A Minimal Working Source](#3-quickstart-a-minimal-working-source)
4. [Core Concepts](#4-core-concepts)
    1. [The Component Lifecycle](#41-the-component-lifecycle)
    2. [Runtime-Owned Query Subscriptions](#42-runtime-owned-query-subscriptions)
    3. [Dispatch, Backpressure, and Subscriber Isolation](#43-dispatch-backpressure-and-subscriber-isolation)
    4. [Bootstrap Is Separate from Streaming](#44-bootstrap-is-separate-from-streaming)
    5. [Replay, Checkpoints, and Source Positions](#45-replay-checkpoints-and-source-positions)
    6. [The Runtime Context](#46-the-runtime-context)
5. [The `Source` Trait](#5-the-source-trait)
    1. [Public import paths](#public-import-paths)
    2. [Required methods](#51-required-methods)
    3. [Lifecycle — the runtime call sequence](#52-lifecycle--the-runtime-call-sequence)
    4. [Optional capability overrides](#53-optional-capability-overrides)
    5. [Error handling](#54-error-handling)
6. [`SourceBase` — Reference](#6-sourcebase--reference)
7. [The Source Event Data Contract](#7-the-source-event-data-contract)
8. [Resilience & Persistence Patterns](#8-resilience--persistence-patterns)
    1. [Pattern A — Native upstream replay](#pattern-a--native-upstream-replay)
    2. [Pattern B — Transient source with Drasi WAL durability](#pattern-b--transient-source-with-drasi-wal-durability)
    3. [Checkpoint and upstream-feedback timing](#checkpoint-and-upstream-feedback-timing)
    4. [Sources that open an inbound port (ingress)](#sources-that-open-an-inbound-port-ingress)
    5. [Using the state store to survive restarts](#using-the-state-store-to-survive-restarts)
    6. [Worked example: a durable ingress source](#worked-example-a-durable-ingress-source)
9. [Bootstrap Providers](#9-bootstrap-providers)
10. [Transforming External Data into Graph Changes](#10-transforming-external-data-into-graph-changes)
11. [Secrets, Environment Variables, and Identity](#11-secrets-environment-variables-and-identity)
12. [Packaging as a Dynamic Plugin](#12-packaging-as-a-dynamic-plugin)
13. [Testing](#13-testing)
14. [Conformance Checklist](#14-conformance-checklist)
15. [Anti-Patterns](#15-anti-patterns)
16. [Appendix A — Full Type Shapes](#appendix-a--full-type-shapes)
17. [Appendix B — Glossary](#appendix-b--glossary)
18. [Appendix C — Repository Conventions](#appendix-c--repository-conventions)

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

### First, the vocabulary

Nine terms appear throughout this guide. Skim them now; each is defined in
context later and again in [Appendix B](#appendix-b--glossary).

| Term | What it is |
|------|------------|
| **`SourceChange`** | The graph mutation you emit: an `Insert`, `Update`, or `Delete` of a node or relation. |
| **`SourceEventWrapper`** | The envelope used by sources and downstream consumers: an event, timestamp, required `sequence: u64`, and optional `source_position`. |
| **`SourceBase`** | The helper you embed in your source struct. It owns subscriptions, dispatch, status, sequencing, replay, and shutdown. |
| **Bootstrap** | The initial snapshot a query receives before live changes, produced by a `BootstrapProvider` — separately from streaming. |
| **Event sequence** | A source-local monotonic `u64` supplied by the source or allocated with `SourceBase::next_sequence()`, used for ordering and query checkpoints. |
| **Source position** | Opaque bytes you attach to an event identifying an upstream replay position (an offset, an LSN, a WAL sequence). |
| **Replay** | Re-serving events from a persisted position after a restart so a resuming query misses nothing. |
| **State store** | A key/value store `drasi-lib` provides so your source can persist small state (cursors, seen-IDs) across restarts ([§8](#using-the-state-store-to-survive-restarts)). |
| **WAL** | Drasi's write-ahead log, which lends durability and replay to transient sources that have no upstream log of their own ([§8](#pattern-b--transient-source-with-drasi-wal-durability)). |

### Key invariants

1. **Sources are active producers.** A Source owns the connection, listener,
   poller, consumer, or in-memory API that ingests data from an external system.
    It pushes `SourceEventWrapper` values to the runtime through `SourceBase`, which
    preserves their required sequence numbers.

2. **Queries subscribe to Sources.** Sources do not know query text and do not
   evaluate Cypher / GQL. The runtime calls `subscribe(settings)` for each query that
   needs the Source. `settings` contains the query ID plus the node and relation
   labels allocated to that Source.

3. **The emitted shape is fixed.** Sources may transform external payloads into a
   graph model, but after that transformation they always emit the same Drasi
   data contract: `SourceChange::Insert`, `Update`, or `Delete` wrapped in
   a `SourceEventWrapper`. Sources do not use reaction-style output templates.

4. **Resilience is part of the Source contract.** A Source must be explicit about
   whether it can replay from a persisted position. Native-log sources use
   upstream positions such as WAL LSNs or stream offsets. Transient ingress
   sources may use Drasi's WAL durability layer.

5. **Bootstrap and streaming are separate.** Bootstrap providers produce initial
   data for a query. The streaming Source produces live changes. The two can be
   implemented by the same crate, but the runtime treats them as separate
   capabilities.

6. **The runtime owns admitted objects.** Use
   `DrasiLib::builder().with_source_factory(...)` to construct a source with
   a framework-assigned UUID. The runtime binds context, initializes that
   actual object once, and owns its start/stop cycles and final disposal.
   Destructive resource deletion requires a separate explicit request.

7. **FFI safety matters.** Dynamic source plugins are loaded through a versioned C
   ABI vtable. Supported unwinding panics are caught by the plugin SDK, but source
   implementations SHOULD return errors rather than panic.

### A source in context

The Quickstart below builds `MySource`. In a real application, the source is one
piece of a `drasi-lib` instance alongside at least one query and, usually, at
least one reaction. The application declares constructors/configuration through
`DrasiLib::builder()`, which owns construction and lifecycle, then starts the runtime.

The example below uses the source you build in this guide so the moving parts are
visible without an external database, stream, or service.

```rust,ignore
use std::sync::Arc;
use drasi_lib::{DrasiLib, Query};
use drasi_lib::computation::v1::ComponentFactory;
use my_source::{MySource, MySourceConfig};

async fn run(query_factory: Arc<dyn ComponentFactory>) -> anyhow::Result<DrasiLib> {
    let query = Query::cypher("counter-query")
        .query("MATCH (c:Counter) RETURN c.value AS value")
        .from_input("my-source")
        .enable_bootstrap(false)
        .build()?;

    let drasi = DrasiLib::builder()
        .with_query_factory(query_factory)
        .with_source_factory("my-source", true, |id| {
            MySource::new(id.to_string(), MySourceConfig::default())
        })
        .with_query(query)
        .build()
        .await?;

    drasi.start().await?;
    Ok(drasi)
}
```

This guide focuses on the source plugin itself. The query above is context: it
shows where the source fits once you have built it.

`"my-source"` and `"counter-query"` are editable graph names, not component IDs.
The source constructor receives an immutable UUID and MUST return it unchanged
from `Source::id()`. `Query::from_input` selects an upstream component by name.
Preconstructed objects require explicit `with_allow_external_component_ids(true)`
and `with_source(name, source)`; they are not the default admission path.

The embedding application supplies an explicitly selected query factory that
supports ordinary query authoring; see the
[Core developer guide](../../lib/docs/developer-guide/README.md). Loading a
source plugin alone neither selects that factory nor authorizes managed updates.
Retain the returned runtime owner rather than dropping an active instance as a
substitute for awaited lifecycle cleanup.

---

## 3. Quickstart: A Minimal Working Source

This walkthrough creates a small Source that emits a `Counter` node every second.
It is intentionally simple: no external database, no bootstrap provider, and no
replay. Later sections add those capabilities, and [§8](#8-resilience--persistence-patterns)
illustrates a durable source that opens its own inbound port.

> ⏱️ Budget about 15 minutes of *typing*, plus a first `cargo build` that
> compiles `drasi-lib`, `drasi-core`, and their dependencies — that initial
> compile can take several minutes and is not counted against the 15.

### Prerequisites

> ✅ Rust 1.95.0 or a compatible newer toolchain matching your host build.<br>
> ✅ Familiarity with async Rust and the `tokio` ecosystem.  
> ✅ Matching Core/runtime/SDK sources or releases providing the APIs in this guide.

Your source lives in its own crate. The path dependencies below assume
`my-source/` is beside the host's matching `drasi-core/` checkout. Published
dependencies are also possible when they provide this lifecycle/UUID API and
the required ordinary ABI; an unrelated older release is not interchangeable.

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
# Use the Core checkout that matches the embedding application.
drasi-lib = { path = "../drasi-core/lib" }
drasi-core = { path = "../drasi-core/core" }
anyhow = "1"
async-trait = "0.1"
chrono = "0.4"
log = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time"] }

# Required the moment you touch `bytes::Bytes` — i.e. as soon as you emit a
# `source_position` or use WAL replay (§8). Harmless to include now.
bytes = "1"
```

> 🔵 **MAY** — A source that opens its own inbound port (see [§8](#sources-that-open-an-inbound-port-ingress))
> also needs tokio's `"net"` feature (and usually `"io-util"` for buffered reads).

> 🔴 **MUST** — Select compatible `drasi-lib`, `drasi-core`, and SDK builds.
> Dynamic plugins must satisfy the host's ordinary ABI/version and target checks;
> crate package versions are not the ABI version.

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

The implementation has four parts, in order: (1) the struct holding `SourceBase`
plus your typed config, (2) a builder that validates config, (3) the per-event
mapping into `SourceChange`, and (4) the `Source` trait impl whose `start()` spawns
a shutdown-aware task that dispatches through `SourceBase`. Read it once top to
bottom; each part is covered in depth in [§5](#5-the-source-trait)–[§8](#8-resilience--persistence-patterns).

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
    pub(crate) base: SourceBase,
    config: MySourceConfig,
}

impl MySource {
    /// `id` is the UUID supplied by the framework, not the source's graph name.
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
            SourceChange::update(element)
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

    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
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

        drasi_lib::context::workers::spawn_owned_worker(&self.base.task_handle, async move {
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
        }).await?;
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
5. **Binding is not initialization.** The required binding hook always wires
   `SourceBase`. This example needs no additional instance-lifetime setup, so it
   uses the default no-op `initialize()` and `dispose()`. Its worker belongs to
   each start/stop cycle. It does not advertise temporary-pause or live-update
   support merely by inheriting no-op hooks.

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
use std::sync::Arc;
use drasi_lib::{DrasiLib, Query};
use drasi_lib::computation::v1::ComponentFactory;
use my_source::{MySource, MySourceConfig};

async fn run(query_factory: Arc<dyn ComponentFactory>) -> anyhow::Result<DrasiLib> {
    let query = Query::cypher("counter-query")
        .query("MATCH (c:Counter) RETURN c.value AS value")
        .from_input("my-source")
        .enable_bootstrap(false)
        .build()?;

    let drasi = DrasiLib::builder()
        .with_query_factory(query_factory)
        .with_source_factory("my-source", true, |id| {
            MySource::new(id.to_string(), MySourceConfig::default())
        })
        .with_query(query)
        .build()
        .await?;

    drasi.start().await?;
    Ok(drasi)
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

Separate the lifetime of an owned source object from its repeatable activations:

```
construct → bind_runtime_context(context) → initialize()
              start() → stop() → start() → stop()
              [delete_owned_resources(), only when explicitly requested]
              dispose() → final object release
```

`SourceBase::new()` creates the local status handle.
`bind_runtime_context(context)` is a **required async method returning `()`**.
It wires that handle and the framework services independently of optional
developer initialization. Sources using `SourceBase` MUST delegate to
`self.base.bind_runtime_context(context).await`, even when `initialize()` is not
overridden. Subsequent `set_status(...)` calls update local status and notify
the runtime.

Typical Source lifecycle:

1. **Construct** — validate typed config and create `SourceBase`.
2. **Bind context** — perform mandatory framework wiring.
3. **Initialize** — call the zero-argument, fallible `initialize()` once for
   instance-lifetime setup. Its default is `Ok(())`.
4. **Start / subscribe / stream** — acquire activation-scoped resources,
   attach subscriptions, and publish source events.
5. **Stop** — signal and join workers, then release activation-scoped resources
   and stale channel-mode dispatchers. Keep the initialized object and retained
   state for another start.
6. **Delete owned resources, if requested** — perform explicitly authorized
   destructive cleanup after a successful stop.
7. **Dispose** — await remaining instance-lifetime cleanup before final release,
   including when the object was initialized but never started.

`initialize`, `start`, `stop`, `dispose`, `pause_processing`, and
`resume_processing` are no-argument async `Result<()>` hooks with no-op defaults.
Sources with their own resources or ingestion work MUST override the hooks
needed to manage that work. No-op defaults do not certify drainage, safe pausing,
state retention, or permanent deletion.

**Once means once per actual constructed object**, not once per UUID, library,
port, or subscriber. Restarting a stopped object does not repeat initialization
or disposal. A newly constructed object with the same UUID has its own lifetime.
Borrowed objects remain their owner's responsibility; removing one subscription
does not dispose a shared source.

Failed initialization MUST prevent start and lead to disposal of that partially
initialized object, not another initialization attempt on it. A failed start may
already own work and therefore still requires stop. Failed or interrupted
stop/dispose/deletion MUST retain the actual cleanup owner and unfinished work
for inspection/retry; they MUST NOT permit conflicting restart or final release.
A successful transition is not repeated; incomplete cleanup must be retry-safe.

Use `pause_processing()` / `resume_processing()` only for a temporary processing
boundary that retains the activation. Do not insert them around normal
start/stop, or resume a paused source merely to stop it. Independently running
sources must implement a real pause before relying on one.

#### Who calls what

| Method | Called by | Implementer responsibility |
|--------|-----------|----------------------------|
| `new(...)` / builder | Application or plugin descriptor | Validate config and create `SourceBase`. |
| `bind_runtime_context(context)` | `DrasiLib` | Always wire `SourceBase`, independently of custom setup. |
| `initialize()` | `DrasiLib` | Optional fallible instance setup, once before the first start. |
| `start()` | `DrasiLib` graph lifecycle | Start ingestion and set status for each activation. |
| `subscribe(settings)` | Query manager | Return streaming receiver and optional bootstrap receiver. |
| `on_subscriptions_complete()` | Lifecycle after startup subscriptions | Release startup fences if the source uses them. |
| `stop()` | `DrasiLib` graph lifecycle | Join activation work, including after a partial start; retain restartable state. |
| `pause_processing()` / `resume_processing()` | An explicitly coordinated processing pause | Pause/resume existing work without replacing the activation. |
| `delete_owned_resources()` | Explicit permanent removal with cleanup enabled | Delete only owned persistent state/external artifacts; the default is unsupported. |
| `dispose()` | Final retirement | Await remaining object-lifetime cleanup; do not delete persistent data implicitly. |

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
    pub resume_sequence: Option<u64>,
    pub request_position_handle: bool,
}
```

The settings are important:

- `nodes` and `relations` let a Source or Bootstrap Provider filter to labels
  the query actually needs.
- `resume_from` contains opaque source-position bytes from a prior checkpoint.
  Only the Source knows how to interpret them.
- `resume_sequence` carries the last source-local sequence the query
  checkpointed, so `SourceBase` keeps framework sequence numbers monotonic
  after restart (it raises its counter to `resume_sequence + 1`).
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
    SourceBaseParams::new(source_id) // UUID supplied to your constructor
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
- `BootstrapResult` carries handoff metadata — its `source_position` marks the
  snapshot's boundary in the streaming source's CDC stream — so the query can
  persist a safe starting checkpoint and live streaming resumes exactly there.
- Any Source can be paired with any Bootstrap Provider. A provider reports a
  `source_position` only when its snapshot boundary is a position in the same
  stream the Source replays from (e.g. a Postgres snapshot taken at an LSN).

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
It is provided during `bind_runtime_context(context)`, before `initialize()`, and includes:

- `source_id`
- component graph update channel for status events
- optional state store provider
- optional WAL provider
- optional identity provider
- runtime instance metadata used for log routing
- optional host-local `log_binding`, containing the actual log destination/key;
  this Rust value is not serialized across plugin FFI

Use the context through `SourceBase`:

```rust,ignore
#[async_trait]
impl Source for MySource {
    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }
}

// Later:
let state_store = self.base.state_store().await;
let identity_provider = self.base.identity_provider().await;
let context = self.base.context().await;
```

---

## 5. The `Source` Trait

All ordinary Sources implement `drasi_lib::sources::Source`. The lifecycle and
delivery surface is shown below; see the [trait definition](../../lib/src/sources/traits.rs)
for the complete API. Optional Rust update hooks do not imply an ordinary ABI
update capability.

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

    async fn bind_runtime_context(&self, context: SourceRuntimeContext);
    async fn initialize(&self) -> anyhow::Result<()> { Ok(()) }
    async fn start(&self) -> anyhow::Result<()> { Ok(()) }
    async fn stop(&self) -> anyhow::Result<()> { Ok(()) }
    async fn dispose(&self) -> anyhow::Result<()> { Ok(()) }
    async fn pause_processing(&self) -> anyhow::Result<()> { Ok(()) }
    async fn resume_processing(&self) -> anyhow::Result<()> { Ok(()) }
    async fn status(&self) -> ComponentStatus;

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse>;

    fn as_any(&self) -> &dyn std::any::Any;

    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        anyhow::bail!("owned-resource deletion is unsupported")
    }

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

### Public import paths

Every type you need has a stable public path. **Copy these imports rather than
guessing** — the obvious roots (`drasi_lib::SourceEventWrapper`,
`drasi_lib::models::*`) do not exist. Most types live in `drasi_lib`, but the
**event data-contract** types come from `drasi_core`, and the event/bootstrap
channel types live under `drasi_lib::channels`.

| Type(s) | Import |
|---------|--------|
| `SourceChange`, `Element`, `ElementMetadata`, `ElementReference`, `ElementValue`, `ElementPropertyMap` | `use drasi_core::models::{SourceChange, Element, ElementMetadata, ElementReference, ElementValue, ElementPropertyMap};` |
| `Source`, `SourceBase`, `SourceBaseParams`, `SourceError` | `use drasi_lib::{Source, SourceBase, SourceBaseParams, SourceError};` |
| `SourceRuntimeContext` | `use drasi_lib::SourceRuntimeContext;` |
| `DispatchMode`, `ComponentStatus`, `SubscriptionResponse` | `use drasi_lib::{DispatchMode, ComponentStatus, SubscriptionResponse};` |
| `SourceSubscriptionSettings` | `use drasi_lib::SourceSubscriptionSettings;` (also at `drasi_lib::config::SourceSubscriptionSettings`) |
| `PositionComparator`, `ByteLexPositionComparator` | `use drasi_lib::{PositionComparator, ByteLexPositionComparator};` |
| `SourceEvent`, `SourceEventWrapper` | `use drasi_lib::channels::events::{SourceEvent, SourceEventWrapper};` |
| `StateStoreProvider`, `StateStoreResult`, `StateStoreError` | `use drasi_lib::{StateStoreProvider, StateStoreResult, StateStoreError};` |
| `WalProvider`, `WriteAheadLogConfig`, `WalError`, `CapacityPolicy` | `use drasi_lib::{WalProvider, WriteAheadLogConfig, WalError, CapacityPolicy};` |
| `BootstrapProvider` | `use drasi_lib::BootstrapProvider;` |
| `BootstrapContext`, `BootstrapRequest`, `BootstrapResult` | `use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest, BootstrapResult};` |
| `BootstrapEvent`, `BootstrapEventSender` | `use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};` |
| `SourcePluginDescriptor`, `BootstrapPluginDescriptor`, `ConfigValue`, `DtoMapper`, `export_plugin!` | `use drasi_plugin_sdk::prelude::*;` |

> 🔴 **MUST** — Add `bytes = "1"` to `Cargo.toml` before you use any of the
> position/replay/WAL APIs: their signatures use `bytes::Bytes`.

### 5.1 Required methods

#### `id` — **MUST**

Return the immutable component UUID supplied to the constructor, unchanged.
The editable graph name is separate and MUST NOT be substituted here. This ID is used in
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

#### `start` — **MUST override when the source owns ingestion work**

Begin ingestion. This usually means:

1. validate runtime dependencies from `SourceRuntimeContext`
2. connect to the external system
3. register WAL or upstream cursors if needed
4. install shutdown signals and register owned worker slots before spawning
5. finish protocol-specific readiness, including binding any listener
6. publish `ComponentStatus::Running` without overwriting a worker failure

Do not block forever inside `start()`. Spawn long-running work and return after
startup succeeds.

#### Worker execution context

Long-lived workers SHOULD use
`drasi_lib::context::workers::spawn_owned_component_worker` (or
`spawn_owned_component_blocking_worker`): these preserve the component binding
without making the start operation every future event's parent. Short snapshot
workers use the corresponding `spawn_owned_scoped_worker` /
`spawn_owned_scoped_blocking_worker` helpers to preserve the initiating context.
Their ownership and cleanup requirements are unchanged.

Worker queues MUST carry each available `ExecutionContext` separately from
event data and restore it per unit using the receiving `ExecutionScope`.
`SourceBase` dispatch and ordinary dispatcher `recv_message` paths preserve
these transient sidecars; payload-only `recv` intentionally does not. Replay
from persisted data has no historical trace context. Do not put tracing
metadata in source positions, WAL records, payload properties or event IDs.
An empty event context MUST clear a previous event's parent, not reuse it.

Sources accepting upstream trace metadata SHOULD use the shared
`drasi_lib::observability::ExecutionContext::from_w3c_headers` helper, or
`ExecutionScope::with_w3c_headers`, with transport `traceparent` and optional
`tracestate` bytes. The SDK preserves trace/parent IDs and sampling; missing
metadata clears prior context, and malformed metadata warns and leaves the
event context-free without rejecting business data. HTTP/gRPC event ingress,
OTLP request metadata, Kafka record headers and Platform CloudEvents extensions
use this shared extraction. A stream's metadata applies to every event in that
stream. OTLP payload span/log IDs remain graph data, not ingestion parents.
Other formats require explicit source mapping, not
guessing a `trace_id` property in business data.

Scope futures per poll and blocking closures with `in_scope`; never hold an
entered tracing/context guard across an await. Optional fetch/decode detail
belongs around real bounded work, not an idle listener lifetime. Legacy
Source cdylibs without portable context support remain a documented boundary;
this does not change their ABI or log routing.

#### `stop` — **MUST override when the source owns activation resources**

Stop ingestion and clean up activation resources, not the whole object's
lifetime or persistent data. Use `SourceBase::stop_common()` if
your source stores a `oneshot::Sender<()>` and task handle in `SourceBase`.
Sources with custom task slots MUST join those workers before calling
`stop_common()`, which also clears channel dispatchers and subscriber position
filters. Custom lifecycle calls must be serialized.
Use `context::workers::spawn_owned_worker` to acquire a vacant task slot before
spawning, rather than spawning and then awaiting `set_task_handle`. A finished
but unjoined worker still owns its slot. Custom cleanup should use
`join_owned_worker` or `cancel_owned_worker` without taking the handle out first.
Cancellation preserves the handle; timeout requests abort and returns a typed
error with joining still required. Do not report `Stopped` or start replacement
work until cleanup succeeds.

Use `join_owned_worker_gracefully` instead when aborting would abandon an
in-flight request/state operation or destroy a child-drain boundary. Its timeout
retains the worker without aborting it. Keep an explicit cleanup-required guard
until all base/resource cleanup finishes: cancellation can happen after the
worker handle has been joined but before dispatchers are cleared. Cloudflare
Radar, Open511 and HERE Traffic demonstrate this polling pattern. Restart must
also reset persistent shutdown signals and create fresh receivers.

Register blocking work with `spawn_owned_blocking_worker`, rather than moving an
untracked thread handle into a stop-time join task. SQLite keeps database, HTTP
and dispatcher owners separately: it drains admitted requests before closing the
database, then joins queued event dispatch before base cleanup. Its transaction
and HTTP handles stay bound to the accepting worker so late continuations cannot
operate on a replacement connection.

Cleanup MUST also handle `Starting` and `Error`; a failed task can still leave
subscriptions and other resources alive. A successful stop MUST report `Stopped`.
Managed lifecycle operations reject a source that returns success without stopping.

PostgreSQL uses `SourceBase::abort_and_join_task()` for immediate cancellation
with ownership retained through joining. MySQL signals its replication worker
and joins it gracefully. Unexpected task failures are returned after completed
plugin-specific cleanup and leave the source in `Error`. Timed-out cleanup
retains the worker and blocks replacement. See each plugin's lifecycle
documentation for its policy.

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

#### `bind_runtime_context(context)` — **MUST**

Delegate to `SourceBase`:

```rust,ignore
async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
    self.base.bind_runtime_context(context).await;
}
```

#### `initialize()` / `dispose()` — optional instance-lifetime hooks

Override `initialize() -> Result<()>` for fallible setup that needs bound
services and must survive start/stop cycles. Do not start normal ingestion or
wait for the graph's live bootstrap here. Override `dispose() -> Result<()>`
to release those resources at final retirement, including partial initialization.
Both default to `Ok(())`; mandatory base wiring has already run, and neither
implicitly performs destructive cleanup.

### 5.2 Lifecycle — the runtime call sequence

Fresh start:

```
new / descriptor create_source
    ↓
bind_runtime_context(context)
    ↓
initialize()
    ↓
start()
    ↓
subscribe(query A, enable_bootstrap = true, resume_from = None)
    ↓
subscribe(query B, enable_bootstrap = true, resume_from = None)
    ↓
on_subscriptions_complete()
    ↓
dispatch live SourceEventWrapper values with required sequence numbers
```

Restart the same initialized object with a query checkpoint:

```
successful stop()
    ↓
start()
    ↓
subscribe(query, resume_from = Some(source_position), resume_sequence = Some(seq))
    ↓
source validates/rewinds/replays
    ↓
bootstrap is skipped
    ↓
live stream resumes
```

This restart does not call `initialize()` or `dispose()`. Reconstruction after
process restart or an explicitly approved replacement creates a new object,
which must bind context and initialize independently.

Removal with cleanup:

```
stop()
    ↓
delete_owned_resources()
    ↓
dispose()
    ↓
final object release
```

Removal without destructive cleanup skips `delete_owned_resources`, not
`dispose`. Any cleanup failure keeps the owner available for retry rather than
claiming removal completed.

**Managed updates require explicit author support.** The ordinary ABI 0.18
descriptor/FFI path is creation-only for managed updates: it exports no factory
`plan_update` or component `apply_update` bridge. A creation descriptor, a
restartable object, or no-op lifecycle hooks do not authorize live replacement,
state reset, or a pause-based update. In-process author-supplied planning must
implement the actual Core contract; an opaque same-UUID replacement callback is
not a substitute. See the [plugin architecture](../plugin-architecture.md).

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

Return `false` if the Source should only start on an explicit runtime request.

#### `supports_replay` — **MUST for volatile sources**

The trait default is `true`. Sources that cannot honor `resume_from` MUST
override this and return `false`.

Return `true` only when the Source can replay from its persisted position, either
through an upstream durable log or through Drasi WAL durability.

#### `describe_schema` — **MAY**

Return a best-effort `SourceSchema` describing node and relation labels.
Database-like sources can derive this from configured tables or introspection;
webhook-style sources can derive it from route mappings.

#### `delete_owned_resources` — **MAY**

The default returns an unsupported error, not success. Override it for
explicitly requested deletion of owned durable state or external resources:

- delete Drasi WAL data
- clear state-store partitions
- drop replication slots or cursors
- unregister consumers or leases

Use `SourceBase::delete_owned_resources_common()` for the source's state-store
partition. It does not delete WAL partitions or external artifacts for you.
Propagate failures and make retries safe; stop/dispose MUST NOT call this
destructive hook implicitly.

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
you use `SourceBase`. For ordinary FFI sources, set it before context binding:
the programmatic provider takes precedence over the context provider, but there
is no late provider-replacement callback.

### 5.4 Error handling

Fallible lifecycle/data methods return `anyhow::Result`; mandatory context
binding returns `()`. Add
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
| Runtime context | `bind_runtime_context`, `context`, `state_store`, `identity_provider`, `set_identity_provider` |
| Status and lifecycle | `get_status`, `set_status`, `status_handle`, `set_task_handle`, `set_shutdown_tx`, `stop_common`, `abort_and_join_task`, `clear_dispatchers` |
| Explicit destructive cleanup | `delete_owned_resources_common` |
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
let params = SourceBaseParams::new(source_id) // framework-supplied UUID
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
profiling metadata. Build a `SourceEventWrapper` with a required sequence,
using your own monotonic ordinal or `SourceBase::next_sequence()`:

```rust,ignore
use drasi_lib::channels::events::{SourceEvent, SourceEventWrapper};

let mut wrapper = SourceEventWrapper::new(
    self.base.id.clone(),
    SourceEvent::Change(change),
    chrono::Utc::now(),
    self.base.next_sequence(),
);
wrapper.set_source_position(bytes::Bytes::from(lsn.to_be_bytes().to_vec()));
self.base.dispatch_event(wrapper).await?;
```

Use `dispatch_events_batch()` when a poll cycle or database read produces many
events and all can be dispatched together.

Serialize allocation and dispatch together when multiple tasks produce events
for one source, including batch construction. `next_sequence()` allocates a
unique number but does not reserve dispatch order. `dispatch_source_change()`
and `broadcast_control()` handle allocation and dispatch under one ordering lock.

### Position helpers

Replay-capable sources MUST register a position comparator. `set_position_comparator`
is `async` and takes the comparator **by value** (an `impl PositionComparator`, not a
`Box<dyn …>`), so call it from `start()` or another async context — never from the
synchronous builder:

```rust,ignore
use drasi_lib::ByteLexPositionComparator;

// In start(), before spawning the streaming task:
self.base.set_position_comparator(ByteLexPositionComparator).await;
```

`ByteLexPositionComparator` orders any position encoded as big-endian bytes
(u64 offsets, Postgres LSNs, MSSQL LSNs). Supply your own `impl PositionComparator`
only if your positions are not big-endian comparable. The remaining helpers:

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

Source authors and downstream consumers use the same envelope. Its `sequence`
field and both constructors (`new` and `with_profiling`) require a `u64`:

```rust,ignore
pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: DateTime<Utc>,
    pub profiling: Option<ProfilingMetadata>,
    pub sequence: u64,
    pub source_position: Option<Bytes>,
}
```

The source supplies `sequence`, either from its own source-local monotonic
ordinal (such as a WAL append index) or from `SourceBase::next_sequence()`.
Dispatch preserves this value and advances the fallback allocator past it.
Sequences start at one and live events must be dispatched in increasing order.
Native positions such as LSNs or partition offsets are not necessarily unique
per event; keep them in `source_position` unless they also satisfy the sequence
contract. Resume floors and WAL-head restoration advance the fallback counter
but cannot correct a source's own sequence generator.

Set `source_position` before dispatch when the
event has a replayable upstream position. `source_position` is opaque to the
framework and must be no larger than 64 KB for checkpoint persistence.

> **Source-author checklist** — the fastest way is the right way:
> 1. Obtain a source-local monotonic sequence or call `base.next_sequence()`.
>    Pass it to `SourceEventWrapper::new` or `with_profiling`.
> 2. If the event has a replayable upstream cursor, attach it with
>    `wrapper.set_source_position(bytes)`.
> 3. Call `base.dispatch_event(wrapper)` or `dispatch_events_batch(wrappers)`.
>    Serialize steps 1 through 3 across concurrent producers.
> 4. On replay, preserve durable sequence values. For native-position sources
>    using the fallback allocator, allocate above the restored resume floor.

### Why there is no source templating

Reactions format query results for arbitrary downstream systems, so they support
output templating. Sources do the opposite: they normalize external data into the
Drasi graph model. The final emitted shape is always `SourceChange`.

If your source accepts arbitrary external payloads, implement or reuse a mapping
layer before creating `SourceChange`; do not add a templating system after
`SourceChange`.

---

## 8. Resilience & Persistence Patterns

WAL-backed sources can delegate subscription selection to
`SourceBase::subscribe_with_wal`. It handles both native cursor bytes and
`resume_sequence` (including zero before the first event checkpoint), preserves
configured fresh bootstrap behavior, and exposes unavailable WAL history as
`SourceError::PositionUnavailable` for the query's explicit recovery policy.
HTTP, gRPC and Application sources share this implementation.

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

The two halves of a native-replay source — stamping positions on the way out and
validating `resume_from` on the way in — look like this. Every call below is
compile-tested against `drasi-lib` 0.8 / `drasi-core` 0.5:

```rust,ignore
use drasi_lib::channels::events::{SourceEvent, SourceEventWrapper};
use drasi_lib::{ByteLexPositionComparator, SourceError};

// --- In start(): register the comparator once, then stream. ---
self.base.set_position_comparator(ByteLexPositionComparator).await;

// `clone_shared()` yields a task-owned handle that dispatches through the
// same SourceBase. Per event, stamp the upstream position (big-endian bytes):
let mut wrapper = SourceEventWrapper::new(
    source_id.clone(),
    SourceEvent::Change(change),
    chrono::Utc::now(),
    base.next_sequence(),
);
wrapper.set_source_position(bytes::Bytes::from(offset.to_be_bytes().to_vec()));
base.dispatch_event(wrapper).await?;

// Periodically: everything up to the confirmed position is safe to acknowledge
// upstream (commit the offset / flush the LSN) and prune. NEVER advance past the
// minimum confirmed subscriber position.
if let Some(confirmed_seq) = base.compute_confirmed_position().await {
    if let Some(confirmed_pos) = base.compute_confirmed_source_position().await {
        // send_upstream_feedback(confirmed_pos).await;  // commit / flush
    }
    base.prune_position_map(confirmed_seq).await;
}

// --- In subscribe(): reject a checkpoint older than what the upstream retains. ---
async fn subscribe(&self, settings: SourceSubscriptionSettings) -> Result<SubscriptionResponse> {
    if let Some(resume_from) = settings.resume_from.as_ref() {
        if decode_offset(resume_from) < self.earliest_retained {
            return Err(SourceError::PositionUnavailable {
                source_id: self.base.get_id().to_string(),
                requested: resume_from.clone(),
                earliest_available: Some(encode_offset(self.earliest_retained)),
            }
            .into());
        }
    }
    self.base.subscribe_with_bootstrap(&settings, "my-native-source").await
}
```

Notes that the compiler will otherwise teach you the hard way:

- `set_position_comparator` is `async` and takes the comparator **by value**.
- `SourceEvent` / `SourceEventWrapper` import from `drasi_lib::channels::events`.
- `compute_confirmed_position()` returns the confirmed framework **sequence**
  (`u64`); `compute_confirmed_source_position()` returns the corresponding opaque
  **position bytes**. Use the sequence to `prune_position_map`, the bytes to
  acknowledge your upstream.

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

The mechanism has three parts. Every call below is compile-tested.

**1. Register the WAL in `start()`** and resume the sequence counter from the
persisted head:

```rust,ignore
use drasi_lib::{WalProvider, WriteAheadLogConfig};

if self.config.durable {
    let ctx = self.base.context().await
        .ok_or_else(|| anyhow::anyhow!("runtime context must be bound before start()"))?;
    let wal = ctx.wal_provider
        .ok_or_else(|| anyhow::anyhow!("durable source requires a WAL provider on DrasiLib"))?;
    wal.register(self.base.get_id(), WriteAheadLogConfig::default()).await?;

    let head = wal.head_sequence(self.base.get_id()).await?;
    self.base.begin_wal_subscriptions(head);
    self.base.set_next_sequence(head);     // resume monotonic sequencing
    *self.wal.write().await = Some(wal);   // keep a handle for ingest + replay
}
```

**2. Append before you acknowledge** the producer, so a crash after the ACK can
still replay the event:

```rust,ignore
// In your ingest path, for each incoming change:
wal.append(self.base.get_id(), &change).await?;   // durable
self.base.dispatch_source_change(change).await?;  // live
```

**3. Replay on resubscribe.** When a query reconnects with a checkpoint, replay
the retained WAL by Drasi sequence before live events flow:

```rust,ignore
async fn subscribe(&self, settings: SourceSubscriptionSettings) -> Result<SubscriptionResponse> {
    if self.config.durable {
        let wal_guard = self.wal.read().await;
        if let (Some(wal), Some(resume_from)) =
            (wal_guard.as_ref(), settings.resume_from.as_ref())
        {
            if resume_from.len() >= 8 {
                let resume_seq = u64::from_be_bytes(resume_from[..8].try_into().unwrap());
                let wal = wal.clone();
                drop(wal_guard);
                return self.base
                    .subscribe_with_replay(&settings, wal.as_ref(), resume_seq, "my-source")
                    .await;
            }
        }
    }
    self.base.subscribe_with_bootstrap(&settings, "my-source").await
}
```

`subscribe_with_replay` reads `wal.read_from(resume_seq + 1)` up to the current
head and delivers those events ahead of the live stream, so the resuming query
sees no gap. Store the WAL handle in a field such as
`Arc<RwLock<Option<Arc<dyn WalProvider>>>>`.

Rules:

- Append to the WAL **before** acknowledging the external producer.
- `WriteAheadLogConfig::default()` is a fine start; size `max_events` and choose a
  capacity policy for your throughput.
- On restart, `head_sequence()` + `set_next_sequence(head)` resumes sequencing.
- Read the committed WAL head without replacing errors with zero. Call
  `SourceBase::begin_wal_subscriptions(head)` on every start and use
  `prune_confirmed_wal()` for automatic cleanup. For a retained log, cleanup
  must wait for the initial subscription batch: implement
  `on_subscriptions_complete()` by calling `complete_wal_subscriptions()`.
  A timer or the first fast subscriber cannot release that fence. Hosts using
  the source directly must issue the completion callback themselves.
  Stop the pruning worker cooperatively and await any submitted storage operation;
  do not abort it while replay registration is excluded by the pruning lock.
- Use `subscribe_with_replay()` when `resume_from` is present; fall back to
  `subscribe_with_bootstrap()` otherwise.
- Return `true` from `supports_replay()` only when durability is enabled.
- Delete owned WAL data only in explicitly requested `delete_owned_resources()`
  via `wal.delete_wal(id)`, never as part of stop or disposal.

> 🟢 A complete, compiling durable ingress source that uses this exact pattern is
> in [Worked example: a durable ingress source](#worked-example-a-durable-ingress-source)
> at the end of this section.

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
An empty retained log with a positive committed head can also represent a gap:
it is not an empty replay when the requested position is behind that head.

### Replay safety rules at a glance

> 🔴 **MUST** — Override `supports_replay()` to return `false` for volatile sources.
> 🔴 **MUST** — Return `SourceError::PositionUnavailable` when `resume_from` predates what you retain; never silently restart from "now".
> 🔴 **MUST** — Acknowledge/commit/prune your upstream only up to the **minimum confirmed** subscriber position (`compute_confirmed_position()`), never past it.
> 🔴 **MUST** — Emit millisecond `effective_from`; emit a `source_position` only when it is genuinely replayable.
> 🟡 **SHOULD** — Call `wait_for_subscribers()` before producing live events on a restart cycle.
> 🟡 **SHOULD** — Make downstream effects idempotent; replay and at-least-once delivery can re-emit events.

### Checkpoint and upstream-feedback timing

Each subscribed query reports its last durably-processed framework sequence
through a position handle. `compute_confirmed_position()` returns the **minimum**
across all live subscribers; `compute_confirmed_source_position()` maps that to the
matching opaque position bytes. The discipline:

- **Advance feedback only to the confirmed position.** Committing an offset,
  flushing an LSN, ACKing a message, or pruning the WAL past the minimum confirmed
  subscriber loses data for the slowest query. This is the single most common
  source bug.
- **Encode "next", not "last", when your upstream resume is inclusive.** If
  resuming from a stored position re-delivers that position, store the *next*
  position to read to avoid an off-by-one on every restart.
- **Compare per partition for multi-partition upstreams.** A single source can
  fan in many partitions/streams. A `PositionComparator` that only compares the
  event's own partition lets independent partitions advance without suppressing
  each other.
- **State your delivery guarantee.** Append-before-ACK plus replay gives
  *at-least-once*: a resuming query can see an event twice. Document that, and
  prefer stable element IDs so downstream upserts are idempotent.

### Supervising long-running tasks

A source's real work runs in a spawned task. Make its failure modes explicit:

- **Separate retryable from fatal.** Connection blips, timeouts, and rate limits
  are retryable: sleep with bounded exponential backoff and continue — but make
  the sleep shutdown-aware so `stop()` stays prompt:

  ```rust,ignore
  tokio::select! {
      biased;
      _ = &mut shutdown_rx => break,
      _ = tokio::time::sleep(backoff) => { /* retry */ }
  }
  ```

  Auth failures, malformed config, and unrecoverable upstream errors are fatal:
  set `ComponentStatus::Error` with a message and stop.
- **Propagate terminal state.** If the task ends or panics, its terminal status
  MUST be reflected (`Error`/`Stopped`). Never leave a dead task reporting
  `Running`.
- **Drain on shutdown.** Persist any cursor (see the state store below) before the
  task returns, so a restart resumes cleanly.

### Sources that open an inbound port (ingress)

A source that has no upstream to poll — a webhook, an RPC endpoint, a TCP feed —
must open and own its own listener. The protocol is yours; the Drasi-side pattern
is always the same:

1. (Optional) register a WAL in `start()` for durability (Pattern B above).
2. **Bind the port, and report `Running` only after the bind succeeds.** On bind
   failure, set `ComponentStatus::Error` and return `Err` so the failure surfaces
   at startup instead of as a silently dead source.
3. Spawn the accept loop with `self.base.clone_shared()` using
   `context::workers::spawn_owned_worker(&self.base.task_handle, future)`;
   store the shutdown sender with `set_shutdown_tx` so `stop_common()` joins it.
4. For each accepted request: authenticate/validate (your concern), map the
   payload to a `SourceChange` (your concern), then — Drasi's concern —
   `wal.append(...)` if durable and `base.dispatch_source_change(change)`.

Ingress sources need tokio's `"net"` feature (and usually `"io-util"`). Health,
readiness, CORS, and request auth are properties of *your* endpoint — handle them
before dispatching.

### Using the state store to survive restarts

The **WAL** is for the event stream. For everything else a source must remember
across a restart — a poll cursor, an ETag, a set of seen IDs, a "last baseline"
timestamp — use the **state store**: a small per-source key/value store.

`drasi-lib` injects it through the runtime context; reach it from `SourceBase`:

```rust,ignore
use drasi_lib::StateStoreProvider;

// Some(...) only if a state store was configured on DrasiLib.
let store: Option<Arc<dyn StateStoreProvider>> = self.base.state_store().await;
```

The trait is a partitioned key/value map — `store_id` is the partition (use your
source ID), values are bytes (serialize with `serde_json`):

```rust,ignore
async fn load_counter(base: &SourceBase, key: &str) -> u64 {
    if let Some(store) = base.state_store().await {
        if let Ok(Some(bytes)) = store.get(base.get_id(), key).await {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                return text.parse().unwrap_or(0);
            }
        }
    }
    0
}

async fn save_counter(base: &SourceBase, key: &str, value: u64) {
    if let Some(store) = base.state_store().await {
        let _ = store
            .set(base.get_id(), key, value.to_string().into_bytes())
            .await;
    }
}
```

`get`/`set`/`delete`/`contains_key`/`get_many`/`set_many` are all `async` and
return `StateStoreResult`. **Load** your cursor when the streaming task starts;
**save** it after dispatch (batched is fine) and again on graceful shutdown. Keep
it small — it is for resumption metadata, not for the data itself.

### Worked example: a durable ingress source

This source sketch ties the section together: it **opens its own inbound TCP
port**, writes every event to the **WAL** for durability and replay, and keeps a
restart-surviving cursor in the **state store**. The protocol is deliberately
trivial (one newline-delimited payload per event) so the Drasi wiring stays
visible — replace `payload_to_change` and the read loop with your own.

`Cargo.toml` needs `bytes = "1"` and tokio's `"net"` + `"io-util"` features.

```rust,ignore
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::{
    ComponentStatus, DispatchMode, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
    SourceSubscriptionSettings, SubscriptionResponse, WalProvider, WriteAheadLogConfig,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FeedConfig {
    pub bind_addr: String, // e.g. "127.0.0.1:0"
    pub label: String,     // graph label for every ingested node
    #[serde(default)]
    pub durable: bool,     // WAL-back every event for crash recovery + replay
}

pub struct FeedSource {
    pub(crate) base: SourceBase,
    config: FeedConfig,
    wal: Arc<RwLock<Option<Arc<dyn WalProvider>>>>,
}

impl FeedSource {
    pub fn new(id: impl Into<String>, config: FeedConfig) -> Result<Self> {
        if config.bind_addr.trim().is_empty() {
            anyhow::bail!("bind_addr must not be empty");
        }
        if config.label.trim().is_empty() {
            anyhow::bail!("label must not be empty");
        }
        let params = SourceBaseParams::new(id)
            .with_dispatch_mode(DispatchMode::Channel)
            .with_auto_start(true);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            wal: Arc::new(RwLock::new(None)),
        })
    }

    pub fn base_mut(&mut self) -> &mut SourceBase {
        &mut self.base
    }
}

// One inbound payload -> one SourceChange. Replace with your protocol + mapping.
fn payload_to_change(source_id: &str, label: &str, payload: &str) -> SourceChange {
    let mut properties = ElementPropertyMap::new();
    properties.insert("payload", ElementValue::String(payload.into()));
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, payload),
            labels: vec![Arc::<str>::from(label)].into(),
            effective_from: Utc::now().timestamp_millis() as u64,
        },
        properties,
    };
    SourceChange::Insert { element }
}

async fn load_counter(base: &SourceBase, key: &str) -> u64 {
    if let Some(store) = base.state_store().await {
        if let Ok(Some(bytes)) = store.get(base.get_id(), key).await {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                return text.parse().unwrap_or(0);
            }
        }
    }
    0
}

async fn save_counter(base: &SourceBase, key: &str, value: u64) {
    if let Some(store) = base.state_store().await {
        let _ = store
            .set(base.get_id(), key, value.to_string().into_bytes())
            .await;
    }
}

async fn handle_connection(
    base: &SourceBase,
    wal: Option<&Arc<dyn WalProvider>>,
    source_id: &str,
    label: &str,
    stream: TcpStream,
    ingested: &mut u64,
) -> Result<()> {
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        let payload = line.trim();
        if payload.is_empty() {
            continue;
        }
        let change = payload_to_change(source_id, label, payload);

        // Durability: append BEFORE acknowledging, so a crash after the ACK
        // can still replay the event.
        if let Some(wal) = wal {
            wal.append(source_id, &change)
                .await
                .map_err(|e| anyhow!("WAL append failed: {e}"))?;
        }
        base.dispatch_source_change(change).await?;

        *ingested += 1;
        if *ingested % 100 == 0 {
            save_counter(base, "events_ingested", *ingested).await;
        }
    }
    Ok(())
}

#[async_trait]
impl Source for FeedSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }
    fn type_name(&self) -> &str {
        "feed"
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }
    fn dispatch_mode(&self) -> DispatchMode {
        self.base.get_dispatch_mode()
    }
    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }
    fn supports_replay(&self) -> bool {
        self.config.durable
    }

    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }
        self.base
            .set_status(ComponentStatus::Starting, Some("starting feed".into()))
            .await;

        // 1. Durability (optional): register a WAL partition and resume the
        //    sequence counter from the persisted head.
        if self.config.durable {
            let ctx = self
                .base
                .context()
                .await
                .ok_or_else(|| anyhow!("runtime context must be bound before start()"))?;
            let wal = ctx
                .wal_provider
                .ok_or_else(|| anyhow!("durable feed requires a WAL provider on DrasiLib"))?;
            wal.register(self.base.get_id(), WriteAheadLogConfig::default())
                .await
                .map_err(|e| anyhow!("WAL register failed: {e}"))?;
            let head = wal.head_sequence(self.base.get_id()).await?;
            self.base.begin_wal_subscriptions(head);
            self.base.set_next_sequence(head);
            *self.wal.write().await = Some(wal);
        }

        // 2. Bind the inbound port BEFORE reporting Running.
        let listener = match TcpListener::bind(&self.config.bind_addr).await {
            Ok(listener) => listener,
            Err(e) => {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("failed to bind {}: {e}", self.config.bind_addr)),
                    )
                    .await;
                return Err(anyhow!("failed to bind {}: {e}", self.config.bind_addr));
            }
        };

        // 3. Load the restart-surviving cursor.
        let mut ingested = load_counter(&self.base, "events_ingested").await;

        // 4. Spawn the accept loop on a task-owned SourceBase handle.
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let base = self.base.clone_shared();
        let wal = self.wal.read().await.clone();
        let source_id = self.base.get_id().to_string();
        let label = self.config.label.clone();

        drasi_lib::context::workers::spawn_owned_worker(&self.base.task_handle, async move {
            base.set_status(ComponentStatus::Running, Some("listening".into()))
                .await;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let (stream, _peer) = match accepted {
                            Ok(pair) => pair,
                            Err(_) => continue,
                        };
                        if let Err(e) = handle_connection(
                            &base, wal.as_ref(), &source_id, &label, stream, &mut ingested,
                        ).await {
                            base.set_status(
                                ComponentStatus::Error,
                                Some(format!("ingest failed: {e}")),
                            ).await;
                        }
                    }
                }
            }
            // Drain: persist the final cursor on graceful shutdown.
            save_counter(&base, "events_ingested", ingested).await;
        }).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(ComponentStatus::Stopping, Some("stopping feed".into()))
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
        // A resuming subscriber with a checkpoint replays retained WAL events
        // by Drasi sequence before live events flow.
        if self.config.durable {
            let wal_guard = self.wal.read().await;
            if let (Some(wal), Some(resume_from)) =
                (wal_guard.as_ref(), settings.resume_from.as_ref())
            {
                if resume_from.len() >= 8 {
                    let resume_seq =
                        u64::from_be_bytes(resume_from[..8].try_into().expect("checked len >= 8"));
                    let wal = wal.clone();
                    drop(wal_guard);
                    return self
                        .base
                        .subscribe_with_replay(&settings, wal.as_ref(), resume_seq, "feed")
                        .await;
                }
            }
        }
        self.base.subscribe_with_bootstrap(&settings, "feed").await
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
    /// Number of bootstrap events sent through the channel.
    pub event_count: usize,
    /// Opaque token identifying the snapshot's position in the streaming
    /// source's CDC stream (e.g. a Postgres WAL LSN, Oracle SCN, MySQL binlog
    /// offset). Persisted as the initial recovery checkpoint and used to start
    /// the stream exactly at the snapshot boundary. `None` for providers
    /// without a positional concept.
    pub source_position: Option<Bytes>,
}
```

Use these fields carefully:

- `event_count` is the number of bootstrap events sent.
- `source_position` is the snapshot's boundary in the streaming source's
  position space. When the provider and the streaming Source share a stream
  (e.g. a Postgres snapshot plus Postgres CDC), set it to the snapshot's
  position: the framework persists it as the initial checkpoint, so live
  streaming starts exactly at the boundary — no overlap, no gap. Leave it
  `None` when the provider has no position in the Source's stream (file/fixture,
  remote-query, or cross-system snapshots); the Source then streams from its own
  start point.

### Provider design categories

| Provider category | Use case |
|-------------------|----------|
| Native snapshot provider | Reads an initial snapshot from the same system that provides the live stream. This is the only category that should usually report a `source_position` (the snapshot's boundary in that stream). |
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
arrive through another. Report a `source_position` only when the bootstrap
boundary is a real position in the same stream the Source replays from;
otherwise leave it `None`.

### Implementing a provider for your source

Providers SHOULD:

- honor `request.node_labels` and `request.relation_labels`
- send `BootstrapEvent` values with stable IDs and millisecond timestamps
- return accurate handoff metadata
- fail loudly on malformed data or inaccessible upstream snapshots
- avoid keeping `BootstrapContext` beyond the call

### A complete provider

Imports come from `drasi_lib::bootstrap` (the request/context/result types and the
`BootstrapProvider` trait) and `drasi_lib::channels` (the event + sender). The
provider sends `BootstrapEvent` values through `event_tx` and uses
`context.next_sequence()` for monotonic bootstrap sequence numbers. This compiles
against `drasi-lib` 0.8:

```rust,ignore
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_lib::SourceSubscriptionSettings;

pub struct OrdersBootstrap {
    source_id: String,
    label: String,
    snapshot: Vec<OrderRecord>,
}

#[async_trait]
impl BootstrapProvider for OrdersBootstrap {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        // Honor the labels the query was allocated.
        if !request.node_labels.is_empty()
            && !request.node_labels.iter().any(|l| l == &self.label)
        {
            return Ok(BootstrapResult::default());
        }

        let mut count = 0usize;
        for order in &self.snapshot {
            let change = order_to_change(&self.source_id, &self.label, order);
            event_tx
                .send(BootstrapEvent {
                    source_id: self.source_id.clone(),
                    change,
                    timestamp: Utc::now(),
                    sequence: context.next_sequence(),
                })
                .await?;
            count += 1;
        }

        Ok(BootstrapResult {
            event_count: count,
            // Set this to the snapshot's position only when this provider shares
            // the streaming source's CDC stream (e.g. a Postgres snapshot taken
            // at an LSN); otherwise leave it None.
            source_position: None,
        })
    }
}
```

Wire it into the source before subscriptions begin — either programmatically or,
for dynamic plugins, inside `create_source` / a `BootstrapPluginDescriptor`
([§12](#the-bootstrap-descriptor)):

```rust,ignore
source.set_bootstrap_provider(Box::new(OrdersBootstrap { /* ... */ })).await;
```

`BootstrapContext` also exposes `get_property(key)` / `get_typed_property::<T>(key)`
for any extra properties passed via `subscribe_with_bootstrap_context`. Do not
retain the context after `bootstrap()` returns.

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

### Mapping JSON-like payloads

Convert external types into `ElementValue` deliberately (see [§7](#elementvalue)):
preserve identifiers as strings even when they look numeric, keep timestamps in
one consistent representation, and avoid lossy float coercion for values that must
stay exact.

If your source ingests arbitrary JSON-like payloads, you usually do **not** need
to hand-write this. The published `drasi-source-mapping` crate is a
configuration-driven engine that turns JSON into `Element`/`ElementValue` with
conditional mappings, operation maps (insert/update/delete), node and relation
templates with endpoint validation, and explicit timestamp formats
(seconds/millis/nanos). Prefer it over a bespoke mapper for JSON/Avro payloads;
reach for a custom converter only for binary or strongly-typed wire formats (such
as protobuf `Struct` conversion) that the engine does not cover.

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

Most CDC sources also need **upstream setup and validation** before they can
stream. Document these prerequisites and check them at `start()` with clear
errors instead of cryptic driver failures:

- **Enable the change feed** — e.g. Postgres `wal_level = logical` plus a
  replication slot and publication; MySQL row-format binlog with a server ID;
  SQL Server / Oracle CDC enabled per table.
- **Verify privileges and prerequisites** at startup.
- **Own the slot/publication lifecycle**: create-or-reuse on start; decide whether
  to drop it only in an explicitly requested `delete_owned_resources()`.
- **Introspect schema** (columns, primary key / replica identity) and keep a
  fallback for when introspection has not run yet or a DDL change races a read.
- **Decide a DDL / schema-change policy**: skip, error, or re-introspect.

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

Polling sources rarely have native replay. Use the **state store**
([§8](#using-the-state-store-to-survive-restarts)) to make restarts deterministic:

- **Persist the cursor** (timestamp / ETag / page token) and **load it on start**;
  pick a first-run policy (start-from-now vs full backfill).
- **Detect deletes** by diffing against a persisted snapshot or set of seen IDs —
  but only treat "missing" as a delete when the API guarantees a complete result.
- **Alternate** periodic full sweeps with incremental `updated_since`-style polls
  when the API supports them; persist state **after** dispatch so a crash re-polls
  rather than skips.
- **Survive partial failures**: keep the last good snapshot for failed-feed
  fallback, and continue past transient poll errors with backoff.
- Document plainly that recovery guarantees here are **weaker** than native-log or
  WAL replay.

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

> 🔴 **MUST** — Return every configuration value needed to recreate the Source.
> What that means depends on how the Source was constructed:
>
> | Constructed via | `properties()` returns | What lands on disk |
> |-----------------|------------------------|--------------------|
> | A dynamic-plugin descriptor (the usual path) | the **raw config JSON** you preserved with `set_raw_config` | unresolved `ConfigValue` envelopes — secret **references**, not secret values |
> | Plain values in code (embedded path) | the serialized typed config | whatever literal values you were built with |
>
> Either way, **never filter keys out**: a missing key is lost on the next
> snapshot and the Source fails to restart. `properties()` is a persistence hook,
> not a redacted inspection DTO — protecting the snapshot file at rest is the
> embedding application's job.

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

### Authentication and transport security

`ConfigValue<T>` covers *where a secret comes from*; it does not cover the
*runtime lifecycle* of credentials. Document which modes your source supports and
validate them at `start()`:

| Mode | How |
|------|-----|
| Static secret | A `ConfigValue<String>` resolved in the descriptor (password, API key, token). |
| Connection string | One `ConfigValue<String>`; parse and validate before connecting. |
| Bearer / API token | Send per request; for ingress sources, compare inbound tokens with a constant-time check (e.g. the `subtle` crate). |
| OAuth client credentials | Fetch a token, cache it, refresh before expiry, retry once on 401. |
| Cloud SDK default chain | Let the SDK resolve ambient credentials; expose an endpoint override for local emulators. |
| TLS / mTLS | Accept CA/cert/key paths or PEM material; validate the scheme (e.g. require `amqps://`) before connecting. |
| SASL / broker auth | Expose mechanism + credentials plus an `additional_properties` escape hatch applied *after* generic settings so it cannot be overridden. |
| Runtime identity | Implement `set_identity_provider()` and call `self.base.identity_provider().await` to obtain credentials/certs at startup or per request. |

Two rules cut across all of them:

> 🔴 **MUST** — Never log resolved secrets. Redact secret fields in `Debug` and
> keep them out of status messages.
> 🟡 **SHOULD** — Validate auth/TLS configuration (URLs, schemes, file existence)
> at `start()` with an actionable error, not at first request.

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
drasi-lib = { path = "../drasi-core/lib" }
drasi-core = { path = "../drasi-core/core" }
drasi-plugin-sdk = { path = "../drasi-core/components/plugin-sdk" }
utoipa = { version = "4", features = ["chrono"] }
bytes = "1"
```

Keeping `"lib"` alongside `"cdylib"` lets the crate be used in tests and static
embedding scenarios while still producing a plugin library.

> 🔴 **MUST** — Build the host and plugin with matching ordinary ABI 0.18
> interfaces and the same target/toolchain. `FFI_SDK_VERSION` is independent of
> the SDK's crate release version. See the compatibility rules below.

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
    core_version = "0.5.10",
    lib_version = "0.9.3",
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::MySourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
```

Set `core_version` and `lib_version` to the exact `drasi-core` and `drasi-lib`
versions used to build the plugin.

If the crate also provides a bootstrap provider, implement
`BootstrapPluginDescriptor` and list it in `bootstrap_descriptors`.

### The bootstrap descriptor

`create_bootstrap_provider` receives **two** JSON values: the bootstrap plugin's
own config and the parent source's config (use the latter for shared connection
details). Its interface is part of the ordinary SDK:

```rust,ignore
pub struct MySourceBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for MySourceBootstrapDescriptor {
    fn kind(&self) -> &str { "my-source-bootstrap" }
    fn config_version(&self) -> &str { "1.0.0" }
    fn config_schema_name(&self) -> &str { "source.my_source.MySourceConfig" }
    fn config_schema_json(&self) -> String { MySourceDescriptor.config_schema_json() }

    async fn create_bootstrap_provider(
        &self,
        _bootstrap_config: &serde_json::Value,
        source_config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: MySourceConfigDto = serde_json::from_value(source_config.clone())?;
        let mapper = DtoMapper::new();
        let config = MySourceConfig {
            interval_ms: mapper.resolve_typed(&dto.interval_ms).await?,
            label: mapper.resolve_string(&dto.label).await?,
        };
        Ok(Box::new(OrdersBootstrap::new(config)))
    }
}
```

Then list both descriptors in `export_plugin!`:

```rust,ignore
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "my-source",
    core_version = "0.5.10",
    lib_version = "0.9.3",
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::MySourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::MySourceBootstrapDescriptor],
);
```

> 🔵 **MAY** — Use `SchemaUiAnnotator` inside `config_schema_json()` to group
> fields, mark password widgets, set placeholders, and collapse advanced sections
> so the plugin renders well in a configuration UI.

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

The ordinary binary contract is **`FFI_SDK_VERSION = "0.18.0"`**. Hosts accept
ordinary 0.18.x tables on the same target and reject 0.16/0.17 component layouts
before initialization. Rebuild old plugins; the unchanged state-store layout
does not make their Source lifecycle tables compatible.

The following contracts cross the dynamic plugin boundary:

- `Source` trait methods through `SourceVtable`
- `SourceSubscriptionSettings` fields deconstructed into FFI arguments
- `SubscriptionResponse` reconstructed field-by-field
- source/bootstrap events as serialized MessagePack in C-compatible envelopes,
  not pointers to foreign Rust-layout values
- `ComponentStatus` and `DispatchMode` as FFI enums
- `BootstrapProvider` through cross-plugin vtables

If you are only implementing a Source plugin, do not change these core types. If
you are modifying `drasi-lib`, `drasi-core`, or the plugin SDK itself, the plugin
SDK vtables, host proxies, and FFI SDK version must be updated together.

Context binding has its own callback; `initialize`, `start`, `stop`, `dispose`,
pause/resume, and `delete_owned_resources` report completion/failure through
`FfiResult`. Binding errors are preserved for the fallible initialization call.
The host runs lifecycle callbacks on retained blocking workers rather than
blocking a `current_thread` executor. Cancellation or timeout of the waiter
does not cancel a synchronous plugin callback. The actual object, library and
worker remain owned; retry cleanup instead of restarting or discarding them.
Dropping an undisposed proxy retains unfinished ownership with a diagnostic,
not successful cleanup.

Supported unwinding initialization panics prevent reuse of that object and still
require disposal. Neither the bridge nor no-op hooks can contain `panic=abort`,
memory corruption, or force-cancel arbitrary non-yielding code.

ABI 0.18 source descriptors support creation, not managed update planning or
application. Loading a plugin does not authorize reconstruction, state loss,
or a same-UUID replacement callback. Native source authoring uses the
[separate native SDK](../computation-plugin-sdk/README.md).

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

- mandatory context binding precedes optional initialization
- initialization occurs once across repeated start/stop cycles
- `start()` transitions to Running
- `stop()` transitions to Stopped and clears channel-mode dispatchers
- `subscribe()` returns a receiver
- `supports_replay()` matches configured durability or native capabilities
- `set_bootstrap_provider()` delegates to `SourceBase`
- final disposal is separate from persistent resource deletion

The one piece the trait does not hand you is a `SourceSubscriptionSettings` — the
runtime normally builds it. Construct it directly in tests. This example uses
the volatile counter scaffold and fixed UUIDs for test-only identity:

```rust,ignore
use std::{collections::HashSet, time::Duration};
use tokio::sync::mpsc;
use drasi_lib::{ComponentStatus, Source, SourceRuntimeContext, SourceSubscriptionSettings};
use crate::{MySource, MySourceConfig};

const SOURCE_ID: &str = "11111111-1111-4111-8111-111111111111";
const QUERY_ID: &str = "22222222-2222-4222-8222-222222222222";
const INSTANCE_ID: &str = "44444444-4444-4444-8444-444444444444";

fn settings(query_id: &str) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: SOURCE_ID.to_string(),
        enable_bootstrap: false,
        query_id: query_id.to_string(),
        nodes: HashSet::from(["Counter".to_string()]),
        relations: HashSet::new(),
        resume_from: None,
        resume_sequence: None,
        request_position_handle: false,
    }
}

#[tokio::test]
async fn lifecycle_and_subscription() -> anyhow::Result<()> {
    let source = MySource::new(SOURCE_ID, MySourceConfig::default())?;
    let (update_tx, _updates) = mpsc::channel(32);
    source.bind_runtime_context(SourceRuntimeContext::new(
        INSTANCE_ID, SOURCE_ID, None, update_tx, None,
    )).await;
    source.initialize().await?;

    for _ in 0..2 {
        // Prewire this unit test so the volatile first event is observable.
        let mut response = source.subscribe(settings(QUERY_ID)).await?;
        assert_eq!(response.query_id, QUERY_ID);
        source.start().await?;
        tokio::time::timeout(Duration::from_secs(2), response.receiver.recv()).await??;
        assert_eq!(source.status().await, ComponentStatus::Running);
        source.stop().await?;
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }
    source.dispose().await?;
    Ok(())
}
```

For replay-capable sources, separately assert `SourceError::PositionUnavailable`
for a pruned position; the volatile counter is not a replay fixture. Add
call-count/resource assertions for initialization without start, failed
initialization, partial start, cleanup failure, cancellation and retry. Exercise
real FFI vtables on both Tokio runtime flavors when shipping a dynamic plugin.

### Replay and checkpoint tests

Replay-capable sources need explicit tests for:

- malformed `resume_from` bytes
- resume from an available position
- resume from a pruned/unavailable position
- bootstrap skipped when `resume_from` is present
- `resume_sequence` advances `SourceBase` sequence generation
- position handles pin upstream feedback
- dropping/removing position handles advances the minimum confirmed position
- source-position comparator filters duplicate replay events

### Bootstrap tests

Bootstrap-capable sources/providers should test:

- label filtering through `BootstrapRequest`
- event counts
- `source_position` handover metadata (the snapshot boundary)
- provider errors propagated through the bootstrap result channel
- mix-and-match provider configuration where supported

### Integration tests

Use integration tests for real external systems or protocol servers:

- database containers for CDC and snapshot bootstrap
- local protocol servers for ingress sources
- embedded brokers or test doubles for stream sources
- failure/restart tests for durability

> 🟡 **SHOULD** — Use the Rust `testcontainers` crate to provision external
> services required by integration tests. Prefer test-owned containers with
> explicit readiness checks over manually starting Docker containers, shell
> scripts, or `docker compose` harnesses. This keeps `cargo test` self-contained
> and makes service lifecycle, ports, logs, and cleanup part of the test.

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
- [ ] Selects compatible `drasi-lib`, `drasi-core`, and `drasi-plugin-sdk`
      builds, using matching path dependencies or released APIs.
- [ ] Dynamic plugins include `crate-type = ["lib", "cdylib"]`.
- [ ] Plugin export is gated behind `dynamic-plugin` so static builds do not
      define duplicate FFI entry points.
- [ ] `bytes` is a dependency if the source emits `source_position`, uses replay,
      or uses the WAL.
- [ ] Ingress sources enable tokio's `"net"` (and usually `"io-util"`) features.

#### Source trait

- [ ] Implements every required `Source` method.
- [ ] Delegates required `bind_runtime_context(context)` to `SourceBase`,
      even when optional `initialize()` is not overridden.
- [ ] Preserves the framework-assigned UUID; graph names are not used as IDs.
- [ ] Initializes once per actual object and supports repeated start/stop
      without implicit reconstruction.
- [ ] Disposes initialized-but-never-started and partially initialized objects.
- [ ] Retains unfinished cleanup and prevents conflicting restart/release;
      interrupted cleanup can be retried.
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
- [ ] Replay-capable sources register a `PositionComparator` via
      `set_position_comparator(...).await` in `start()`.
- [ ] Native-log sources return `SourceError::PositionUnavailable` for pruned
      gaps.
- [ ] Transient durable sources append to WAL before acknowledging upstream.
- [ ] WAL pruning/offset commits/flush feedback use minimum confirmed subscriber
      position.
- [ ] Ingress sources bind their port before reporting `Running` and set
      `ComponentStatus::Error` on bind failure.
- [ ] State that must survive restarts (cursors, seen-IDs) is loaded on start and
      saved through the state store.
- [ ] Startup/restart does not dispatch events before subscribers are attached
      when that would create gaps.
- [ ] Stop logic clears channel-mode dispatchers.
- [ ] `delete_owned_resources` deletes only explicitly requested owned state,
      returns unsupported when not implemented, and propagates cleanup failures.
- [ ] Stop and disposal do not delete persistent state or external artifacts.
- [ ] No-op pause/resume hooks are not advertised as proof of paused ingestion.

#### Bootstrap

- [ ] Bootstrap provider honors node/relation label filters.
- [ ] Bootstrap result includes accurate handoff metadata.
- [ ] Source does not bootstrap when `resume_from` is present.
- [ ] `source_position` is reported only when the snapshot boundary is a real position in the Source's own stream.

#### Dynamic plugin packaging

- [ ] DTOs use `ConfigValue<T>` for secrets/env-configurable values.
- [ ] DTOs use `#[serde(rename_all = "camelCase", deny_unknown_fields)]`.
- [ ] DTO schema path matches `config_schema_name()`.
- [ ] Descriptor resolves `ConfigValue` asynchronously through `DtoMapper`.
- [ ] Descriptor preserves raw config via `source.base_mut().set_raw_config(...)`
      or an equivalent source helper.
- [ ] `export_plugin!` lists the source descriptor and any bootstrap descriptors.
- [ ] Host/plugin ordinary ABI is 0.18.x; old 0.16/0.17 layouts are rejected.
- [ ] Creation descriptors are not treated as managed-update authorization.

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
`SourceEventWrapper`, `BootstrapEvent`, `SourceChange`,
`ComponentStatus`, or `DispatchMode`, update the plugin SDK/host SDK mappings and
bump the FFI SDK version.

---

## Appendix A — Full Type Shapes

These shapes are reference orientation; the compile-tested examples earlier in this
guide and the [Public import paths](#public-import-paths) table are the working
source of truth. For exact field-level detail, the published `drasi-lib` and
`drasi-core` API docs on docs.rs match the versions you depend on.

### Source subscription settings

```rust,ignore
pub struct SourceSubscriptionSettings {
    pub source_id: String,
    pub enable_bootstrap: bool,
    pub query_id: String,
    pub nodes: HashSet<String>,
    pub relations: HashSet<String>,
    pub resume_from: Option<Bytes>,
    pub resume_sequence: Option<u64>,
    pub request_position_handle: bool,
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
    pub sequence: u64,
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
| `SourceEventWrapper` | Source and downstream event envelope with a required `sequence: u64`. |
| Event sequence | Source-local monotonic `u64`, supplied by the source or allocated by `SourceBase`, for ordering and query checkpoints. |
| Source position | Opaque bytes owned by the Source that identify an upstream replay position. |
| Position handle | Shared atomic used by a query to report its last durably processed framework sequence back to the Source. |
| Replay | Serving events from a prior persisted source position after restart or resubscription. |
| WAL | Write-ahead log used by transient sources for local durability and replay. |
| `PositionUnavailable` | Structured error meaning the requested replay position has been pruned or cannot be served. |
| Dispatch mode | Channel or Broadcast routing from Source to subscribed queries. |
| `ConfigValue<T>` | Plugin SDK wrapper for static, environment, or secret-backed configuration values. |
| Dynamic plugin | A Source compiled as a `cdylib` and loaded through the plugin SDK FFI boundary. |

---

## Appendix C — Repository Conventions

Sources contributed to the Drasi workspace follow a few shared conventions beyond
the code itself:

- **`source-documentation-standards.md`** (in `components/sources/`) lists the
  reference implementations and the per-source documentation and test expectations
  new sources are held to.
- **Generated code is reproducible.** Sources that compile protobuf or other IDLs
  use a `build.rs` (e.g. with `tonic-build`) and commit the build script, not the
  generated output. Pin the codegen tool versions.
- **Per-crate `Makefile`** targets are standardized — typically `build`, `test`,
  `integration-test`, and `lint` — so every source builds and validates the same
  way. Name container-backed tests so they can be gated or ignored by default.
- **`CHANGELOG.md`** follows Keep a Changelog + SemVer with generated sections
  (e.g. `git-cliff`). Bump `config_version()` when the config DTO changes
  incompatibly.
- **Workspace dependency inheritance.** When inheriting a workspace dependency, the
  root `[workspace.dependencies]` entry must declare `default-features` for a member
  to be able to override it.
- **Config UI.** Use `SchemaUiAnnotator` in `config_schema_json()` so the plugin
  presents well in a configuration UI (field grouping, password widgets,
  placeholders, advanced-section collapse).

### Where to get help

This guide is the authoritative reference for the Rust source-plugin API. The
public Drasi documentation at <https://drasi.io/> explains Drasi's concepts
(sources, continuous queries, reactions) at the platform level: use it for the
*why*, and this guide plus its compile-tested examples for the *how*.
