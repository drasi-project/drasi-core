// Copyright 2026 The Drasi Authors.
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

//! Shared memory tuning for the RocksDB index backend.
//!
//! Every query opens its own `OptimisticTransactionDB` with 15 column families.
//! Without shared budgets, each CF gets a private block cache and a 64 MiB
//! write buffer, which costs ~80 MiB of RSS per query before any data
//! (see drasi-core#634). `RocksDbTuning` holds the process- or provider-wide
//! `Cache` and `WriteBufferManager` handles plus the per-CF sizing used by all
//! column families.
//!
//! `RocksDbTuning` is `Clone`, and clones share the same underlying cache and
//! write buffer manager. Passing one tuning value (or clones of it) to several
//! `RocksDbIndexProvider`s makes all of their DBs draw from the same budgets.

use rocksdb::{BlockBasedOptions, Cache, DataBlockIndexType, Options, WriteBufferManager};

/// Default capacity of the shared block cache (all DBs/CFs).
pub const DEFAULT_BLOCK_CACHE_SIZE: usize = 256 * 1024 * 1024;

/// Default global memtable budget, charged against the block cache.
pub const DEFAULT_WRITE_BUFFER_BUDGET: usize = 128 * 1024 * 1024;

/// Default memtable size for hot data CFs (elements, adjacency, values, archive).
pub const DEFAULT_HOT_WRITE_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Default memtable size for all other CFs.
pub const DEFAULT_COLD_WRITE_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Shared memory budgets and per-CF sizing for RocksDB index databases.
///
/// Clones share the same `Cache` and `WriteBufferManager` handles, so the
/// sharing scope (per provider, per instance, or process-wide) is decided by
/// whoever constructs and distributes this value.
#[derive(Clone)]
pub struct RocksDbTuning {
    /// Block cache shared by all DBs and CFs opened with this tuning.
    pub block_cache: Cache,
    /// Global memtable budget shared by all DBs opened with this tuning.
    pub write_buffer_manager: WriteBufferManager,
    /// Memtable size for hot data CFs.
    pub hot_write_buffer_size: usize,
    /// Memtable size for all other CFs.
    pub cold_write_buffer_size: usize,
    /// Keeps the budget saturation monitor alive; shared by all clones and
    /// stopped when the last clone is dropped.
    monitor: std::sync::Arc<crate::budget_monitor::BudgetMonitor>,
}

impl RocksDbTuning {
    /// Create a tuning value with the given budgets.
    ///
    /// The write buffer budget is charged against the block cache, so
    /// `block_cache_size` is the single number bounding both read and write
    /// memory across every DB opened with this tuning.
    pub fn with_budgets(block_cache_size: usize, write_buffer_budget: usize) -> Self {
        let block_cache = Cache::new_lru_cache(block_cache_size);
        let write_buffer_manager = WriteBufferManager::new_write_buffer_manager_with_cache(
            write_buffer_budget,
            false,
            block_cache.clone(),
        );
        // Derive per-CF buffer sizes from the budget so a larger budget also
        // buys fewer flushes (less write amplification), not just more cache.
        // budget/16 keeps the defaults at a 256 MiB budget (16 MiB hot, 8 MiB
        // cold) and caps at RocksDB's stock 64 MiB, where flush frequency
        // stops being the bottleneck.
        const MIB: usize = 1024 * 1024;
        let hot = (block_cache_size / 16).clamp(8 * MIB, 64 * MIB);
        let cold = (hot / 2).clamp(4 * MIB, 16 * MIB);
        let monitor = std::sync::Arc::new(crate::budget_monitor::spawn(
            write_buffer_manager.clone(),
            block_cache.clone(),
        ));
        Self {
            block_cache,
            write_buffer_manager,
            hot_write_buffer_size: hot,
            cold_write_buffer_size: cold,
            monitor,
        }
    }

    /// The saturation monitor handle (test hook; the field itself keeps the
    /// monitor thread alive for the lifetime of the tuning value).
    #[cfg(test)]
    pub(crate) fn monitor_refcount(&self) -> usize {
        std::sync::Arc::strong_count(&self.monitor)
    }

    fn write_buffer_size(&self, hot: bool) -> usize {
        if hot {
            self.hot_write_buffer_size
        } else {
            self.cold_write_buffer_size
        }
    }

    fn block_based_options(&self) -> BlockBasedOptions {
        let mut bbo = BlockBasedOptions::default();
        bbo.set_block_cache(&self.block_cache);
        // Index/filter blocks stay in table-reader memory rather than being
        // charged to the cache: charging them (cache_index_and_filter_blocks)
        // measured a ~4% hit on query-perf ingest, and per-query index SSTs
        // are small enough that the unaccounted memory is negligible.
        bbo
    }

    fn apply_common(&self, opts: &mut Options, hot: bool) {
        let wbs = self.write_buffer_size(hot);
        opts.set_write_buffer_size(wbs);
        // Memtables allocate arena memory in blocks of this size, and the first
        // block is paid at memtable construction. The sanitized default is
        // min(1 MiB, write_buffer_size / 8), which puts a ~1 MiB floor under
        // every CF: with 15+ CFs per query DB that is the dominant at-rest cost.
        // Scale the block with the buffer: small buffers get small blocks
        // (near-free idle CFs), large buffers get 1 MiB blocks (memtable
        // locality for write-heavy loads; benchmarked ~5% of write throughput).
        opts.set_arena_block_size((wbs / 64).clamp(64 * 1024, 1024 * 1024));
    }

    /// Base options for a CF: shared block cache and tiered write buffer.
    pub(crate) fn base_cf_options(&self, hot: bool) -> Options {
        let mut opts = Options::default();
        self.apply_common(&mut opts, hot);
        opts.set_block_based_table_factory(&self.block_based_options());
        opts
    }

    /// Options for point-lookup CFs. This is `Options::optimize_for_point_lookup`
    /// unrolled so the block cache is the shared one instead of a private
    /// per-CF cache the convenience method constructs internally.
    pub(crate) fn point_lookup_cf_options(&self, hot: bool) -> Options {
        let mut opts = Options::default();
        self.apply_common(&mut opts, hot);
        // Memtable bloom for whole-key point lookups (2% of the write buffer,
        // matching optimize_for_point_lookup).
        opts.set_memtable_prefix_bloom_ratio(0.02);
        opts.set_memtable_whole_key_filtering(true);
        let mut bbo = self.block_based_options();
        bbo.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);
        bbo.set_data_block_hash_ratio(0.75);
        bbo.set_bloom_filter(10.0, false);
        opts.set_block_based_table_factory(&bbo);
        opts
    }
}

impl Default for RocksDbTuning {
    fn default() -> Self {
        Self::with_budgets(DEFAULT_BLOCK_CACHE_SIZE, DEFAULT_WRITE_BUFFER_BUDGET)
    }
}

impl std::fmt::Debug for RocksDbTuning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksDbTuning")
            .field("hot_write_buffer_size", &self.hot_write_buffer_size)
            .field("cold_write_buffer_size", &self.cold_write_buffer_size)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tuning_builds() {
        let tuning = RocksDbTuning::default();
        let _ = tuning.base_cf_options(true);
        let _ = tuning.base_cf_options(false);
        let _ = tuning.point_lookup_cf_options(true);
    }

    #[test]
    fn buffer_sizes_derive_from_budget() {
        const MIB: usize = 1024 * 1024;
        let t = RocksDbTuning::with_budgets(256 * MIB, 128 * MIB);
        assert_eq!(t.hot_write_buffer_size, 16 * MIB);
        assert_eq!(t.cold_write_buffer_size, 8 * MIB);
        let small = RocksDbTuning::with_budgets(64 * MIB, 32 * MIB);
        assert_eq!(small.hot_write_buffer_size, 8 * MIB);
        assert_eq!(small.cold_write_buffer_size, 4 * MIB);
        let big = RocksDbTuning::with_budgets(4096 * MIB, 2048 * MIB);
        assert_eq!(big.hot_write_buffer_size, 64 * MIB);
        assert_eq!(big.cold_write_buffer_size, 16 * MIB);
    }

    #[test]
    fn clones_share_monitor() {
        let tuning = RocksDbTuning::default();
        assert_eq!(tuning.monitor_refcount(), 1);
        let clone = tuning.clone();
        assert_eq!(tuning.monitor_refcount(), 2);
        drop(clone);
        assert_eq!(tuning.monitor_refcount(), 1);
    }

    #[test]
    fn clones_share_handles() {
        let tuning = RocksDbTuning::default();
        let clone = tuning.clone();
        // Both handles point at the same cache; capacity reads must agree.
        assert_eq!(
            tuning.block_cache.get_usage(),
            clone.block_cache.get_usage()
        );
    }
}
