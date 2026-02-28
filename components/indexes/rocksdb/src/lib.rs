#![allow(unexpected_cfgs)]
// Copyright 2024 The Drasi Authors.
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

//! RocksDB Index Backend for Drasi
//!
//! This crate provides a persistent storage backend for Drasi queries using RocksDB.
//!
//! # Usage
//!
//! ```ignore
//! use drasi_index_rocksdb::RocksDbIndexProvider;
//! use drasi_lib::DrasiLib;
//! use std::sync::Arc;
//!
//! let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
//! let drasi = DrasiLib::builder()
//!     .with_index_provider(Arc::new(provider))
//!     .build()?;
//! ```

pub mod element_index;
pub mod future_queue;
mod plugin;
pub mod result_index;
mod storage_models;

// Re-export the plugin provider and unified DB opener for easy access
pub use plugin::open_unified_db;
pub use plugin::RocksDbIndexProvider;
