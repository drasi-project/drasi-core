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

//! # Storage Backend Configuration for DrasiServerCore
//!
//! This module provides configuration and factory support for different storage backends
//! that can be used for query indexes. DrasiServerCore supports three storage backends:
//!
//! ## Storage Backend Types
//!
//! - **Memory**: In-memory storage (volatile, fast, no persistence)
//! - **RocksDB**: Local persistent storage using RocksDB (persistent, local, production-ready)
//! - **Redis/Garnet**: Distributed storage using Redis/Garnet (persistent, distributed, scalable)
//!
//! ## Configuration
//!
//! Storage backends can be configured globally and referenced by queries, or configured
//! inline for specific queries.
//!
//! ### Named Backends (Global Configuration)
//!
//! ```yaml
//! storage_backends:
//!   - id: rocks_persistent
//!     backend_type: rocksdb
//!     path: /data/drasi
//!     enable_archive: true
//!     direct_io: false
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
//!       backend_type: rocksdb
//!       path: /data/drasi
//!       enable_archive: true
//! ```
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
pub use factory::{IndexError, IndexFactory, IndexSet};
