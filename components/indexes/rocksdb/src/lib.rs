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

#![allow(unexpected_cfgs)]

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
//! let provider = RocksDbIndexProvider::new("/data/drasi", true, false)
//!     .with_memory_budget_bytes(512 << 20)?;
//! let drasi = DrasiLib::builder()
//!     .with_index_provider("rocksdb", Arc::new(provider))
//!     .build()?;
//! ```

/// The transactional RocksDB database type used for all query indexes.
///
/// Pessimistic `TransactionDB` rather than `OptimisticTransactionDB`, for two
/// reasons. First, every optimistic DB eagerly allocates its lock-bucket table
/// at open (`occ_lock_buckets = 1 << 20` bucketed mutexes per DB), a fixed
/// per-query cost paid before any data; pessimistic row locks are sized by the
/// locks actually held, near zero for our single-writer sessions. Second,
/// optimistic commit-time validation reads retained write-buffer history, so
/// history must stay large; pessimistic mode never validates against history,
/// which is what makes the small explicit `WRITE_BUFFER_HISTORY_BYTES` bound
/// safe.
pub type IndexDb = rocksdb::TransactionDB;

/// Flushed-memtable history retained per column family, in bytes.
///
/// Set explicitly on every column family: leaving
/// `max_write_buffer_size_to_maintain` at zero is not neutral, RocksDB sanitizes
/// it back to a large default (128 MiB per CF observed), and the retained
/// memtables count against process memory after every flush. A 1 MiB bound is
/// safe only because pessimistic transactions never validate against history;
/// do not carry it back to an optimistic DB.
pub(crate) const WRITE_BUFFER_HISTORY_BYTES: usize = 1024 * 1024;

/// Apply the explicit flushed-memtable history bound to a set of options.
pub(crate) fn bound_write_buffer_history(opts: &mut rocksdb::Options) {
    opts.set_max_write_buffer_size_to_maintain(WRITE_BUFFER_HISTORY_BYTES as i64);
}

mod cf_options;
pub mod checkpoint;
#[cfg(feature = "plugin-descriptor")]
mod descriptor;
pub mod element_index;
pub mod future_queue;
pub mod live_results;
mod memory;
mod options;
pub mod outbox;
mod plugin;
mod point_lookup;
pub mod result_index;
mod session_state;
mod sizing;
mod storage_models;

// Re-export the plugin provider and unified DB opener for easy access
pub use checkpoint::RocksDbCheckpointStore;
pub use live_results::RocksDbLiveResultsWriter;
pub use memory::{
    RocksDbMemoryBudget, RocksDbMemoryBudgetError, DEFAULT_BLOCK_CACHE_CAPACITY_BYTES,
    DEFAULT_WRITE_BUFFER_BUDGET_BYTES,
};
pub use options::RocksIndexOptions;
pub use outbox::RocksDbOutboxWriter;
pub use plugin::open_unified_db;
pub use plugin::RocksDbIndexProvider;

#[cfg(feature = "plugin-descriptor")]
pub use descriptor::{RocksDbIndexConfigDto, RocksDbIndexDescriptor};

// Re-export session types
pub use session_state::RocksDbSessionControl;
pub use session_state::RocksDbSessionState;
