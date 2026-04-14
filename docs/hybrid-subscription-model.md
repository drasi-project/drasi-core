# Hybrid Subscription Model in Drasi

## Overview

Drasi's continuous query engine moves data through a three-stage pipeline: **Sources → Queries → Reactions**. At each boundary, a *subscription* connects a consumer to a producer. The two subscription relationships use fundamentally different patterns because they solve different problems.

This document explains both subscription patterns, the hybrid reaction subscription model, and why they differ.

---

## 1. The Two Subscription Relationships

### Source → Query: "Give me your data changes"

A **Query subscribes to one or more Sources**. The Query needs a stream of `SourceEventWrapper` items (inserts, updates, deletes on graph elements). The Source doesn't know or care what the Query does with the events — it just dispatches them.

### Query → Reaction: "Give me your query results"

A **Reaction subscribes to one or more Queries**. The Reaction needs a stream of `QueryResult` items (the computed output of the continuous query). Again, the Query doesn't know what the Reaction does with the results.

Both relationships look similar from the outside — a consumer subscribes to a producer and gets a stream of events. But the *mechanism* for setting up the subscription differs, and the reasons are architectural.

---

## 2. Source → Query Subscription (Single Model)

There is only one way a Query subscribes to a Source. The **Query always self-manages** its subscriptions.

### How It Works

When `DrasiQuery::start()` is called:

1. The Query iterates over its configured source IDs
2. For each source, it calls `source.subscribe(settings)` directly
3. The Source returns a `SubscriptionResponse` containing:
   - A `receiver: Box<dyn ChangeReceiver<SourceEventWrapper>>` — an async stream of events
   - An optional `bootstrap_receiver` — for loading initial data
4. The Query spawns a forwarder task that reads from the receiver and pushes events into its internal priority queue

```
┌─────────────┐       subscribe(settings)        ┌──────────────┐
│             │ ──────────────────────────────▶   │              │
│   Query     │                                   │    Source    │
│             │  ◀─── SubscriptionResponse ────   │              │
│             │       { receiver, bootstrap }      │              │
└──────┬──────┘                                   └──────────────┘
       │
       │  spawns forwarder task:
       │  loop { receiver.recv() → priority_queue.enqueue() }
       ▼
  ┌──────────────┐
  │ PriorityQueue│ → event processor → ContinuousQuery engine
  └──────────────┘
```

### Key Details

- **Direct access**: The Query has a direct `Arc<dyn Source>` reference from the `SourceManager`.
- **In-process**: Both Query and Source live in the same process and share the same tokio runtime.
- **Dispatch modes**: The Source can use either `Broadcast` (one-to-many, using `tokio::sync::broadcast`) or `Channel` (one-to-one, using `tokio::sync::mpsc`). The Query adapts its enqueue strategy accordingly:
  - **Channel mode**: `priority_queue.enqueue_wait()` — blocks if queue is full (backpressure, no message loss)
  - **Broadcast mode**: `priority_queue.enqueue()` — drops if queue is full (prevents deadlock with broadcast)

### Why There's Only One Model

Queries always run in-process. There is no FFI boundary between a Query and its Sources. The `SourceManager` provides the Source instances, and the Query can call `source.subscribe()` directly. There's no reason for an intermediary.

---

## 3. Query → Reaction Subscription (Hybrid Model)

The Reaction → Query subscription is more complex because **Reactions can be either native Rust code or FFI plugins loaded from shared libraries**. This creates two fundamentally different runtime scenarios, leading to two subscription paths.

### 3.1 Host-Managed Subscriptions (Default)

This is the default path (`self_manages_subscriptions() = false`). The **ReactionManager** (host) sets up subscriptions on behalf of the Reaction.

```
┌──────────────────┐                           ┌──────────────┐
│                  │                           │              │
│ ReactionManager  │── get_query_instance() ──▶│ QueryManager │
│    (host)        │                           │              │
│                  │◀── Arc<dyn Query> ────────│              │
│                  │                           └──────┬───────┘
│                  │                                  │
│                  │── query.subscribe(reaction_id) ──┘
│                  │◀── QuerySubscriptionResponse { receiver }
│                  │
│                  │   spawns forwarder task:
│                  │   loop {
│                  │       result = receiver.recv()
│                  │       reaction.enqueue_query_result(result)
│                  │   }
└────────┬─────────┘
         │ enqueue_query_result(result)
         ▼
┌──────────────────┐
│    Reaction      │
│  ┌─────────────┐ │
│  │PriorityQueue│ │ → processing loop → side effects (HTTP, gRPC, etc.)
│  └─────────────┘ │
└──────────────────┘
```

**Step by step:**

1. `ReactionManager::start_reaction()` calls `reaction.start()`
2. It checks `reaction.self_manages_subscriptions()` — returns `false`
3. `ReactionManager` calls `subscribe_reaction_to_queries()`:
   a. Gets a `QueryProvider` (which is the `QueryManager`)
   b. For each query ID the reaction is configured to watch:
      - Calls `query_provider.get_query_instance(query_id)` → `Arc<dyn Query>`
      - Calls `query.subscribe(reaction_id)` → `QuerySubscriptionResponse { receiver }`
      - Spawns a forwarder task that reads from the receiver
      - The forwarder calls `reaction.enqueue_query_result(result)` for each result
4. The Reaction's `enqueue_query_result()` pushes the result into its internal `PriorityQueue`
5. The Reaction's processing loop dequeues results and acts on them

### 3.2 Self-Managed Subscriptions (Opt-In)

A Reaction can override `self_manages_subscriptions()` to return `true`. In this case, the **Reaction itself** is responsible for subscribing to queries.

```
┌──────────────────┐                           ┌──────────────┐
│                  │                           │              │
│    Reaction      │── query_provider          │ QueryManager │
│                  │   .get_query_instance() ──▶│              │
│                  │                           │              │
│                  │◀── Arc<dyn Query> ────────│              │
│                  │                           └──────────────┘
│                  │
│                  │── query.subscribe(self.id())
│                  │◀── receiver
│                  │
│                  │   internally:
│  ┌─────────────┐ │   loop { result = receiver.recv()
│  │PriorityQueue│◀──         priority_queue.enqueue(result) }
│  └─────────────┘ │
└──────────────────┘
```

**Step by step:**

1. `ReactionManager::start_reaction()` calls `reaction.start()`
2. It checks `reaction.self_manages_subscriptions()` — returns `true`
3. `ReactionManager` does **nothing else** — no forwarding tasks
4. Inside `reaction.start()`, the reaction calls `self.base.subscribe_to_queries()`:
   a. Gets the `QueryProvider` that was injected via `ReactionRuntimeContext`
   b. For each query ID:
      - Calls `query_provider.get_query_instance(query_id)` → `Arc<dyn Query>`
      - Calls `query.subscribe(self.id())` → `QuerySubscriptionResponse { receiver }`
      - Spawns its own forwarder task internally
5. The reaction processes results directly

### 3.3 The Hybrid Decision Point

The decision happens in `ReactionManager::start_reaction()`:

```rust
// lib/src/reactions/manager.rs:212-216
reaction.start().await?;

if !reaction.self_manages_subscriptions() {
    self.subscribe_reaction_to_queries(&id, reaction).await?;
}
```

That's it. One `if` statement. If the reaction self-manages, the host stays out of the way. If not, the host wires everything up.

### 3.4 Where ReactionProxy Fits In

You're right to ask about this — the `ReactionProxy` is the key piece that makes the host-managed path work for FFI plugins. It **implements the `Reaction` trait** on the host side, acting as an in-process stand-in for the actual reaction code running inside a separate shared library (`.so`/`.dylib`).

#### What ReactionProxy Is

`ReactionProxy` lives in `components/host-sdk/src/proxies/reaction.rs`. It wraps a `ReactionVtable` — a struct of C ABI function pointers that the plugin exported at load time. Every `Reaction` trait method on `ReactionProxy` translates to an FFI call through these function pointers:

```rust
pub struct ReactionProxy {
    vtable: ReactionVtable,           // C ABI function pointers to plugin code
    _library: Arc<Library>,            // Keeps the .so/.dylib loaded
    cached_id: String,
    cached_type_name: String,
    _callback_ctx: Mutex<Option<Arc<InstanceCallbackContext>>>,
    result_tx: Mutex<Option<SyncSender<QueryResult>>>,   // ← the bridge
    _push_ctx: Mutex<Option<Arc<ResultPushContext>>>,
}
```

The critical field is `result_tx` — a `std::sync::mpsc::SyncSender<QueryResult>`. This is the bridge that carries query results from the host-managed forwarder into the plugin's runtime.

#### How Results Cross the FFI Boundary

When a `ReactionProxy` starts, it sets up a push-based result delivery pipeline with **two channels** and a **callback loop**:

```
Host Runtime                    │ FFI Boundary │        Plugin Runtime
                                │              │
ReactionManager                 │              │
  └─ forwarder task             │              │
       │                        │              │
       │  receiver.recv()       │              │
       │  (QueryResult)         │              │
       ▼                        │              │
reaction.enqueue_query_result() │              │
       │                        │              │
       ▼                        │              │
┌──────────────────┐            │              │
│ result_tx.send() │ ═══════════╪══════════════╪═▶ ┌────────────────────┐
│ (std::sync::mpsc)│            │   callback   │   │ result_push_callback│
└──────────────────┘            │   blocks in  │   │ rx.recv() → *mut   │
                                │ spawn_blocking   │ (returns raw ptr)   │
                                │              │   └─────────┬──────────┘
                                │              │             │
                                │              │             ▼
                                │              │   ┌────────────────────┐
                                │              │   │ Plugin's forwarder │
                                │              │   │ task (on plugin's  │
                                │              │   │ tokio runtime)     │
                                │              │   │                    │
                                │              │   │ Box::from_raw(ptr) │
                                │              │   │ inner.enqueue_     │
                                │              │   │ query_result(qr)   │
                                │              │   └─────────┬──────────┘
                                │              │             │
                                │              │             ▼
                                │              │   ┌────────────────────┐
                                │              │   │ Plugin's Reaction  │
                                │              │   │ PriorityQueue      │
                                │              │   │ → processing loop  │
                                │              │   │ → side effects     │
                                │              │   └────────────────────┘
```

**Step by step:**

1. **`ReactionProxy::start()`** (host side):
   - Creates a `std::sync::mpsc::sync_channel::<QueryResult>(256)` — the `(result_tx, rx)` pair
   - Stores `result_tx` in `self.result_tx`
   - Wraps `rx` in a `ResultPushContext`
   - Calls `vtable.start_result_push_fn(state, result_push_callback, ctx_ptr)` — tells the plugin: "here's a callback you can call to get the next result"
   - Calls `vtable.start_fn(state)` — starts the plugin's reaction

2. **Plugin-side forwarder** (plugin runtime, in `plugin-sdk/src/ffi/vtable_gen.rs`):
   - Spawns an async task on the **plugin's** tokio runtime
   - Loops: calls `spawn_blocking(|| callback(ctx))` — this calls back into the host
   - The callback (`result_push_callback`) blocks on `rx.recv()`, waiting for the next `QueryResult`
   - When a result arrives, returns it as a raw pointer (`*mut c_void`)
   - The plugin forwarder reconstructs the `QueryResult` from the pointer
   - Calls `inner.enqueue_query_result(result)` on the **plugin's** `Reaction` implementation

3. **Host forwarder** (host runtime, in `ReactionManager`):
   - Spawned by `subscribe_reaction_to_queries()`
   - Loops: `receiver.recv()` gets `Arc<QueryResult>` from the Query's dispatcher
   - Calls `reaction.enqueue_query_result(result)` on the `ReactionProxy`
   - `ReactionProxy::enqueue_query_result()` calls `result_tx.send(result)` — pushes into the `std::sync::mpsc` channel

4. **The result_push_callback unblocks**: the `rx.recv()` returns the `QueryResult`, which flows into the plugin's processing loop.

#### Why `std::sync::mpsc` Instead of `tokio::sync::mpsc`?

The channel between host and plugin uses `std::sync::mpsc` (from the standard library), not `tokio::sync::mpsc`. This is deliberate:

- The host and plugin run on **separate tokio runtimes**
- `tokio::sync::mpsc` internally relies on runtime-specific thread-local state
- Sharing a `tokio` channel between independently compiled cdylibs would cause undefined behavior (different tokio versions, different runtime instances)
- `std::sync::mpsc` is runtime-agnostic — it just works with OS threading primitives

The blocking `rx.recv()` on the plugin side is wrapped in `spawn_blocking()` to avoid starving the plugin's async workers.

#### The Complete Data Path for an FFI Reaction

Putting it all together, here is the end-to-end flow from a Source change to an FFI Reaction's side effect:

```
1. Source dispatches SourceChange
     │ via ChangeDispatcher<SourceEventWrapper>
     ▼
2. Query's forwarder task receives it
     │ receiver.recv() → priority_queue.enqueue()
     ▼
3. Query's event processor processes the change
     │ ContinuousQuery engine evaluates
     ▼
4. Query dispatches QueryResult
     │ via ChangeDispatcher<QueryResult>
     ▼
5. ReactionManager's forwarder task receives it           ← Host-managed subscription
     │ receiver.recv()
     ▼
6. Calls ReactionProxy.enqueue_query_result(result)       ← Reaction trait impl
     │ result_tx.send(result)  [std::sync::mpsc]
     ▼
7. result_push_callback unblocks                          ← Crosses FFI boundary
     │ rx.recv() returns QueryResult as raw pointer
     ▼
8. Plugin forwarder reconstructs QueryResult              ← Plugin runtime
     │ Box::from_raw(ptr)
     ▼
9. Plugin's Reaction.enqueue_query_result(result)         ← Plugin's own Reaction
     │ priority_queue.enqueue_wait(Arc::new(result))
     ▼
10. Plugin's processing loop dequeues and acts             ← Side effects
     │ HTTP POST, gRPC call, database write, etc.
```

#### Why the Proxy Makes Self-Managed Subscriptions Impossible for FFI

The `ReactionProxy` is why FFI reactions **cannot** self-manage subscriptions. Consider what self-management would require:

1. The plugin's `Reaction::start()` would need to call `query_provider.get_query_instance(id)` — but `QueryProvider` is a Rust trait object that lives in the host runtime. It can't cross the FFI boundary.
2. The plugin would need to call `query.subscribe(reaction_id)` — but `Arc<dyn Query>` is a host-side trait object.
3. The plugin would need to hold a `ChangeReceiver<QueryResult>` — which is a host-side tokio channel receiver.

None of these can be represented as C ABI types. The `ReactionProxy` exists precisely to abstract this away — it presents a simple `enqueue_query_result(QueryResult)` interface where `QueryResult` is a plain data struct that *can* be passed as a raw pointer.

---

## 4. Why the Hybrid Model Exists

### The FFI Problem

FFI (Foreign Function Interface) reactions run in a **separate tokio runtime** inside a dynamically loaded shared library. They communicate with the host via C ABI vtables and opaque pointers. This creates a fundamental problem:

**An FFI reaction cannot hold an `Arc<dyn Query>`.**

The `Query` trait object lives in the host's memory space and is bound to the host's tokio runtime. Passing it across the FFI boundary would require:
- Serializing it (impossible — it's a trait object with internal state)
- Wrapping it in another vtable (possible but complex and fragile)
- Sharing raw pointers (unsafe, lifetime management nightmare)

The host-managed path avoids this entirely: the host holds the `Arc<dyn Query>`, subscribes, receives results, and pushes them to the reaction via the simple `enqueue_query_result()` method — which *can* cross the FFI boundary because `QueryResult` is a plain data struct.

### The Native Advantage

Native reactions (compiled directly into the binary) don't have the FFI constraint. They *can* hold `Arc<dyn Query>` references. Self-managing subscriptions gives them:

- **Simpler architecture**: No intermediary forwarder task
- **Direct control**: The reaction decides exactly when and how to subscribe
- **Flexibility**: Can dynamically subscribe/unsubscribe at runtime
- **Fewer moving parts**: One less task per query subscription

---

## 5. Comparison: Source→Query vs Query→Reaction

| Aspect | Source → Query | Query → Reaction (Host-Managed) | Query → Reaction (Self-Managed) |
|--------|---------------|--------------------------------|-------------------------------|
| **Who subscribes** | Query (always) | ReactionManager (host) | Reaction itself |
| **Who holds the producer reference** | Query holds `Arc<dyn Source>` | ReactionManager holds `Arc<dyn Query>` via QueryProvider | Reaction holds `Arc<dyn Query>` via QueryProvider |
| **Subscription method** | `source.subscribe(settings)` | `query.subscribe(reaction_id)` | `query.subscribe(reaction_id)` |
| **Response type** | `SubscriptionResponse` (receiver + bootstrap) | `QuerySubscriptionResponse` (receiver only) | `QuerySubscriptionResponse` (receiver only) |
| **Forwarder task owner** | Query spawns it | ReactionManager spawns it | Reaction spawns it |
| **Data delivery** | `receiver.recv()` → Query's priority queue | `receiver.recv()` → `reaction.enqueue_query_result()` → Reaction's priority queue | `receiver.recv()` → Reaction's priority queue directly |
| **Bootstrap support** | Yes — `SubscriptionResponse` includes optional `bootstrap_receiver` | No — bootstrap is a Source concept | No |
| **FFI compatible** | N/A (Queries are always in-process) | ✅ Yes — `enqueue_query_result()` crosses FFI | ❌ No — requires `Arc<dyn Query>` |
| **Extra hop** | No | Yes — host forwarder task | No |
| **Backpressure** | Priority queue capacity | Priority queue capacity | Priority queue capacity |

### Structural Symmetry

Despite the different mechanisms, both subscription relationships share the same underlying pattern:

```
Producer (Source or Query)
    │
    ├── Has a ChangeDispatcher<T>
    │   ├── BroadcastChangeDispatcher (1-to-N via tokio::sync::broadcast)
    │   └── ChannelChangeDispatcher (1-to-1 via tokio::sync::mpsc)
    │
    ├── subscribe() creates a ChangeReceiver<T>
    │
    └── dispatch_change(Arc<T>) sends to all receivers
         │
         ▼
Consumer (Query or Reaction)
    │
    ├── Spawns forwarder task: receiver.recv() → priority_queue.enqueue()
    │
    └── Processing loop: priority_queue.dequeue() → handle event
```

The `ChangeDispatcher<T>` / `ChangeReceiver<T>` abstraction is generic over the event type (`T`):
- For Sources: `T = SourceEventWrapper`
- For Queries: `T = QueryResult`

---

## 6. Examples

### Example 1: A Native Reaction Using Host-Managed Subscriptions

This is the most common pattern — used by all 12 built-in reaction components (HTTP, gRPC, SSE, Log, etc.):

```rust
pub struct HttpReaction {
    base: ReactionBase,
    client: reqwest::Client,
    endpoint: String,
}

#[async_trait]
impl Reaction for HttpReaction {
    // self_manages_subscriptions() defaults to false — host manages

    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;

        // Just start the processing loop — host handles subscriptions
        let priority_queue = self.base.priority_queue.clone();
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    result = priority_queue.dequeue() => result,
                };
                // Process the query result
                let _ = client.post(&endpoint)
                    .json(&*result)
                    .send()
                    .await;
            }
        });

        self.base.set_processing_task(task).await;
        Ok(())
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        // Host calls this with each query result
        self.base.enqueue_query_result(result).await
    }

    // ... other trait methods
}
```

**What happens at runtime:**
1. `ReactionManager::add_reaction(http_reaction)` — registers in ComponentGraph
2. `ReactionManager::start_reaction("http-1")`:
   - Calls `http_reaction.start()` → starts processing loop
   - Checks `self_manages_subscriptions()` → `false`
   - Calls `subscribe_reaction_to_queries("http-1", reaction)`:
     - For query "orders-query": subscribes, spawns forwarder
     - Forwarder: `receiver.recv()` → `reaction.enqueue_query_result(result)`
3. Data flows: Source change → Query processes → QueryResult dispatched → Host forwarder receives → `enqueue_query_result()` → PriorityQueue → processing loop → HTTP POST

### Example 2: A Native Reaction Using Self-Managed Subscriptions

```rust
pub struct SmartReaction {
    base: ReactionBase,
}

#[async_trait]
impl Reaction for SmartReaction {
    fn self_manages_subscriptions(&self) -> bool {
        true  // I'll handle my own subscriptions
    }

    async fn start(&self) -> Result<()> {
        // Subscribe to all configured queries
        self.base.subscribe_to_queries().await?;

        self.base.set_status(ComponentStatus::Running, None).await;

        // Start processing loop (same as above)
        let priority_queue = self.base.priority_queue.clone();
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    result = priority_queue.dequeue() => result,
                };
                // Process result...
            }
        });

        self.base.set_processing_task(task).await;
        Ok(())
    }

    // enqueue_query_result() not needed — base.subscribe_to_queries()
    // wires results directly to the priority queue

    // ... other trait methods
}
```

**What happens at runtime:**
1. `ReactionManager::start_reaction("smart-1")`:
   - Calls `smart_reaction.start()`:
     - `subscribe_to_queries()` uses the injected `QueryProvider` to get each query
     - Subscribes and spawns internal forwarder tasks
   - Checks `self_manages_subscriptions()` → `true`
   - Does **nothing else** — reaction handled it

### Example 3: An FFI Plugin Reaction (Host-Managed, Necessarily)

```rust
// In host-sdk/src/proxies/reaction.rs
pub struct ReactionProxy {
    vtable: ReactionVtable,     // C ABI function pointers
    _library: Arc<Library>,      // Keeps .so loaded
    result_tx: Mutex<Option<SyncSender<QueryResult>>>,
    // ...
}

impl Reaction for ReactionProxy {
    // self_manages_subscriptions() defaults to false

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        // Push result through std::sync::mpsc channel to plugin's runtime
        let tx = self.result_tx.lock()?.as_ref()
            .ok_or_else(|| anyhow!("Channel closed"))?;
        tx.send(result)?;
        Ok(())
    }
}
```

**Why it must be host-managed**: The `ReactionProxy` wraps FFI function pointers. It cannot hold an `Arc<dyn Query>` because:
- The Query lives in the host's tokio runtime
- The proxy communicates via C ABI vtables
- Query results must be serialized/copied across the boundary
- `enqueue_query_result()` is the only safe data injection point

---

## 7. Benefits and Disadvantages

### Host-Managed Subscriptions

**Benefits:**
- ✅ Works across FFI boundaries — the only option for plugin reactions
- ✅ Reaction code is simpler — just implement `enqueue_query_result()` and a processing loop
- ✅ Subscription lifecycle managed centrally — easier to debug, monitor, and clean up
- ✅ ReactionManager tracks all forwarder tasks — can abort them on stop/remove
- ✅ Backward compatible — all existing reactions use this path

**Disadvantages:**
- ❌ Extra indirection — one additional forwarder task per query subscription
- ❌ Extra `Arc::try_unwrap()` or clone per result in the forwarder
- ❌ Reaction has no control over subscription timing or dynamic re-subscription
- ❌ Coupling — Reaction depends on ReactionManager to wire subscriptions correctly

### Self-Managed Subscriptions

**Benefits:**
- ✅ Direct data path — no intermediary forwarder task
- ✅ Full control — reaction decides when, how, and to which queries to subscribe
- ✅ Dynamic subscription — can subscribe/unsubscribe at runtime based on state
- ✅ Cleaner architecture — reaction is self-contained

**Disadvantages:**
- ❌ Does not work across FFI — cannot pass `Arc<dyn Query>` to plugin
- ❌ More responsibility on the reaction developer
- ❌ No centralized subscription tracking — ReactionManager doesn't know about these tasks
- ❌ Must handle cleanup carefully — reaction must abort its own forwarder tasks on stop

---

## 8. When to Use Which

| Scenario | Recommended Path | Reason |
|----------|-----------------|--------|
| Standard reaction plugin | Host-managed (default) | Simplest, works everywhere |
| FFI/dynamic plugin | Host-managed (required) | `Arc<dyn Query>` can't cross FFI |
| Reaction needing dynamic subscription | Self-managed | Can subscribe/unsubscribe at will |
| Reaction with complex routing | Self-managed | Full control over result routing |
| Existing reaction code | Host-managed (default) | No changes needed |

**Rule of thumb**: Use the default (host-managed) unless you have a specific reason to self-manage. The self-managed path exists to enable advanced patterns, not as the primary recommendation.
