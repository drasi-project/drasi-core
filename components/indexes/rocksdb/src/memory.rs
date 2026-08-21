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

use std::fmt;
use std::sync::Arc;

use rocksdb::{Cache, WriteBufferManager};

use crate::budget_monitor::BudgetMonitor;

/// Default combined cache capacity for unconfigured embedders.
pub const DEFAULT_BLOCK_CACHE_CAPACITY_BYTES: usize = 256 * 1024 * 1024;

/// Default provider-wide memtable budget.
pub const DEFAULT_WRITE_BUFFER_BUDGET_BYTES: usize = 128 * 1024 * 1024;

const WRITE_BUFFER_BUDGET_DIVISOR: usize = 2;

/// Invalid shared RocksDB memory budget configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RocksDbMemoryBudgetError {
    ZeroBlockCacheCapacity,
    ZeroWriteBufferBudget,
    WriteBufferExceedsCache {
        block_cache_capacity_bytes: usize,
        write_buffer_budget_bytes: usize,
    },
}

impl fmt::Display for RocksDbMemoryBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBlockCacheCapacity => write!(f, "block cache capacity must be greater than 0"),
            Self::ZeroWriteBufferBudget => write!(f, "write buffer budget must be greater than 0"),
            Self::WriteBufferExceedsCache {
                block_cache_capacity_bytes,
                write_buffer_budget_bytes,
            } => write!(
                f,
                "write buffer budget ({write_buffer_budget_bytes} bytes) must not exceed block cache capacity ({block_cache_capacity_bytes} bytes)"
            ),
        }
    }
}

impl std::error::Error for RocksDbMemoryBudgetError {}

struct RocksDbMemoryBudgetInner {
    // Keeps the worker alive until the final shared budget owner is dropped.
    _monitor: BudgetMonitor,
    block_cache: Cache,
    block_cache_capacity_bytes: usize,
    write_buffer_manager: WriteBufferManager,
}

/// Shared block-cache and memtable resources for RocksDB query databases.
///
/// Clone this value into multiple providers to extend the sharing scope beyond
/// one provider. The write-buffer manager is charged to the same cache, so its
/// memtable reservations compete with data, index, and filter blocks under one
/// cache capacity. Sustained write pressure therefore reduces block-cache
/// headroom rather than exceeding the combined bound.
///
/// Each independently constructed budget owns one monitoring thread. Clones
/// share that monitor along with the cache and write-buffer manager.
#[derive(Clone)]
pub struct RocksDbMemoryBudget {
    inner: Arc<RocksDbMemoryBudgetInner>,
}

impl RocksDbMemoryBudget {
    /// Create a budget from one combined capacity using the default policy.
    ///
    /// Half of the capacity is available to memtables. Memtable reservations
    /// are charged to the same cache, so the two capacities are not additive.
    pub fn from_total_budget_bytes(
        total_budget_bytes: usize,
    ) -> Result<Self, RocksDbMemoryBudgetError> {
        Self::new(
            total_budget_bytes,
            total_budget_bytes / WRITE_BUFFER_BUDGET_DIVISOR,
            false,
        )
    }

    /// Create a coherent cache and write-buffer budget.
    pub fn new(
        block_cache_capacity_bytes: usize,
        write_buffer_budget_bytes: usize,
        allow_stall: bool,
    ) -> Result<Self, RocksDbMemoryBudgetError> {
        if block_cache_capacity_bytes == 0 {
            return Err(RocksDbMemoryBudgetError::ZeroBlockCacheCapacity);
        }
        if write_buffer_budget_bytes == 0 {
            return Err(RocksDbMemoryBudgetError::ZeroWriteBufferBudget);
        }
        if write_buffer_budget_bytes > block_cache_capacity_bytes {
            return Err(RocksDbMemoryBudgetError::WriteBufferExceedsCache {
                block_cache_capacity_bytes,
                write_buffer_budget_bytes,
            });
        }

        Ok(Self::from_sizes(
            block_cache_capacity_bytes,
            write_buffer_budget_bytes,
            allow_stall,
        ))
    }

    /// Shared block cache used by every query database and column family.
    pub fn block_cache(&self) -> &Cache {
        &self.inner.block_cache
    }

    /// Capacity configured when the shared block cache was created.
    pub fn block_cache_capacity_bytes(&self) -> usize {
        self.inner.block_cache_capacity_bytes
    }

    /// Shared manager charged with every query database's memtable allocations.
    pub fn write_buffer_manager(&self) -> &WriteBufferManager {
        &self.inner.write_buffer_manager
    }

    fn from_sizes(
        block_cache_capacity_bytes: usize,
        write_buffer_budget_bytes: usize,
        allow_stall: bool,
    ) -> Self {
        let block_cache = Cache::new_lru_cache(block_cache_capacity_bytes);
        let write_buffer_manager = WriteBufferManager::new_write_buffer_manager_with_cache(
            write_buffer_budget_bytes,
            allow_stall,
            block_cache.clone(),
        );
        let monitor = BudgetMonitor::start(write_buffer_manager.clone(), block_cache.clone());
        Self {
            inner: Arc::new(RocksDbMemoryBudgetInner {
                _monitor: monitor,
                block_cache,
                block_cache_capacity_bytes,
                write_buffer_manager,
            }),
        }
    }
}

impl Default for RocksDbMemoryBudget {
    fn default() -> Self {
        Self::from_sizes(
            DEFAULT_BLOCK_CACHE_CAPACITY_BYTES,
            DEFAULT_WRITE_BUFFER_BUDGET_BYTES,
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_unwind_safe<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}

    #[test]
    fn monitor_preserves_public_unwind_safety() {
        assert_unwind_safe::<RocksDbMemoryBudget>();
        assert_unwind_safe::<crate::RocksIndexOptions>();
        assert_unwind_safe::<crate::RocksDbIndexProvider>();
    }

    #[test]
    fn default_budget_has_expected_write_limit() {
        let budget = RocksDbMemoryBudget::default();
        assert_eq!(
            budget.block_cache_capacity_bytes(),
            DEFAULT_BLOCK_CACHE_CAPACITY_BYTES
        );
        assert_eq!(
            budget.write_buffer_manager().get_buffer_size(),
            DEFAULT_WRITE_BUFFER_BUDGET_BYTES
        );
        assert!(budget.write_buffer_manager().enabled());
    }

    #[test]
    fn total_budget_derives_cache_and_write_limits() {
        let budget = RocksDbMemoryBudget::from_total_budget_bytes(64 * 1024 * 1024)
            .expect("valid total budget");
        assert_eq!(budget.block_cache_capacity_bytes(), 64 * 1024 * 1024);
        assert_eq!(
            budget.write_buffer_manager().get_buffer_size(),
            32 * 1024 * 1024
        );
    }

    #[test]
    fn rejects_invalid_sizes() {
        assert!(matches!(
            RocksDbMemoryBudget::from_total_budget_bytes(0),
            Err(RocksDbMemoryBudgetError::ZeroBlockCacheCapacity)
        ));
        assert!(matches!(
            RocksDbMemoryBudget::from_total_budget_bytes(1),
            Err(RocksDbMemoryBudgetError::ZeroWriteBufferBudget)
        ));
        assert!(matches!(
            RocksDbMemoryBudget::new(0, 1, false),
            Err(RocksDbMemoryBudgetError::ZeroBlockCacheCapacity)
        ));
        assert!(matches!(
            RocksDbMemoryBudget::new(1, 0, false),
            Err(RocksDbMemoryBudgetError::ZeroWriteBufferBudget)
        ));
        assert!(matches!(
            RocksDbMemoryBudget::new(1, 2, false),
            Err(RocksDbMemoryBudgetError::WriteBufferExceedsCache { .. })
        ));
    }
}
