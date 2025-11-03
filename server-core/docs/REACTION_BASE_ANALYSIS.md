# ReactionBase Abstraction Analysis

## Executive Summary

The reactions implementation would **strongly benefit** from a `ReactionBase` abstraction similar to `SourceBase`. Analysis reveals extensive duplication across 7 concrete reaction implementations, with identical lifecycle management, priority queue handling, subscription management, and task orchestration patterns. A `ReactionBase` abstraction could eliminate approximately **600-800 lines of duplicated code** while improving maintainability and consistency.

---

## 1. Current Architecture Overview

### 1.1 Reaction Trait Definition
Location: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/manager.rs:50-73`

```rust
#[async_trait]
pub trait Reaction: Send + Sync {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &ReactionConfig;
}
```

### 1.2 Current Reaction Implementations
1. **ApplicationReaction** - Internal SDK integration
2. **HttpReaction** - HTTP endpoint callbacks
3. **GrpcReaction** - gRPC streaming service
4. **LogReaction** - Logging output
5. **SseReaction** - Server-Sent Events to browsers
6. **PlatformReaction** - Redis Streams publishing
7. **ProfilerReaction** - Performance metrics collection

Plus two adaptive variants:
- **AdaptiveHttpReaction** - Adaptive batching for HTTP
- **AdaptiveGrpcReaction** - Adaptive batching for gRPC

---

## 2. Common Patterns Across Reactions

### 2.1 Shared Fields Analysis

**All 7 core reactions contain these fields:**

```
┌─────────────────────────────────────────┐
│         Field                           │ Count
├─────────────────────────────────────────┤
│ config: ReactionConfig                  │  7/7 (100%)
│ status: Arc<RwLock<ComponentStatus>>    │ 14/14 occurrences
│ event_tx: ComponentEventSender          │  7/7 (100%)
└─────────────────────────────────────────┘
```

**Priority Queue & Task Management (11/7 = specialized):**
```
┌─────────────────────────────────────────┐
│ priority_queue: PriorityQueue<QR>       │ 11/11 (all that need it)
│ subscription_tasks: Vec<JoinHandle>     │ 8/7 (most reactions)
│ processing_task: JoinHandle             │ All 7 (100%)
└─────────────────────────────────────────┘
```

### 2.2 Initialization Pattern Duplication

**Pattern Found in All 7 Reactions:**

```rust
// ApplicationReaction (lines 132-165)
pub struct ApplicationReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    // ... specific fields ...
    subscription_tasks: Arc<RwLock<Vec<JoinHandle<()>>>>,
    priority_queue: PriorityQueue<QueryResult>,
    processing_task: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl ApplicationReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let (app_tx, app_rx) = mpsc::channel(1000);
        Self {
            config: config.clone(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            // ...
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue: PriorityQueue::new(config.priority_queue_capacity.unwrap_or(10000)),
            processing_task: Arc::new(RwLock::new(None)),
        }
    }
}
```

**Same pattern in:** LogReaction, SseReaction, HttpReaction, GrpcReaction, PlatformReaction, ProfilerReaction

### 2.3 Query Subscription Boilerplate (Identical in All 7)

**Exact pattern found in ApplicationReaction:170-244, LogReaction:119-174, SseReaction:115-163, etc.**

```rust
// GET QueryManager from server_core
let query_manager = server_core.query_manager();

// Subscribe to all configured queries and spawn forwarder tasks
for query_id in &self.config.queries {
    // Get the query instance
    let query = query_manager
        .get_query_instance(query_id)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    // Subscribe to the query
    let subscription_response = query
        .subscribe(self.config.id.clone())
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut receiver = subscription_response.receiver;

    // Clone necessary data for the forwarder task
    let priority_queue = self.priority_queue.clone();
    let query_id_clone = query_id.clone();
    let reaction_id = self.config.id.clone();

    // Spawn forwarder task to read from receiver and enqueue to priority queue
    let forwarder_task = tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(query_result) => {
                    // Enqueue to priority queue for timestamp-ordered processing
                    if !priority_queue.enqueue(query_result).await {
                        warn!(
                            "[{}] Failed to enqueue result from query '{}' - priority queue at capacity",
                            reaction_id, query_id_clone
                        );
                    }
                }
                Err(e) => {
                    // Check if it's a lag error or closed channel
                    let error_str = e.to_string();
                    if error_str.contains("lagged") {
                        warn!(
                            "[{}] Receiver lagged for query '{}': {}",
                            reaction_id, query_id_clone, error_str
                        );
                        continue;
                    } else {
                        info!(
                            "[{}] Receiver error for query '{}': {}",
                            reaction_id, query_id_clone, error_str
                        );
                        break;
                    }
                }
            }
        }
    });

    self.subscription_tasks.write().await.push(forwarder_task);
}
```

**Lines of duplicated code: ~40 lines per reaction × 7 reactions = 280 lines**

### 2.4 Status Transition Pattern

**Pattern found in all reactions (start → starting → running, stop → stopping → stopped):**

```rust
// Start lifecycle
*self.status.write().await = ComponentStatus::Starting;
let _ = self.event_tx.send(ComponentEvent {
    component_id: self.config.id.clone(),
    component_type: ComponentType::Reaction,
    status: ComponentStatus::Starting,
    timestamp: chrono::Utc::now(),
    message: Some("Starting <type> reaction".to_string()),
}).await;

// ... work ...

*self.status.write().await = ComponentStatus::Running;
let _ = self.event_tx.send(ComponentEvent {
    component_id: self.config.id.clone(),
    component_type: ComponentType::Reaction,
    status: ComponentStatus::Running,
    timestamp: chrono::Utc::now(),
    message: Some("<type> reaction started".to_string()),
}).await;

// Stop lifecycle
*self.status.write().await = ComponentStatus::Stopping;
let _ = self.event_tx.send(ComponentEvent { /* ... */ }).await;

// ... cleanup ...

*self.status.write().await = ComponentStatus::Stopped;
let _ = self.event_tx.send(ComponentEvent { /* ... */ }).await;
```

**Lines of duplicated code: ~25 lines per reaction × 7 reactions = 175 lines**

### 2.5 Stop/Cleanup Pattern

**Identical cleanup in all 7 reactions:**

```rust
async fn stop(&self) -> Result<()> {
    info!("[{}] Stopping <type> reaction", self.config.id);

    *self.status.write().await = ComponentStatus::Stopping;

    // Send stopping event...

    // Abort all subscription forwarder tasks
    let mut subscription_tasks = self.subscription_tasks.write().await;
    for task in subscription_tasks.drain(..) {
        task.abort();
    }
    drop(subscription_tasks);

    // Abort the processing task
    let mut processing_task = self.processing_task.write().await;
    if let Some(task) = processing_task.take() {
        task.abort();
    }
    drop(processing_task);

    // Drain the priority queue
    let drained_events = self.priority_queue.drain().await;
    if !drained_events.is_empty() {
        info!(
            "[{}] Drained {} pending events from priority queue",
            self.config.id,
            drained_events.len()
        );
    }

    *self.status.write().await = ComponentStatus::Stopped;

    // Send stopped event...

    Ok(())
}
```

**Lines of duplicated code: ~35 lines per reaction × 7 reactions = 245 lines**

### 2.6 Status and Config Access (Trivial Methods)

All 7 reactions implement identical methods:

```rust
async fn status(&self) -> ComponentStatus {
    self.status.read().await.clone()
}

fn get_config(&self) -> &ReactionConfig {
    &self.config
}
```

**Lines of duplicated code: ~6 lines per reaction × 7 reactions = 42 lines**

---

## 3. Quantified Duplication Analysis

### 3.1 By Reaction Implementation

| Reaction | Common Fields | Lifecycle | Status Events | Shutdown | Helpers | Total |
|----------|---|---|---|---|---|---|
| Application | 8 | Yes | Yes | Yes | Yes | 500+ lines |
| Http | 8 | Yes | Yes | Yes | Yes | 550+ lines |
| Grpc | 8 | Yes | Yes | Yes | Yes | 600+ lines |
| Log | 8 | Yes | Yes | Yes | Yes | 350+ lines |
| Sse | 8 | Yes | Yes | Yes | Yes | 450+ lines |
| Platform | 8 | Yes | Yes | Yes | Yes | 500+ lines |
| Profiler | 8 | Yes | Yes | Yes | Yes | 400+ lines |
| **TOTAL** | **56** | **7×** | **7×** | **7×** | **7×** | **~3350 lines** |

### 3.2 Duplicated Code Estimate

**Conservative estimate of duplicated lines:**
- Query subscription loop: 280 lines
- Status transitions: 175 lines
- Stop/cleanup: 245 lines
- Trivial methods: 42 lines
- Field initialization: 100 lines (across the 7 implementations)

**Total Duplicated Code: ~840 lines**

---

## 4. Comparison with SourceBase Implementation

### 4.1 SourceBase Structure

Location: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs`

**Key features:**
- **Centralized state management**: `config`, `status`, `dispatchers`, `event_tx`, `task_handle`, `shutdown_tx`
- **Lifecycle helpers**: `get_status()`, `set_status()`, `set_task_handle()`, `set_shutdown_tx()`
- **Event handling**: `dispatch_source_change()`, `dispatch_event()`, `broadcast_control()`, `send_component_event()`
- **Subscription management**: `subscribe_with_bootstrap()`, `create_streaming_receiver()`, `test_subscribe()`
- **Shutdown**: `stop_common()` for unified cleanup
- **Task dispatch**: `dispatch_from_task()` for spawned tasks

**Result:** Enabled source implementations to reduce boilerplate significantly

### 4.2 Why ReactionBase Would Be Different

Unlike SourceBase which handles **dispatching/broadcasting**, ReactionBase would handle:
- Query subscription lifecycle
- Priority queue management
- Task forwarder creation
- Status lifecycle (Starting → Running → Stopping → Stopped)
- Event reporting
- Cleanup patterns

---

## 5. Specific Code Segments Duplicated

### 5.1 Configuration Extraction Pattern

**Seen in:** HttpReaction:74-109, GrpcReaction:86-158, LogReaction:43-49, SseReaction:53-76, PlatformReaction:95-149, ProfilerReaction:365-376

```rust
let field_name = config
    .properties
    .get("property_key")
    .and_then(|v| v.as_type())
    .unwrap_or(default_value)
    .to_string();
```

**Duplication:** 8+ instances per reaction

### 5.2 Component Event Sending

**Exact duplication in all 7 reactions (start and stop methods):**

```rust
let event = ComponentEvent {
    component_id: self.config.id.clone(),
    component_type: ComponentType::Reaction,
    status: ComponentStatus::Starting,
    timestamp: chrono::Utc::now(),
    message: Some("Starting <type> reaction".to_string()),
};

if let Err(e) = self.event_tx.send(event).await {
    error!("Failed to send component event: {}", e);
}
```

**Occurs:** 4 times per reaction (Starting, Running, Stopping, Stopped) × 7 = 28 instances

### 5.3 Task Abort/Cleanup Pattern

**Exact duplication in all 7 reactions stop() method:**

```rust
// Abort all subscription forwarder tasks
let mut subscription_tasks = self.subscription_tasks.write().await;
for task in subscription_tasks.drain(..) {
    task.abort();
}
drop(subscription_tasks);

// Abort the processing task
let mut processing_task = self.processing_task.write().await;
if let Some(task) = processing_task.take() {
    task.abort();
}
drop(processing_task);

// Drain the priority queue
let drained_events = self.priority_queue.drain().await;
if !drained_events.is_empty() {
    info!(
        "[{}] Drained {} pending events from priority queue",
        self.config.id,
        drained_events.len()
    );
}
```

**Occurs:** 7 times (once per reaction, near-identical)

---

## 6. Benefits of ReactionBase Abstraction

### 6.1 Code Elimination
- **280 lines**: Query subscription boilerplate
- **175 lines**: Status event transitions
- **245 lines**: Shutdown/cleanup logic
- **100+ lines**: Configuration management helpers
- **Total: 800+ lines** could be moved to base class

### 6.2 Consistency & Correctness
- **Unified lifecycle management**: All reactions follow same state machine
- **Consistent error handling**: Centralized error patterns
- **Event consistency**: All reactions report state changes uniformly
- **Resource cleanup**: Guaranteed cleanup of all spawned tasks

### 6.3 Maintainability
- **Single source of truth** for common patterns
- **Easier to audit** lifecycle correctness
- **Simpler to test** base behavior once
- **Lower barrier** for new reaction implementations

### 6.4 Feature Addition
- **Global timeout settings**: Apply across all reactions
- **Metrics collection**: Centralized for all reactions
- **Circuit breakers**: Implement once, apply everywhere
- **Rate limiting**: Configure per-reaction

---

## 7. Implementation Strategy

### 7.1 ReactionBase Structure

```rust
pub struct ReactionBase {
    /// Reaction configuration
    pub config: ReactionConfig,
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Channel for sending component lifecycle events
    pub event_tx: ComponentEventSender,
    /// Priority queue for timestamp-ordered result processing
    pub priority_queue: PriorityQueue<QueryResult>,
    /// Handles to subscription forwarder tasks
    pub subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    /// Handle to the main processing task
    pub processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl ReactionBase {
    /// Create a new ReactionBase with the given configuration
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self { /* */ }

    /// Subscribe to all configured queries and spawn forwarder tasks
    pub async fn subscribe_to_queries<F>(
        &self,
        server_core: Arc<DrasiServerCore>,
        forwarder_factory: F,
    ) -> Result<()>
    where
        F: Fn(String, String, Arc<Self>) -> tokio::task::JoinHandle<()>,
    { /* */ }

    /// Send a component lifecycle event
    pub async fn send_component_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> { /* */ }

    /// Transition to a new status and send event
    pub async fn set_status_with_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> { /* */ }

    /// Get current status
    pub async fn get_status(&self) -> ComponentStatus { /* */ }

    /// Perform common cleanup (task abortion, queue draining)
    pub async fn stop_common(&self) -> Result<()> { /* */ }
}
```

### 7.2 Refactored Reaction Implementation Example

```rust
pub struct HttpReaction {
    base: ReactionBase,
    // HTTP-specific fields
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    query_configs: HashMap<String, QueryConfig>,
}

impl HttpReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let base = ReactionBase::new(config.clone(), event_tx);

        let base_url = config
            .properties
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("http://localhost")
            .to_string();

        // ... other specific fields ...

        Self {
            base,
            base_url,
            token,
            timeout_ms,
            query_configs,
        }
    }
}

#[async_trait]
impl Reaction for HttpReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        // Transition to Starting state
        self.base.set_status_with_event(
            ComponentStatus::Starting,
            Some("Starting HTTP reaction".to_string()),
        ).await?;

        // Subscribe to all queries using standard pattern
        self.base.subscribe_to_queries(server_core, |query_id, reaction_id, base| {
            // HTTP-specific forwarder logic
            tokio::spawn(async move {
                // forwarder implementation
            })
        }).await?;

        // Transition to Running
        self.base.set_status_with_event(
            ComponentStatus::Running,
            Some("HTTP reaction started".to_string()),
        ).await?;

        // Spawn HTTP-specific processing task
        let priority_queue = self.base.priority_queue.clone();
        let base_url = self.base_url.clone();
        // ... spawn processing task ...

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_status_with_event(
            ComponentStatus::Stopping,
            Some("Stopping HTTP reaction".to_string()),
        ).await?;

        self.base.stop_common().await?;

        self.base.set_status_with_event(
            ComponentStatus::Stopped,
            Some("HTTP reaction stopped".to_string()),
        ).await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.base.config
    }
}
```

### 7.3 Key Design Considerations

1. **Composition over Inheritance**: Use composition (contains ReactionBase) rather than trait-based inheritance
   - Allows flexibility for different task management needs
   - Adapts variants (AdaptiveHttpReaction, AdaptiveGrpcReaction) can have different structures

2. **Closure-based Forwarder Factory**: Allow reactions to customize forwarder behavior
   - Handles subscription loop creation
   - Reactions provide logic for enqueuing results
   - Enables specialized filtering/processing

3. **Preserve Flexibility**: Not all reactions need all features
   - ApplicationReaction has custom handle mechanism
   - SseReaction has broadcast channel
   - Make base features optional through traits

4. **Testing Support**: Include test utilities in ReactionBase
   - Standard test subscription methods
   - Mock event TX support
   - Priority queue snapshot access

---

## 8. Trait Definition Changes

### Current Trait
```rust
#[async_trait]
pub trait Reaction: Send + Sync {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &ReactionConfig;
}
```

### Suggested Changes
No changes required to trait. ReactionBase would be an optional base struct, not a trait requirement. Reactions can:
- Use ReactionBase for standard implementations
- Implement Reaction trait directly if they need custom behavior
- Compose ReactionBase with their own fields

This maintains backward compatibility while providing the abstraction.

---

## 9. Challenges & Mitigation

### 9.1 Specialization Challenges

**Challenge**: Different reactions have different needs
- ApplicationReaction needs custom handle mechanism
- SseReaction uses broadcast channels
- GrpcReaction needs batch management
- PlatformReaction needs sequence counters

**Mitigation**:
- Make ReactionBase handle only **common core**: status, config, event TX, basic task management
- Reactions manage their own specialized data (priority queue setup, batch config, etc.)
- Use composition: `struct HttpReaction { base: ReactionBase, ... }`
- Allow callbacks/closures for customization

### 9.2 Testing Impact

**Challenge**: Existing tests assume direct field access

**Mitigation**:
- Expose necessary fields as public (e.g., `base.priority_queue`, `base.subscription_tasks`)
- Add testing methods on ReactionBase
- Make ReactionBase fields accessible for test verification

### 9.3 Adaptive Reaction Variants

**Challenge**: AdaptiveHttpReaction and AdaptiveGrpcReaction add batching logic

**Mitigation**:
- ReactionBase handles forwarder subscription loop
- Adaptive reactions can add their own batching layer
- Priority queue remains shared
- Processing task logic is reaction-specific anyway

---

## 10. Migration Path

### Phase 1: Foundation (1-2 PRs)
1. Create ReactionBase struct with core fields
2. Implement basic lifecycle helpers
3. Implement send_component_event
4. Write tests for ReactionBase

### Phase 2: Extraction (1-2 PRs per reaction group)
1. Extract LogReaction (simplest, no special features)
2. Extract ApplicationReaction (complex handle mechanism)
3. Extract Http/Grpc (batch-related)
4. Extract Platform/Profiler (specialized logic)

### Phase 3: Cleanup
1. Remove duplicated code from old implementations
2. Verify all tests pass
3. Performance regression testing

---

## 11. Recommendation

### Verdict: STRONGLY RECOMMENDED

**Rationale:**
1. **High duplication**: 800+ lines of identical/near-identical code across 7 implementations
2. **Low risk**: Clear patterns, minimal dependencies between reactions
3. **High benefit**:
   - Simpler to understand system
   - Easier to maintain
   - Consistency in behavior
   - Easier to add new reactions
4. **Precedent**: SourceBase successfully demonstrates this pattern
5. **Flexibility**: Composition approach allows specialization where needed

### Implementation Priority: **MEDIUM**

**Reasoning:**
- Not critical for functionality (all reactions work fine today)
- Valuable for maintainability and future development
- Best done incrementally (one reaction at a time)
- Test coverage exists, making refactoring safer

### Suggested Scope

Start with a **focused ReactionBase** handling:
1. Core fields: config, status, event_tx, priority_queue
2. Task management: subscription_tasks, processing_task
3. Lifecycle helpers: set_status_with_event(), send_component_event()
4. Shutdown: stop_common()
5. Query subscription: subscribe_to_queries() with customization hooks

**First target**: LogReaction (simplest, 350 lines, no special features)

---

## 12. Specific Code Locations

### Files to Modify
- Create: `/server-core/src/reactions/base.rs`
- Modify: `/server-core/src/reactions/log/mod.rs`
- Modify: `/server-core/src/reactions/application/mod.rs`
- Modify: `/server-core/src/reactions/http/mod.rs`
- Modify: `/server-core/src/reactions/grpc/mod.rs`
- Modify: `/server-core/src/reactions/sse/mod.rs`
- Modify: `/server-core/src/reactions/platform/mod.rs`
- Modify: `/server-core/src/reactions/profiler/mod.rs`
- Update: `/server-core/src/reactions/mod.rs`

### Test Coverage to Add
- `base.rs` unit tests (10-15 tests)
- Refactored reaction integration tests (verify backward compatibility)
- Lifecycle state machine tests

---

## Appendix: Detailed Code Comparison

### A1. LogReaction vs ApplicationReaction - Subscription Loop

**LogReaction (lines 119-174):**
```rust
for query_id in &self.config.queries {
    let query = query_manager
        .get_query_instance(query_id)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let subscription_response = query
        .subscribe(self.config.id.clone())
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut receiver = subscription_response.receiver;

    let priority_queue = self.priority_queue.clone();
    let query_id_clone = query_id.clone();
    let reaction_id = self.config.id.clone();

    let forwarder_task = tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(query_result) => {
                    if !priority_queue.enqueue(query_result).await {
                        warn!("[{}] Failed to enqueue result...", reaction_id, query_id_clone);
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    if error_str.contains("lagged") {
                        warn!("[{}] Receiver lagged...", reaction_id, query_id_clone, error_str);
                        continue;
                    } else {
                        info!("[{}] Receiver error...", reaction_id, query_id_clone, error_str);
                        break;
                    }
                }
            }
        }
    });

    self.subscription_tasks.write().await.push(forwarder_task);
}
```

**ApplicationReaction (lines 189-244):** Nearly identical

**GrpcReaction (lines 295+):** Nearly identical

**SseReaction (lines 115+):** Nearly identical

**HttpReaction (lines 295+):** Nearly identical

---

## Summary Table

| Aspect | Lines | Reactions | Total Duplication |
|--------|-------|-----------|-------------------|
| Config extraction | 30-40 | 7 | 210-280 |
| Query subscription | 35-45 | 7 | 245-315 |
| Status lifecycle | 20-30 | 7 | 140-210 |
| Stop/cleanup | 30-40 | 7 | 210-280 |
| Event reporting | 10-15 | 28 instances | 140-210 |
| Trivial methods | 5-10 | 7 | 35-70 |
| **TOTAL** | | | **~800-1200 lines** |

