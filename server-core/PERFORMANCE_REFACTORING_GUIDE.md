# Performance Refactoring Guide: Code Examples

This document provides concrete before/after code examples for implementing the three performance optimizations.

---

## Part 1: String Cloning → Arc<str>

### Step 1: Update Config Structs

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/config/schema.rs`**

#### Before
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Unique identifier for the source
    pub id: String,
    /// Type of source (e.g., "mock", "kafka", "database")
    pub source_type: String,
    // ... other fields
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Unique identifier for the query
    pub id: String,
    /// Query string (Cypher or GQL depending on query_language)
    pub query: String,
    /// IDs of sources this query subscribes to
    pub sources: Vec<String>,
    // ... other fields
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionConfig {
    /// Unique identifier for the reaction
    pub id: String,
    /// Type of reaction (e.g., "log", "webhook", "notification")
    pub reaction_type: String,
    /// IDs of queries this reaction subscribes to
    pub queries: Vec<String>,
    // ... other fields
}
```

#### After
```rust
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Unique identifier for the source (Arc<str> for cheap cloning)
    #[serde(deserialize_with = "deserialize_arc_str")]
    pub id: Arc<str>,
    /// Type of source (e.g., "mock", "kafka", "database")
    #[serde(deserialize_with = "deserialize_arc_str")]
    pub source_type: Arc<str>,
    // ... other fields (strings → Arc<str> where appropriate)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Unique identifier for the query
    #[serde(deserialize_with = "deserialize_arc_str")]
    pub id: Arc<str>,
    /// Query string (Cypher or GQL)
    #[serde(deserialize_with = "deserialize_arc_str")]
    pub query: Arc<str>,
    /// IDs of sources this query subscribes to
    pub sources: Vec<Arc<str>>,
    // ... other fields
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionConfig {
    /// Unique identifier for the reaction
    #[serde(deserialize_with = "deserialize_arc_str")]
    pub id: Arc<str>,
    /// Type of reaction (e.g., "log", "webhook", "notification")
    #[serde(deserialize_with = "deserialize_arc_str")]
    pub reaction_type: Arc<str>,
    /// IDs of queries this reaction subscribes to
    pub queries: Vec<Arc<str>>,
    // ... other fields
}

// Helper for deserialization
fn deserialize_arc_str<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(Arc::from(s.into_boxed_str()))
}
```

---

### Step 2: Update SourceManager

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/manager.rs`**

#### Before
```rust
async fn add_source_internal(
    &self,
    config: SourceConfig,
    allow_auto_start: bool,
) -> Result<()> {
    if self.sources.read().await.contains_key(&config.id) {
        return Err(anyhow::anyhow!(
            "Source with id '{}' already exists",
            config.id  // ← Clone happens later
        ));
    }

    let source: Arc<dyn Source> = match config.source_type.as_str() {
        // Each source constructor clones config
        "mock" => Arc::new(MockSource::new(config.clone(), self.event_tx.clone())?),
        "postgres" => Arc::new(super::PostgresReplicationSource::new(
            config.clone(),
            self.event_tx.clone(),
        )?),
        // ... for all types
    };

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

    Ok(())
}

pub async fn list_sources(&self) -> Vec<(String, ComponentStatus)> {
    let sources = self.sources.read().await;
    let mut result = Vec::new();

    for (id, source) in sources.iter() {
        let status = source.status().await;
        result.push((id.clone(), status));  // ← Clone for each entry
    }

    result
}

pub async fn get_source(&self, id: String) -> Result<SourceRuntime> {
    let sources = self.sources.read().await;
    if let Some(source) = sources.get(&id) {
        let status = source.status().await;
        let config = source.get_config();
        let runtime = SourceRuntime {
            id: config.id.clone(),  // ← Clone
            source_type: config.source_type.clone(),  // ← Clone
            status: status.clone(),
            error_message: match &status {
                ComponentStatus::Error => Some("Source error occurred".to_string()),
                _ => None,
            },
            properties: config.properties.clone(),  // ← Clone
        };
        Ok(runtime)
    } else {
        Err(anyhow::anyhow!("Source not found: {}", id))
    }
}
```

#### After
```rust
async fn add_source_internal(
    &self,
    config: SourceConfig,
    allow_auto_start: bool,
) -> Result<()> {
    if self.sources.read().await.contains_key(&config.id) {
        return Err(anyhow::anyhow!(
            "Source with id '{}' already exists",
            &config.id  // ← Reference (no clone)
        ));
    }

    // Wrap config in Arc ONCE
    let config = Arc::new(config);

    let source: Arc<dyn Source> = match config.source_type.as_str() {
        // Use Arc::clone instead of config.clone()
        "mock" => Arc::new(MockSource::new(Arc::clone(&config), self.event_tx.clone())?),
        "postgres" => Arc::new(super::PostgresReplicationSource::new(
            Arc::clone(&config),
            self.event_tx.clone(),
        )?),
        // ... for all types (now cheap Arc clones)
    };

    let source_id = Arc::clone(&config.id);  // ← Cheap Arc clone
    let should_auto_start = config.auto_start;

    self.sources.write().await.insert(Arc::clone(&config.id), source);  // ← Cheap
    info!("Added source: {}", config.id);

    if should_auto_start && allow_auto_start {
        info!("Auto-starting source: {}", source_id);
        if let Err(e) = self.start_source(Arc::clone(&source_id)).await {  // ← Cheap
            error!("Failed to auto-start source {}: {}", source_id, e);
        }
    }

    Ok(())
}

pub async fn list_sources(&self) -> Vec<(Arc<str>, ComponentStatus)> {
    let sources = self.sources.read().await;
    let mut result = Vec::new();

    for (id, source) in sources.iter() {
        let status = source.status().await;
        result.push((Arc::clone(id), status));  // ← Cheap Arc clone
    }

    result
}

pub async fn get_source(&self, id: &Arc<str>) -> Result<SourceRuntime> {
    let sources = self.sources.read().await;
    if let Some(source) = sources.get(id) {
        let status = source.status().await;
        let config = source.get_config();
        let runtime = SourceRuntime {
            id: Arc::clone(&config.id),  // ← Cheap
            source_type: Arc::clone(&config.source_type),  // ← Cheap
            status: status.clone(),
            error_message: match &status {
                ComponentStatus::Error => Some("Source error occurred".to_string()),
                _ => None,
            },
            properties: config.properties.clone(),  // Keep (large structure)
        };
        Ok(runtime)
    } else {
        Err(anyhow::anyhow!("Source not found: {}", id))
    }
}
```

---

### Step 3: Update Source Constructors

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/mock/mod.rs`**

#### Before
```rust
pub struct MockSource {
    base: SourceBase,
}

impl MockSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }
}
```

#### After
```rust
use std::sync::Arc;

pub struct MockSource {
    base: SourceBase,
}

impl MockSource {
    pub fn new(config: Arc<SourceConfig>, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }
}
```

---

### Step 4: Update Channels and Events

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/channels/mod.rs`**

#### Before
```rust
#[derive(Debug, Clone)]
pub struct ComponentEvent {
    pub component_id: String,  // ← String clone on every event
    pub component_type: ComponentType,
    pub status: ComponentStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubscriptionResponse {
    pub query_id: String,  // ← String clone
    pub source_id: String,  // ← String clone
    pub receiver: Box<dyn ChangeReceiver<SourceEventWrapper>>,
    pub bootstrap_receiver: Option<BootstrapEventReceiver>,
}
```

#### After
```rust
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ComponentEvent {
    pub component_id: Arc<str>,  // ← Cheap Arc clone
    pub component_type: ComponentType,
    pub status: ComponentStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubscriptionResponse {
    pub query_id: Arc<str>,  // ← Cheap Arc clone
    pub source_id: Arc<str>,  // ← Cheap Arc clone
    pub receiver: Box<dyn ChangeReceiver<SourceEventWrapper>>,
    pub bootstrap_receiver: Option<BootstrapEventReceiver>,
}
```

---

## Part 2: RwLock → AtomicU8 for ComponentStatus

### Step 1: Define ComponentStatus Conversion

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/channels/mod.rs`**

#### Before
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

#### After
```rust
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error,
}

// Constants for atomic operations
const STATUS_STOPPED: u8 = 0;
const STATUS_STARTING: u8 = 1;
const STATUS_RUNNING: u8 = 2;
const STATUS_STOPPING: u8 = 3;
const STATUS_ERROR: u8 = 4;

impl ComponentStatus {
    pub fn as_u8(self) -> u8 {
        match self {
            ComponentStatus::Stopped => STATUS_STOPPED,
            ComponentStatus::Starting => STATUS_STARTING,
            ComponentStatus::Running => STATUS_RUNNING,
            ComponentStatus::Stopping => STATUS_STOPPING,
            ComponentStatus::Error => STATUS_ERROR,
        }
    }

    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            STATUS_STOPPED => Some(ComponentStatus::Stopped),
            STATUS_STARTING => Some(ComponentStatus::Starting),
            STATUS_RUNNING => Some(ComponentStatus::Running),
            STATUS_STOPPING => Some(ComponentStatus::Stopping),
            STATUS_ERROR => Some(ComponentStatus::Error),
            _ => None,
        }
    }
}
```

---

### Step 2: Update SourceBase

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/base.rs`**

#### Before
```rust
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct SourceBase {
    pub config: SourceConfig,
    pub status: Arc<RwLock<ComponentStatus>>,  // RwLock for enum
    pub(crate) dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    pub event_tx: ComponentEventSender,
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl SourceBase {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        let dispatch_mode = config.dispatch_mode.unwrap_or_default();
        let mut dispatchers: Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>> = Vec::new();

        if dispatch_mode == DispatchMode::Broadcast {
            let capacity = config.dispatch_buffer_capacity.unwrap_or(1000);
            let dispatcher = BroadcastChangeDispatcher::<SourceEventWrapper>::new(capacity);
            dispatchers.push(Box::new(dispatcher));
        }

        Ok(Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }
}

// Usage examples showing RwLock overhead
pub async fn set_status(&self, status: ComponentStatus) {
    *self.status.write().await = status;  // Async write lock
}

pub async fn get_status(&self) -> ComponentStatus {
    self.status.read().await.clone()  // Async read lock + clone
}
```

#### After
```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::Mutex;

pub struct SourceBase {
    pub config: Arc<SourceConfig>,
    pub status: Arc<AtomicU8>,  // Lock-free atomic
    pub(crate) dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    pub event_tx: ComponentEventSender,
    pub task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,  // Lighter lock
    pub shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,  // Lighter lock
}

impl SourceBase {
    pub fn new(config: Arc<SourceConfig>, event_tx: ComponentEventSender) -> Result<Self> {
        let dispatch_mode = config.dispatch_mode.unwrap_or_default();
        let mut dispatchers: Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>> = Vec::new();

        if dispatch_mode == DispatchMode::Broadcast {
            let capacity = config.dispatch_buffer_capacity.unwrap_or(1000);
            let dispatcher = BroadcastChangeDispatcher::<SourceEventWrapper>::new(capacity);
            dispatchers.push(Box::new(dispatcher));
        }

        Ok(Self {
            config,
            status: Arc::new(AtomicU8::new(ComponentStatus::Stopped.as_u8())),
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            event_tx,
            task_handle: Arc::new(Mutex::new(None)),
            shutdown_tx: Arc::new(Mutex::new(None)),
        })
    }
}

// Zero-overhead status operations
pub fn set_status(&self, status: ComponentStatus) {
    self.status.store(status.as_u8(), Ordering::SeqCst);  // Atomic, no lock
}

pub fn get_status(&self) -> ComponentStatus {
    let val = self.status.load(Ordering::SeqCst);  // Zero-copy read
    ComponentStatus::from_u8(val).unwrap_or(ComponentStatus::Error)
}

// For async contexts (still used in some places)
pub async fn set_status_async(&self, status: ComponentStatus) {
    self.set_status(status);  // Just call sync version
}

pub async fn get_status_async(&self) -> ComponentStatus {
    self.get_status()  // Just call sync version
}
```

---

### Step 3: Update Usage in Source Implementations

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/mock/mod.rs`**

#### Before
```rust
async fn start(&self) -> Result<()> {
    self.base.set_status(ComponentStatus::Starting).await;  // Async
    self.base
        .send_component_event(
            ComponentStatus::Starting,
            Some("Starting mock source".to_string()),
        )
        .await?;

    let status = Arc::clone(&self.base.status);
    let source_name = self.base.config.id.clone();

    let task = tokio::spawn(async move {
        loop {
            if !matches!(*status.read().await, ComponentStatus::Running) {  // Await + lock
                break;
            }
            // ... work
        }
    });
}
```

#### After
```rust
async fn start(&self) -> Result<()> {
    self.base.set_status(ComponentStatus::Starting);  // No await
    self.base
        .send_component_event(
            ComponentStatus::Starting,
            Some("Starting mock source".to_string()),
        )
        .await?;

    let status = Arc::clone(&self.base.status);
    let source_name = Arc::clone(&self.base.config.id);

    let task = tokio::spawn(async move {
        loop {
            if !matches!(
                ComponentStatus::from_u8(status.load(Ordering::SeqCst)),
                Some(ComponentStatus::Running)
            ) {
                break;  // No await, no lock
            }
            // ... work
        }
    });
}
```

---

### Step 4: Update Task Handle Usage

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`**

#### Before
```rust
// Line 808-809: Taking task handle from RwLock
if let Some(handle) = self.base.task_handle.write().await.take() {
    handle.abort();
    let _ = handle.await;
}

// Line 785: Setting task handle
*task_handle_clone.write().await = Some(handle);
```

#### After
```rust
// Taking task handle from Mutex (simpler pattern)
if let Some(handle) = self.base.task_handle.lock().await.take() {
    handle.abort();
    let _ = handle.await;
}

// Setting task handle
*task_handle_clone.lock().await = Some(handle);
```

---

## Part 3: Config Cloning → Arc<Config>

### Step 1: Update Manager Method Signatures

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`**

#### Before
```rust
pub async fn add_query(&self, config: QueryConfig) -> Result<()> {
    self.add_query_internal(config).await
}

async fn add_query_internal(&self, config: QueryConfig) -> Result<()> {
    if self.queries.read().await.contains_key(&config.id) {
        return Err(anyhow::anyhow!(
            "Query with id '{}' already exists",
            config.id
        ));
    }

    let query = DrasiQuery::new(
        config.clone(),  // ← Full clone here
        self.event_tx.clone(),
        self.source_manager.clone(),
    );

    let query_id = config.id.clone();  // ← String clone
    let should_auto_start = config.auto_start;

    self.queries.write().await.insert(config.id.clone(), query);
    info!("Added query: {} with bootstrap support", config.id);

    Ok(())
}
```

#### After
```rust
pub async fn add_query(&self, config: QueryConfig) -> Result<()> {
    let config = Arc::new(config);  // Wrap once
    self.add_query_internal(config).await
}

async fn add_query_internal(&self, config: Arc<QueryConfig>) -> Result<()> {
    if self.queries.read().await.contains_key(&config.id) {
        return Err(anyhow::anyhow!(
            "Query with id '{}' already exists",
            &config.id  // Reference
        ));
    }

    let query = DrasiQuery::new(
        Arc::clone(&config),  // ← Cheap Arc clone
        self.event_tx.clone(),
        self.source_manager.clone(),
    );

    let query_id = Arc::clone(&config.id);  // ← Cheap Arc<str> clone
    let should_auto_start = config.auto_start;

    self.queries.write().await.insert(Arc::clone(&config.id), query);
    info!("Added query: {} with bootstrap support", config.id);

    Ok(())
}
```

---

### Step 2: Update Component Constructors

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/base.rs`**

#### Before
```rust
pub struct QueryBase {
    pub config: QueryConfig,  // Full copy
    pub status: Arc<RwLock<ComponentStatus>>,
    // ...
}

impl QueryBase {
    pub fn new(config: QueryConfig, event_tx: ComponentEventSender) -> Result<Self> {
        let dispatch_mode = config.dispatch_mode.unwrap_or_default();
        // ... setup

        Ok(Self {
            config,  // Takes ownership of full config
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            // ...
        })
    }
}
```

#### After
```rust
pub struct QueryBase {
    pub config: Arc<QueryConfig>,  // Just Arc pointer
    pub status: Arc<AtomicU8>,
    // ...
}

impl QueryBase {
    pub fn new(config: Arc<QueryConfig>, event_tx: ComponentEventSender) -> Result<Self> {
        let dispatch_mode = config.dispatch_mode.unwrap_or_default();
        // ... setup

        Ok(Self {
            config,  // Takes ownership of Arc (cheap)
            status: Arc::new(AtomicU8::new(ComponentStatus::Stopped.as_u8())),
            // ...
        })
    }
}
```

---

### Step 3: Update Event Creation

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/queries/manager.rs`**

#### Before
```rust
let event = ComponentEvent {
    component_id: self.base.config.id.clone(),  // String clone
    component_type: ComponentType::Query,
    status: ComponentStatus::Starting,
    timestamp: chrono::Utc::now(),
    message: Some("Starting query".to_string()),
};

let query_str = self.base.config.query.clone();  // String clone
```

#### After
```rust
let event = ComponentEvent {
    component_id: Arc::clone(&self.base.config.id),  // Cheap Arc clone
    component_type: ComponentType::Query,
    status: ComponentStatus::Starting,
    timestamp: chrono::Utc::now(),
    message: Some("Starting query".to_string()),
};

let query_str = Arc::clone(&self.base.config.query);  // Cheap Arc clone
```

---

## Part 4: Typed Config Structs

### Step 1: Create Typed Config Module

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/config/typed_configs.rs`** (NEW)

```rust
//! Typed configurations for sources and reactions.
//!
//! This module provides strongly-typed configuration structs
//! that are created from the generic property maps.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use serde_json::Value;

/// PostgreSQL Replication Source Configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresSourceConfig {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

impl PostgresSourceConfig {
    /// Create from generic property map
    pub fn from_properties(props: &HashMap<String, Value>) -> Result<Self> {
        Ok(Self {
            host: props
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("localhost")
                .to_string(),
            port: props
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(5432) as u16,
            database: props
                .get("database")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'database' property"))?
                .to_string(),
            user: props
                .get("user")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'user' property"))?
                .to_string(),
            password: props
                .get("password")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tables: props
                .get("tables")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default(),
            slot_name: props
                .get("slot_name")
                .and_then(|v| v.as_str())
                .unwrap_or("drasi_slot")
                .to_string(),
            publication_name: props
                .get("publication_name")
                .and_then(|v| v.as_str())
                .unwrap_or("drasi_publication")
                .to_string(),
            ssl_mode: props
                .get("ssl_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("prefer")
                .to_string(),
            table_keys: props
                .get("table_keys")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let obj = v.as_object()?;
                            let table = obj.get("table")?.as_str()?.to_string();
                            let key_columns = obj
                                .get("key_columns")?
                                .as_array()?
                                .iter()
                                .filter_map(|k| k.as_str())
                                .map(|s| s.to_string())
                                .collect();
                            Some(TableKeyConfig { table, key_columns })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        if self.database.is_empty() {
            return Err(anyhow::anyhow!("'database' cannot be empty"));
        }
        if self.user.is_empty() {
            return Err(anyhow::anyhow!("'user' cannot be empty"));
        }
        if self.tables.is_empty() {
            return Err(anyhow::anyhow!("'tables' cannot be empty"));
        }
        Ok(())
    }
}

/// HTTP Reaction Configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpReactionConfig {
    pub url: String,
    pub method: String,
    pub headers: Option<HashMap<String, String>>,
    pub queries: Vec<String>,
}

impl HttpReactionConfig {
    /// Create from generic property map
    pub fn from_properties(props: &HashMap<String, Value>) -> Result<Self> {
        let queries = props
            .get("queries")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            url: props
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'url' property"))?
                .to_string(),
            method: props
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("POST")
                .to_string(),
            headers: None,  // Implement if needed
            queries,
        })
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        if self.url.is_empty() {
            return Err(anyhow::anyhow!("'url' cannot be empty"));
        }
        if self.queries.is_empty() {
            return Err(anyhow::anyhow!("'queries' cannot be empty"));
        }
        Ok(())
    }
}

// ... Similar configs for other source/reaction types
```

---

### Step 2: Update Source Implementation

**File: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/sources/postgres/mod.rs`**

#### Before
```rust
pub struct PostgresReplicationSource {
    base: SourceBase,
}

impl PostgresReplicationSource {
    pub fn new(config: Arc<SourceConfig>, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }

    fn parse_config(&self) -> Result<PostgresReplicationConfig> {
        let props = &self.base.config.properties;

        Ok(PostgresReplicationConfig {
            host: props
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("localhost")
                .to_string(),
            // ... 10+ more fields with same pattern
        })
    }
}

#[async_trait]
impl Source for PostgresReplicationSource {
    async fn start(&self) -> Result<()> {
        // Parse config every time
        let config = self.parse_config()?;
        // Use config
    }
}
```

#### After
```rust
use crate::config::typed_configs::PostgresSourceConfig;

pub struct PostgresReplicationSource {
    base: SourceBase,
    config: PostgresSourceConfig,  // Typed config
}

impl PostgresReplicationSource {
    pub fn new(config: Arc<SourceConfig>, event_tx: ComponentEventSender) -> Result<Self> {
        // Parse typed config once at construction
        let typed_config = PostgresSourceConfig::from_properties(&config.properties)?;
        typed_config.validate()?;  // Validate once

        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
            config: typed_config,
        })
    }
}

#[async_trait]
impl Source for PostgresReplicationSource {
    async fn start(&self) -> Result<()> {
        // Use pre-parsed config (no parsing overhead)
        let config = &self.config;
        // Use config
    }
}
```

---

## Summary: Refactoring Checklist

### Phase 1: Arc<str> IDs (Highest Priority)

- [ ] Update config/schema.rs: SourceConfig, QueryConfig, ReactionConfig IDs to Arc<str>
- [ ] Add deserialization helper for Arc<str>
- [ ] Update channels/mod.rs: ComponentEvent, SubscriptionResponse
- [ ] Update sources/manager.rs: Remove string clones, use Arc::clone
- [ ] Update queries/manager.rs: Remove string clones, use Arc::clone
- [ ] Update reactions/manager.rs: Remove string clones, use Arc::clone
- [ ] Update all source constructors to accept Arc<SourceConfig>
- [ ] Update all reaction constructors
- [ ] Update all log statements (Arc<str> derefs automatically)
- [ ] Update tests to work with Arc<str>

### Phase 2: RwLock → Atomic/Mutex (High Priority)

- [ ] Update channels/mod.rs: ComponentStatus u8 conversion functions
- [ ] Update sources/base.rs: status RwLock → AtomicU8, task_handle → Mutex
- [ ] Update queries/base.rs: status RwLock → AtomicU8, task_handle → Mutex
- [ ] Update reactions/base.rs: status RwLock → AtomicU8, subscription_tasks → Mutex, processing_task → Mutex
- [ ] Update all status access patterns (.read().await → .load(), etc.)
- [ ] Update all task_handle patterns (RwLock::write → Mutex::lock)
- [ ] Add sync alternatives for status (set_status_async, get_status_async)
- [ ] Update tests

### Phase 3: Config Cloning (Medium Priority)

- [ ] Update sources/manager.rs: Wrap config in Arc once, pass Arc::clone
- [ ] Update queries/manager.rs: Wrap config in Arc once, pass Arc::clone
- [ ] Update reactions/manager.rs: Wrap config in Arc once, pass Arc::clone
- [ ] Update all SourceBase, QueryBase, ReactionBase to accept Arc<Config>
- [ ] Update event creation to use Arc::clone for config fields
- [ ] Update all method signatures passing configs

### Phase 4: Typed Configs (Medium Priority, Future)

- [ ] Create config/typed_configs.rs module
- [ ] Implement from_properties for PostgresSourceConfig
- [ ] Implement from_properties for HttpReactionConfig, etc.
- [ ] Update PostgresReplicationSource to use typed config
- [ ] Update other sources/reactions incrementally
- [ ] Remove parse_config methods as they're consolidated

---

## Expected Performance Improvements

After completing these optimizations:

| Optimization | Area | Expected Improvement |
|--------------|------|----------------------|
| Arc<str> IDs | String cloning | 5-15% latency reduction in hot paths |
| AtomicU8 status | Lock-free operations | 10-20% improvement in status checks |
| Config Arc | Memory overhead | 5-10% startup time reduction |
| Typed configs | Type safety + parsing | 1-5% overhead reduction |
| **Total** | **Overall** | **15-35% potential improvement** |

The exact improvement depends on workload characteristics and contention patterns in your production environment.
