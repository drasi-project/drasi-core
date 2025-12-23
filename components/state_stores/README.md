# State Store Plugins

State store plugins allow Sources, BootstrapProviders, and Reactions to persist runtime state that can survive restarts of DrasiLib.

## Overview

DrasiLib provides a pluggable state storage architecture:

- **Default**: `MemoryStateStoreProvider` (built into drasi-lib) - Non-persistent, in-memory storage
- **JSON Files**: `JsonStateStoreProvider` - Persistent storage using JSON files

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        DrasiLib                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │   Sources   │  │   Queries   │  │     Reactions       │  │
│  └──────┬──────┘  └─────────────┘  └──────────┬──────────┘  │
│         │                                      │             │
│         │    Arc<dyn StateStoreProvider>       │             │
│         └──────────────┬───────────────────────┘             │
│                        │                                     │
└────────────────────────┼─────────────────────────────────────┘
                         │
         ┌───────────────┴───────────────┐
         │                               │
┌────────▼────────┐           ┌──────────▼──────────┐
│ Memory Provider │           │   JSON Provider      │
│   (built-in)    │           │  (drasi-state-      │
│                 │           │   store-json)       │
└─────────────────┘           └─────────────────────┘
```

## StateStoreProvider Trait

The `StateStoreProvider` trait defines the interface for all state store implementations:

```rust
#[async_trait]
pub trait StateStoreProvider: Send + Sync {
    // Get a single value
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>>;
    
    // Set a single value
    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()>;
    
    // Delete a single key
    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool>;
    
    // Batch operations
    async fn get_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<HashMap<String, Vec<u8>>>;
    async fn set_many(&self, store_id: &str, entries: &[(&str, Vec<u8>)]) -> StateStoreResult<()>;
    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize>;
    
    // Store management
    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize>;
    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>>;
    async fn store_exists(&self, store_id: &str) -> bool;
}
```

### Partitioning with StoreId

The state store uses `store_id` to partition data between different plugins. Each plugin should use a unique `store_id` (typically the plugin's ID) when interacting with the store. This ensures:

- Different plugins don't interfere with each other
- Same keys can be used by different plugins without conflict
- Plugin data can be cleared independently

## Using State Stores

### Default (Memory) Provider

When no state store provider is configured, DrasiLib uses the built-in `MemoryStateStoreProvider`:

```rust
use drasi_lib::DrasiLib;

// Memory provider is used by default
let core = DrasiLib::builder()
    .with_id("my-app")
    .build()
    .await?;
```

### JSON Provider

To use persistent JSON file storage:

```rust
use drasi_lib::DrasiLib;
use drasi_state_store_json::JsonStateStoreProvider;
use std::sync::Arc;

// Create JSON state store provider
let state_store = JsonStateStoreProvider::new("/data/state")?;

// Use with DrasiLib
let core = DrasiLib::builder()
    .with_id("my-app")
    .with_state_store_provider(Arc::new(state_store))
    .build()
    .await?;
```

## Accessing State Store in Plugins

When a Source or Reaction is added to DrasiLib, the state store provider is automatically injected via the `inject_state_store()` method.

### Example: Source with State Storage

```rust
use drasi_lib::plugin_core::{Source, StateStoreProvider};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct MySource {
    id: String,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
}

impl MySource {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state_store: Arc::new(RwLock::new(None)),
        }
    }
    
    async fn save_checkpoint(&self, offset: u64) -> Result<()> {
        if let Some(store) = self.state_store.read().await.as_ref() {
            store.set(&self.id, "checkpoint", offset.to_le_bytes().to_vec()).await?;
        }
        Ok(())
    }
    
    async fn load_checkpoint(&self) -> Result<Option<u64>> {
        if let Some(store) = self.state_store.read().await.as_ref() {
            if let Some(bytes) = store.get(&self.id, "checkpoint").await? {
                let arr: [u8; 8] = bytes.try_into().unwrap_or_default();
                return Ok(Some(u64::from_le_bytes(arr)));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl Source for MySource {
    // ... other trait methods ...
    
    async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }
}
```

## Creating Custom State Store Providers

To create a custom state store provider:

1. Add `drasi-lib` as a dependency
2. Implement the `StateStoreProvider` trait
3. Export your provider type

### Example: Custom Provider

```rust
use drasi_lib::plugin_core::{StateStoreProvider, StateStoreError, StateStoreResult};
use async_trait::async_trait;
use std::collections::HashMap;

pub struct MyStateStoreProvider {
    // Your implementation details
}

impl MyStateStoreProvider {
    pub fn new(/* config */) -> Self {
        Self { /* ... */ }
    }
}

#[async_trait]
impl StateStoreProvider for MyStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        // Your implementation
        todo!()
    }
    
    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        // Your implementation
        todo!()
    }
    
    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        // Your implementation
        todo!()
    }
    
    // ... implement other methods ...
}
```

## Available Providers

| Provider | Crate | Persistence | Use Case |
|----------|-------|-------------|----------|
| Memory | `drasi-lib` (built-in) | No | Development, testing |
| JSON | `drasi-state-store-json` | Yes | Simple persistent storage |

## Best Practices

1. **Use plugin ID as store_id**: Use your plugin's unique identifier as the `store_id` to avoid conflicts
2. **Serialize data carefully**: The state store accepts `Vec<u8>`, so serialize your data appropriately
3. **Handle missing state**: Always handle the case where state might not exist (e.g., first run)
4. **Batch operations**: Use `set_many` and `get_many` for efficiency when working with multiple keys
5. **Clean up**: Use `clear_store` when a plugin is being removed or reset

## Error Handling

All state store operations return `StateStoreResult<T>`, which is an alias for `Result<T, StateStoreError>`:

```rust
pub enum StateStoreError {
    KeyNotFound(String),
    SerializationError(String),
    StorageError(String),
    StoreNotFound(String),
    Other(String),
}
```

Handle errors appropriately in your plugin:

```rust
match store.get(&self.id, "key").await {
    Ok(Some(value)) => { /* use value */ }
    Ok(None) => { /* key doesn't exist */ }
    Err(StateStoreError::StorageError(e)) => { /* handle storage error */ }
    Err(e) => { /* handle other errors */ }
}
```
