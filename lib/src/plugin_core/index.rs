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

//! Index Backend Plugin Trait
//!
//! This module re-exports the `IndexBackendPlugin` trait from `drasi_core::interface`.
//!
//! # Architecture
//!
//! The index plugin system follows pure dependency inversion:
//! - **Core** provides index traits (`ElementIndex`, `ResultIndex`, etc.) and a default
//!   in-memory implementation
//! - **Lib** uses the plugin trait but has no knowledge of specific implementations
//! - **External plugins** (in `components/indexes/`) implement this trait
//! - **Applications** optionally inject plugins into DrasiLib; if none provided,
//!   the in-memory default is used
//!
//! # Usage
//!
//! ## Without a plugin (uses in-memory default)
//! ```ignore
//! let drasi = DrasiLib::builder()
//!     .build()?;
//! ```
//!
//! ## With an external plugin
//! ```ignore
//! use drasi_index_rocksdb::RocksDbIndexProvider;
//!
//! let rocksdb = RocksDbIndexProvider::new(path, true, false);
//! let drasi = DrasiLib::builder()
//!     .with_index_provider(Arc::new(rocksdb))
//!     .build()?;
//! ```

// Re-export from drasi_core to avoid circular dependencies
// The trait is defined in core since it only uses core types
pub use drasi_core::interface::IndexBackendPlugin;
