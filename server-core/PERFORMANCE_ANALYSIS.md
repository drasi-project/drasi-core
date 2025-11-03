# Performance Issue Analysis: drasi-core/server-core

## Executive Summary

This document provides a comprehensive analysis of three key performance issues in the drasi-core/server-core codebase:

1. **String cloning** (query IDs, source IDs, config fields) - Widespread issue across hot paths
2. **Config struct cloning** (SourceConfig/QueryConfig/ReactionConfig objects) - Unnecessary full struct copies
3. **Unnecessary RwLock usage** - Suboptimal synchronization primitives

Additionally, we identify opportunities for **typed config structs** to replace property maps, reducing runtime overhead and improving type safety.

---

## Issue 1: String Cloning

### Overview
String cloning occurs frequently in hot paths where IDs and field values are cloned unnecessarily. String is a heap-allocated type, so cloning involves heap allocation and memory copying, which can be expensive when done repeatedly in tight loops.

### Current Pattern
```rust
// Current (inefficient)
let source_id = config.id.clone();  // Heap allocation + copy
self.start_source(source_id.clone()).await  // Cloned again for function call
```

### Recommended Solution: Arc<str>
Use `Arc<str>` which provides cheap cloning (atomic reference count increment) instead of heap allocation/copying.

```rust
// Proposed (efficient)
let source_id: Arc<str> = Arc::from(config.id.as_str());
self.start_source(Arc::clone(&source_id)).await  // Cheap Arc clone
```

### Hot Path Locations

#### 1. **SourceManager** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs`

**String clone occurrences in hot paths:**

| Line | Code | Type | Frequency | Impact |
|------|------|------|-----------|--------|
| 156-163 | `config.clone()` passed to sources | Config cloning | Per source creation | Medium |
| 171 | `config.id.clone()` | Source ID | Per application source | Low |
| 182 | `let source_id = config.id.clone()` | Source ID | Per source start | **High** |
| 185 | `config.id.clone()` in HashMap insert | Source ID | Per source add | High |
| 191 | `source_id.clone()` in start call | Source ID | Per auto-start | **High** |
| 242 | `id.clone()` in list results | Source ID | Per list call × sources | **High** |
| 259-260, 266 | `config.id/type.clone()` in SourceRuntime | Config fields | Per get source | Medium |
| 284, 299 | `id.clone()` in method calls | Source ID | Per update/delete | Low |
| 365 | `config.id.clone()` in error handling | Source ID | Per failure | Low |

**Specific Examples:**

```rust
// Line 182-191: Auto-start path (HIGH FREQUENCY in startup)
let source_id = config.id.clone();  // ← Clone 1
let should_auto_start = config.auto_start;

self.sources.write().await.insert(config.id.clone(), source);  // ← Clone 2
info!("Added source: {}", config.id);

if should_auto_start && allow_auto_start {
    info!("Auto-starting source: {}", source_id);
    if let Err(e) = self.start_source(source_id.clone()).await {  // ← Clone 3
        error!("Failed to auto-start source {}: {}", source_id, e);
    }
}
```

**Result: 3 string clones per source during initialization**

---

#### 2. **QueryManager** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs` (1163 lines)

**String clone occurrences:**

| Line | Code | Type | Frequency | Impact |
|------|------|------|-----------|--------|
| 880 | `query.get_config().id.clone()` | Query ID | Per test add | Low |
| 910 | `config.id.clone()` | Query ID | Per add query | High |
| 913 | `config.id.clone()` in HashMap insert | Query ID | Per add query | High |
| 1084 | `id.clone()` in list results | Query ID | Per list × queries | **High** |

**Specific Examples:**

```rust
// Line 893-914: Add query path
async fn add_query_internal(&self, config: QueryConfig) -> Result<()> {
    if self.queries.read().await.contains_key(&config.id) {  // ← Reference OK
        return Err(anyhow::anyhow!(
            "Query with id '{}' already exists",
            config.id  // ← Reference OK
        ));
    }

    let query = DrasiQuery::new(
        config.clone(),  // ← ENTIRE CONFIG CLONED
        self.event_tx.clone(),
        self.source_manager.clone(),
    );

    let query: Arc<dyn Query> = Arc::new(query);

    let query_id = config.id.clone();  // ← Clone 1
    let should_auto_start = config.auto_start;

    self.queries.write().await.insert(config.id.clone(), query);  // ← Clone 2
    info!("Added query: {} with bootstrap support", config.id);  // ← Reference OK
```

**In DrasiQuery::start()** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`

```rust
// Lines 173-176, 186, 232-236, etc.
let event = ComponentEvent {
    component_id: self.base.config.id.clone(),  // ← Clone in event
    component_type: ComponentType::Query,
    status: ComponentStatus::Starting,
    timestamp: chrono::Utc::now(),
    message: Some("Starting query".to_string()),
};

let query_str = self.base.config.query.clone();  // ← Query string clone

// Line 296: In subscribe loop
let subscription_response = match source
    .subscribe(
        self.base.config.id.clone(),  // ← Clone for each source
        enable_bootstrap,
        labels.node_labels.clone(),
        labels.relation_labels.clone(),
    )
    .await
```

---

#### 3. **ReactionManager** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/manager.rs` (501 lines)

**String clone occurrences:**

| Line | Code | Type | Frequency | Impact |
|------|------|------|-----------|--------|
| 240 | `config.id.clone()` | Reaction ID | Per application reaction | Low |
| 246 | `config.id.clone()` | Reaction ID | Per add reaction | High |
| 252 | `config.id.clone()` in HashMap insert | Reaction ID | Per add reaction | High |
| 317-318 | `config.id/type.clone()` in ReactionRuntime | Config fields | Per get reaction | Medium |
| 438 | `id.clone()` in list results | Reaction ID | Per list × reactions | **High** |

---

#### 4. **SourceBase/QueryBase Event Creation** - High-Frequency Path

**In SourceBase** (`/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs`):

```rust
// Line 178-180: Bootstrap context creation
let context = BootstrapContext::new(
    self.config.id.clone(),  // ← Clone server_id
    Arc::new(self.config.clone()),  // ← FULL CONFIG CLONED
    self.config.id.clone(),  // ← Clone again
);
```

**Impact: Full SourceConfig cloned just to wrap in Arc**

---

### Migration Strategy for Arc<str>

#### Phase 1: Config Restructuring
1. Change config struct fields to `Arc<str>`:
   ```rust
   // Before
   pub struct SourceConfig {
       pub id: String,
       pub source_type: String,
   }

   // After
   pub struct SourceConfig {
       pub id: Arc<str>,
       pub source_type: Arc<str>,
   }
   ```

2. Update deserialization to convert String → Arc<str>

#### Phase 2: Manager Updates
1. Replace string clone patterns with Arc::clone
2. Update method signatures to accept `&Arc<str>` instead of `&str`
3. Update HashMap keys to use Arc<str>

#### Phase 3: Event Creation
1. Update ComponentEvent creation to use Arc<str>
2. Update log statements to work with Arc<str> (Display trait works via deref)

---

## Issue 2: Config Struct Cloning

### Overview
Entire `SourceConfig`, `QueryConfig`, and `ReactionConfig` structs are cloned unnecessarily, causing:
- Full HashMap cloning (properties field)
- Vector allocations (sources, queries lists)
- Redundant memory overhead

### Current Pattern: Config Cloning

**SourceManager** - Line 156-166:
```rust
match config.source_type.as_str() {
    "mock" => Arc::new(MockSource::new(config.clone(), self.event_tx.clone())?),
    "postgres" => Arc::new(super::PostgresReplicationSource::new(
        config.clone(),  // ← ENTIRE CONFIG CLONED
        self.event_tx.clone(),
    )?),
    "http" => Arc::new(HttpSource::new(config.clone(), self.event_tx.clone())?),
    // ... for all 6 source types
}
```

**QueryManager** - Line 902-906:
```rust
let query = DrasiQuery::new(
    config.clone(),  // ← ENTIRE QUERYCONFIG CLONED
    self.event_tx.clone(),
    self.source_manager.clone(),
);
```

**ReactionManager** - Line 214-243:
```rust
let reaction: Arc<dyn Reaction> = match config.reaction_type.as_str() {
    "log" => Arc::new(LogReaction::new(config.clone(), self.event_tx.clone())),
    "http" => Arc::new(HttpReaction::new(config.clone(), self.event_tx.clone())),
    // ... for all reaction types
};
```

### SourceConfig Clone Cost Analysis

**Current SourceConfig structure:**
```rust
pub struct SourceConfig {
    pub id: String,                          // 24 bytes
    pub source_type: String,                 // 24 bytes
    pub auto_start: bool,                    // 1 byte
    pub properties: HashMap<String, Value>,  // ~48+ bytes per entry
    pub bootstrap_provider: Option<...>,     // Variable
    pub dispatch_buffer_capacity: Option<u>,  // 8 bytes or 0
    pub dispatch_mode: Option<DispatchMode>,  // 1 byte or 0
}
```

**Clone overhead per source:**
- id: String clone = heap alloc + copy (e.g., "postgres_source" = ~60 bytes)
- source_type: String clone = heap alloc + copy (e.g., "postgres" = ~40 bytes)
- properties: HashMap full clone = ALL entries cloned
  - Example with 5 properties: ~5 String keys + 5 Value variants = potentially 1-10KB
- bootstrap_provider: Full enum/struct clone if Some(...)

**Total per source: 100 bytes - 100KB depending on properties**

---

### QueryConfig Clone Cost Analysis

**Current QueryConfig structure:**
```rust
pub struct QueryConfig {
    pub id: String,                           // 24 bytes
    pub query: String,                        // 24 bytes → actual query 100-1000+ chars
    pub query_language: QueryLanguage,        // 1 byte
    pub sources: Vec<String>,                 // 24 bytes → entries
    pub auto_start: bool,                     // 1 byte
    pub properties: HashMap<String, Value>,   // Variable
    pub joins: Option<Vec<QueryJoinConfig>>,  // Variable
    // ... capacities and flags
}
```

**Clone overhead per query:**
- query: String clone = Large (Cypher queries often 200-2000+ chars)
- sources: Vec<String> clone = Each source_id string cloned (10-50 bytes each × N)
- properties: HashMap clone = All entries
- joins: Option<Vec> clone if present

**Total per query: 1KB - 100KB+ depending on query size and joins**

---

### Sources HashMap Clone (Critical Path)

**In BootstrapContext** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs:179`:

```rust
// Line 179: FULL config wrapped in Arc after being cloned
let context = BootstrapContext::new(
    self.config.id.clone(),
    Arc::new(self.config.clone()),  // ← Entire SourceConfig cloned then Arc'd
    self.config.id.clone(),
);
```

**Issue: Why clone if wrapping in Arc?**
```rust
// Should be:
let context = BootstrapContext::new(
    self.config.id.clone(),
    Arc::new(self.config.clone()),  // Keep if Arc is mutable
    // OR use Arc::new reference if immutable
);
```

---

### ReactionBase Clone Pattern

**In ReactionBase** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs:136-137`:

```rust
let priority_queue = self.priority_queue.clone();
let query_id_clone = query_id.clone();
let reaction_id = self.config.id.clone();
```

**Better approach:**
```rust
let priority_queue = self.priority_queue.clone();  // OK - PriorityQueue likely Arc<T>
let query_id_ref = &query_id;  // Ref for closure
let reaction_id_ref = &self.config.id;  // Ref for closure
```

---

### Recommended Solution: Arc<Config> Pattern

#### Current (Anti-pattern)
```rust
pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
    // In manager:
    let source = MockSource::new(config.clone(), ...)?;  // Clone before passing
}
```

#### Proposed (Better)
```rust
pub fn new(config: Arc<SourceConfig>, event_tx: ComponentEventSender) -> Result<Self> {
    // In manager:
    let config = Arc::new(config);  // Wrap once
    let source = MockSource::new(Arc::clone(&config), ...)?;  // Cheap Arc clone
}
```

#### Benefits
1. **Single clone per config** at manager level
2. **Cheap Arc clones** for internal use
3. **No property map duplication**
4. **Easier testing** with shared immutable configs

---

## Issue 3: Unnecessary RwLock Usage

### Overview
`Arc<RwLock<T>>` is used for fields that don't need full read-write synchronization. Analysis shows:

- **51 instances** of Arc<RwLock<...>> in the codebase
- Many are used for **write-once** patterns (better suited for OnceLock)
- Many track **ComponentStatus enum** (better suited for AtomicU8)
- Many protect **simple primitives** (better suited for Mutex if truly contended)

### Current Usage Patterns

#### 1. ComponentStatus - Best candidate for AtomicU8

**Current Pattern:**
```rust
pub struct SourceBase {
    pub status: Arc<RwLock<ComponentStatus>>,
    // ...
}

// Usage:
*self.status.write().await = ComponentStatus::Running;  // Write lock for enum
let status = self.status.read().await.clone();  // Read lock + clone
```

**Found in:**
- `SourceBase` (line 40) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs`
- `QueryBase` (line 65) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/base.rs`
- `ReactionBase` (line 42) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs`
- `MockReaction` (line 77) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/manager.rs:77`
- 10+ additional locations

**Issue:**
- Read lock acquired even for simple status checks
- Clone required on every read
- Overkill for enum with few discrete values

**ComponentStatus Enum:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error,
}
```

**Proposed Solution: AtomicU8**
```rust
use std::sync::atomic::{AtomicU8, Ordering};

// Define: ComponentStatus as u8 constants or TryFrom impl
const STATUS_STOPPED: u8 = 0;
const STATUS_STARTING: u8 = 1;
const STATUS_RUNNING: u8 = 2;
const STATUS_STOPPING: u8 = 3;
const STATUS_ERROR: u8 = 4;

pub struct SourceBase {
    pub status: Arc<AtomicU8>,
}

// Usage (zero-copy, lock-free):
self.status.store(1, Ordering::SeqCst);  // Set to Starting
let status = self.status.load(Ordering::SeqCst);  // Zero-copy read
```

**Benefits:**
- **Lock-free** atomic operations
- **Zero-copy** reads (no Clone needed)
- **Better performance** in hot paths (status checks)
- 8 bytes vs 16+ bytes for RwLock

---

#### 2. Task Handles - Best candidate for OnceLock

**Current Pattern:**
```rust
pub struct SourceBase {
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    // ...
}

// Usage:
*self.task_handle.write().await = Some(handle);  // Write lock to set
if let Some(handle) = self.task_handle.write().await.take() {  // Write lock to take
    handle.abort();
}
```

**Found in:**
- `SourceBase` (line 50) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs`
- `QueryBase` (line 71) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/base.rs`
- `ReactionBase` (line 50) - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs`

**Issue:**
- Write lock acquired for one-time initialization
- Pattern: set once during start, take once during stop
- No concurrent access to same handle

**Proposed Solution: OnceLock**
```rust
use std::sync::OnceLock;

pub struct SourceBase {
    pub task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    // Or for truly write-once:
    pub task_handle: OnceLock<tokio::task::JoinHandle<()>>,
}

// Usage:
self.task_handle.set(handle).ok();  // Set once (atomic)
if let Some(handle) = self.task_handle.get() {
    handle.abort();  // Read without lock
}
```

**Benefits:**
- **No locks** for read operations
- **Panic-free** setup with `get()`
- **Clearer intent** - write-once semantics

---

#### 3. Shutdown Sender - Best candidate for OnceLock or Mutex

**Current Pattern:**
```rust
pub struct SourceBase {
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
}

// Usage:
*self.shutdown_tx.write().await = Some(tx);  // Set
if let Some(tx) = self.shutdown_tx.write().await.take() {  // Take
    let _ = tx.send(());
}
```

**Found in:**
- `SourceBase` (line 52)
- `QueryBase` (line 73)

**Issue:** Similar to task_handle - write-once pattern with RwLock

**Proposed Solution:**
```rust
pub struct SourceBase {
    pub shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    // Mutex is lighter for single write + single read
}
```

---

#### 4. Dispatchers - Requires Read-Write Access (Keep RwLock, Optimize)

**Current Pattern:**
```rust
pub struct SourceBase {
    pub(crate) dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
}

// Write access: Line 111-112
let mut dispatchers = self.dispatchers.write().await;
dispatchers.push(Box::new(dispatcher));

// Read access: Line 97-99, 510-513
let dispatchers = self.dispatchers.read().await;
for dispatcher in dispatchers.iter() {
    if dispatcher.dispatch_change(arc_result.clone()).await.is_ok() {
        dispatched = true;
    }
}
```

**Analysis:**
- **Legitimate read-write pattern**: Add dispatcher on subscribe, read for dispatch
- **Contention possible** during heavy query creation
- **Optimization possible** but RwLock is appropriate

**Keep RwLock but optimize reads:**
```rust
// Instead of reading in loop closure, cache reads
let dispatchers = self.dispatchers.read().await;
for dispatcher in dispatchers.iter() {
    // dispatch...
}
drop(dispatchers);  // Release earlier if needed
```

---

### Detailed Locations for Optimization

| Location | Field | Current | Recommended | Benefit |
|----------|-------|---------|-------------|---------|
| SourceBase:40 | status | Arc<RwLock<ComponentStatus>> | Arc<AtomicU8> | Lock-free, zero-copy |
| SourceBase:50 | task_handle | Arc<RwLock<Option<JoinHandle>>> | Arc<Mutex<...>> or OnceLock | Lighter lock for rare writes |
| SourceBase:52 | shutdown_tx | Arc<RwLock<Option<Sender>>> | Arc<Mutex<...>> | Write-once pattern |
| QueryBase:65 | status | Arc<RwLock<ComponentStatus>> | Arc<AtomicU8> | Lock-free |
| QueryBase:71 | task_handle | Arc<RwLock<Option<JoinHandle>>> | Arc<Mutex<...>> | Lighter |
| QueryBase:73 | shutdown_tx | Arc<RwLock<Option<Sender>>> | Arc<Mutex<...>> | Write-once |
| ReactionBase:42 | status | Arc<RwLock<ComponentStatus>> | Arc<AtomicU8> | Lock-free |
| ReactionBase:48 | subscription_tasks | Arc<RwLock<Vec<JoinHandle>>> | Arc<Mutex<Vec<...>>> | Lighter for vec updates |
| ReactionBase:50 | processing_task | Arc<RwLock<Option<JoinHandle>>> | Arc<Mutex<...>> | Lighter |

---

### Migration Path

#### Phase 1: ComponentStatus → AtomicU8 (High Impact)
1. Define conversion functions between ComponentStatus and u8
2. Update SourceBase/QueryBase/ReactionBase
3. Update all `.read().await` patterns to `.load()`
4. Update all `.write().await` patterns to `.store()`

#### Phase 2: Task Handles → Mutex or OnceLock (Medium Impact)
1. Change Arc<RwLock<Option<Handle>>> → Arc<Mutex<Option<Handle>>>
2. Update write/read patterns to use Mutex (simpler for this use case)

#### Phase 3: Shutdown Senders → Mutex (Low Impact)
1. Similar to phase 2

---

## Issue 4: Typed Config Structs Design

### Current Architecture: Property Maps

**Problem:**
```rust
pub struct SourceConfig {
    pub properties: HashMap<String, serde_json::Value>,
    // ...
}

// Usage requires runtime parsing:
let host = config.properties
    .get("host")
    .and_then(|v| v.as_str())
    .unwrap_or("localhost");

let port = config.properties
    .get("port")
    .and_then(|v| v.as_u64())
    .unwrap_or(5432) as u16;
```

**Issues:**
1. **No type safety** - Runtime errors possible
2. **Repeated parsing** - Same code in every source type
3. **No IDE support** - Can't autocomplete properties
4. **Hard to refactor** - String-based property names
5. **Performance cost** - HashMap lookups + type conversions

### Proposed Typed Config Design

#### Strategy: Trait-Based Config

**For each source type, create a typed config:**

```rust
// Base config (stays as property map for flexibility)
pub struct SourceConfig {
    pub id: Arc<str>,
    pub source_type: Arc<str>,
    pub auto_start: bool,
    pub properties: HashMap<String, serde_json::Value>,
    // ... common fields
}

// Typed configs created from properties
#[derive(Debug, Clone)]
pub struct PostgresSourceConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub tables: Vec<String>,
    pub table_keys: Vec<TableKeyConfig>,
    pub slot_name: String,
    pub publication_name: String,
    pub ssl_mode: String,
}

impl PostgresSourceConfig {
    pub fn from_properties(props: &HashMap<String, serde_json::Value>) -> Result<Self> {
        Ok(Self {
            host: props.get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("localhost")
                .to_string(),
            port: props.get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(5432) as u16,
            database: props.get("database")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'database'"))?
                .to_string(),
            // ... etc
        })
    }
}

// Usage in PostgresReplicationSource
pub struct PostgresReplicationSource {
    base: SourceBase,
    config: PostgresSourceConfig,  // Typed!
}

impl PostgresReplicationSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        let typed_config = PostgresSourceConfig::from_properties(&config.properties)?;
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
            config: typed_config,
        })
    }
}
```

#### Current Parsing Code (Consolidated)

**PostgresReplicationSource** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/postgres/mod.rs:76-147`:

```rust
fn parse_config(&self) -> Result<PostgresReplicationConfig> {
    let props = &self.base.config.properties;

    Ok(PostgresReplicationConfig {
        host: props.get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("localhost")
            .to_string(),
        port: props.get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(5432) as u16,
        // ... 10+ more fields
    })
}
```

**This code can be:**
1. Extracted to a typed config struct
2. Reused across multiple methods
3. Type-checked at compile time
4. Cached once at construction

#### Recommended Structure

Create a new module: `/config/typed_configs.rs`

```rust
// config/typed_configs.rs

pub mod postgres {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use serde_json::Value;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Config {
        pub host: String,
        pub port: u16,
        pub database: String,
        pub user: String,
        pub password: String,
        pub tables: Vec<String>,
        pub slot_name: String,
        pub publication_name: String,
        pub ssl_mode: String,
        pub table_keys: Vec<TableKeyConfig>,
    }

    impl Config {
        pub fn from_properties(props: &HashMap<String, Value>) -> anyhow::Result<Self> {
            // Consolidated parsing logic
        }
    }
}

pub mod http {
    // Similar for HTTP source
}

pub mod grpc {
    // Similar for gRPC source
}

// ... etc for all source types
```

---

### Property Map Consolidation Opportunities

#### Current Problem: Scattered Parsing

**HttpReaction** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/http/mod.rs:87`:
```rust
if let Some(queries_value) = config.properties.get("queries") {
    if let Ok(queries_array) = queries_value.as_array() {
        // Process
    }
}
```

**AdaptiveHttpReaction** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/http_adaptive.rs:74`:
```rust
if let Some(queries_value) = config.properties.get("queries") {
    if let Ok(queries_array) = queries_value.as_array() {
        // SAME CODE
    }
}
```

**GrpcReaction** - `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/grpc/mod.rs:124`:
```rust
if let Some(meta_value) = config.properties.get("metadata") {
    if let Ok(meta_obj) = meta_value.as_object() {
        // Process
    }
}
```

#### Consolidated Typed Config

```rust
// config/typed_configs.rs - reactions module

pub mod http {
    #[derive(Debug, Clone)]
    pub struct Config {
        pub url: String,
        pub method: String,
        pub headers: Option<HashMap<String, String>>,
        pub queries: Vec<String>,  // Typed!
    }

    impl Config {
        pub fn from_properties(props: &HashMap<String, Value>) -> Result<Self> {
            let queries = props.get("queries")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect())
                .unwrap_or_default();

            Ok(Self {
                url: props.get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'url'"))?
                    .to_string(),
                method: props.get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("POST")
                    .to_string(),
                headers: None,  // Implement if needed
                queries,
            })
        }
    }
}
```

---

### Config Trait Pattern (Optional Advanced)

For shared configuration logic across similar components:

```rust
pub trait TypedConfig: Sized {
    fn from_properties(props: &HashMap<String, Value>) -> Result<Self>;
    fn validate(&self) -> Result<()>;
}

impl TypedConfig for PostgresConfig {
    fn from_properties(props: &HashMap<String, Value>) -> Result<Self> {
        // Consolidated parsing
    }

    fn validate(&self) -> Result<()> {
        if self.database.is_empty() {
            return Err(anyhow::anyhow!("database cannot be empty"));
        }
        Ok(())
    }
}
```

---

## Implementation Priorities

### High Priority (Highest ROI)

1. **String → Arc<str> in IDs** (Issue 1)
   - Impact: 3-5 clones per operation in hot paths
   - Complexity: Medium (config changes)
   - Estimated improvement: 5-15% in hot path latency
   - Files: `sources/manager.rs`, `queries/manager.rs`, `reactions/manager.rs`

2. **ComponentStatus → AtomicU8** (Issue 3)
   - Impact: Lock-free status checks in every operation
   - Complexity: Medium (conversion logic)
   - Estimated improvement: 10-20% in status-check latency
   - Files: `sources/base.rs`, `queries/base.rs`, `reactions/base.rs`

3. **Config cloning → Arc<Config>** (Issue 2)
   - Impact: 100B-100KB+ saved per component creation
   - Complexity: High (sig changes)
   - Estimated improvement: 5-10% startup time
   - Files: All managers, all source/reaction implementations

### Medium Priority

4. **Task Handle RwLock → Mutex** (Issue 3)
   - Impact: Lighter lock for rare operations
   - Complexity: Low
   - Estimated improvement: 1-2%
   - Files: `sources/base.rs`, `queries/base.rs`, `reactions/base.rs`

5. **Typed Config Structs** (Issue 4)
   - Impact: Type safety, reduced parsing overhead
   - Complexity: High (new module, migrations)
   - Estimated improvement: 1-5%
   - Files: New `config/typed_configs.rs`, all source/reaction files

### Lower Priority

6. **Shutdown Sender optimization** (Issue 3)
   - Impact: Minimal (rarely used)
   - Complexity: Low
   - Estimated improvement: <1%

---

## Summary: Specific File Changes Required

### Files Requiring String → Arc<str> Changes

1. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/config/schema.rs`
   - SourceConfig.id, source_type → Arc<str>
   - QueryConfig.id, query → Arc<str>
   - ReactionConfig.id, reaction_type → Arc<str>
   - QueryJoinConfig.id → Arc<str>

2. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs`
   - Lines 156-185: Remove .clone() on config when passing
   - Lines 242, 259-260: Update to handle Arc<str>

3. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`
   - Lines 880-914: Update add_query_internal
   - Lines 1082-1084: Update list_queries

4. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/manager.rs`
   - Lines 213-252: Update add_reaction_internal
   - Line 437-438: Update list_reactions

### Files Requiring RwLock Optimizations

1. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs`
   - Line 40: status → Arc<AtomicU8>
   - Line 50: task_handle → Arc<Mutex<...>>
   - Line 52: shutdown_tx → Arc<Mutex<...>>
   - Update all access patterns

2. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/base.rs`
   - Line 65: status → Arc<AtomicU8>
   - Line 71: task_handle → Arc<Mutex<...>>
   - Line 73: shutdown_tx → Arc<Mutex<...>>

3. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/base.rs`
   - Line 42: status → Arc<AtomicU8>
   - Line 48: subscription_tasks → Arc<Mutex<Vec<...>>>
   - Line 50: processing_task → Arc<Mutex<...>>

### Files Requiring Config Cloning Reduction

1. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs`
   - Line 156-166: Change source constructor signatures to Arc<SourceConfig>

2. `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`
   - Line 902-906: Change DrasiQuery::new to Arc<QueryConfig>

3. All source implementations:
   - MockSource, PostgresReplicationSource, HttpSource, GrpcSource, PlatformSource, ApplicationSource
   - Update to accept Arc<SourceConfig> instead of cloning

4. All reaction implementations:
   - LogReaction, HttpReaction, GrpcReaction, SseReaction, etc.
   - Update to accept Arc<ReactionConfig> instead of cloning

---

## Conclusion

These three performance issues represent significant opportunities for optimization:

- **String cloning** is the most straightforward to fix with highest impact (5-15% improvement)
- **RwLock optimization** requires moderate effort with good payoff (10-20% on status operations)
- **Config cloning** requires more substantial refactoring but reduces memory overhead
- **Typed configs** provide long-term maintenance benefits and type safety

The recommended approach is a phased implementation starting with Arc<str> and AtomicU8, then progressing to Arc<Config> and typed configuration structures.
