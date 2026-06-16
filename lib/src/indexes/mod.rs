// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Storage Backend Configuration for DrasiLib
//!
//! This module provides configuration and factory support for the storage backends
//! used for query indexes. A storage backend is either in-memory or a named persistent
//! plugin (e.g. RocksDB or Redis/Garnet).
//!
//! ## Storage Backend Types
//!
//! - **Memory**: In-memory storage (volatile, fast, no persistence). Fully self-contained.
//! - **Plugin** (`kind: rocksdb`, `kind: redis`, ...): Persistent storage provided by an
//!   index backend plugin. The backend is selected by `kind` and carries its own
//!   plugin-specific config (e.g. `path`, `connectionString`).
//!
//! ## Configuration
//!
//! Storage backends can be declared globally and referenced by id, or configured inline
//! on a single query. Both use a `kind` discriminator and camelCase fields, consistent
//! with every other DrasiLib component DTO.
//!
//! ### Named Backends (Global Declaration)
//!
//! ```yaml
//! storage_backends:
//!   - id: rocks_persistent
//!     kind: rocksdb
//!     path: /data/drasi
//!     enableArchive: true
//!     directIo: false
//!
//! queries:
//!   - id: my_query
//!     query: "MATCH (n) RETURN n"
//!     sources: [my_source]
//!     storage_backend: rocks_persistent
//! ```
//!
//! ### Inline Configuration
//!
//! ```yaml
//! queries:
//!   - id: my_query
//!     query: "MATCH (n) RETURN n"
//!     sources: [my_source]
//!     storage_backend:
//!       kind: memory
//!       enableArchive: true
//! ```
//!
//! ## Embedded (in-process) usage
//!
//! When running DrasiLib as an embedded Rust library, persistent (plugin) backends are
//! supplied as pre-built providers and registered by name via
//! [`crate::builder::DrasiLibBuilder::with_index_provider`]. A query selects a provider
//! by referencing that same name through [`StorageBackendRef::Named`]:
//!
//! ```ignore
//! use drasi_index_rocksdb::RocksDbIndexProvider;
//! use drasi_lib::{DrasiLib, Query, StorageBackendRef};
//! use std::sync::Arc;
//!
//! let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
//! let query = Query::cypher("my_query")
//!     .query("MATCH (n) RETURN n")
//!     .from_source("my_source")
//!     .with_storage_backend(StorageBackendRef::Named("rocksdb".to_string()))
//!     .build();
//! let core = DrasiLib::builder()
//!     .with_index_provider("rocksdb", Arc::new(provider))
//!     .with_query(query)
//!     .build()
//!     .await?;
//! ```
//!
//! Only in-memory backends can be instantiated purely from configuration in embedded
//! mode; a plugin backend always requires a matching injected provider.
//!
//! ## Performance Characteristics
//!
//! | Backend | Persistence | Performance | Use Case |
//! |---------|------------|-------------|----------|
//! | Memory | No | Fastest | Development, testing |
//! | RocksDB | Yes (local) | Fast | Production single-node |
//! | Redis | Yes (distributed) | Good | Production distributed |

pub mod config;
pub mod factory;

pub use config::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};
pub use factory::{IndexError, IndexFactory};

// Re-export IndexSet and IndexBackendPlugin from drasi_core for plugin developers
pub use drasi_core::interface::CreatedIndexes;
pub use drasi_core::interface::IndexBackendPlugin;
pub use drasi_core::interface::IndexSet;
