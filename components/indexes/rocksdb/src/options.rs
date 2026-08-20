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

use crate::RocksDbMemoryBudget;

#[derive(Clone)]
pub struct RocksIndexOptions {
    archive_enabled: bool,
    direct_io: bool,
    memory_budget: RocksDbMemoryBudget,
    pub(crate) large_write_buffer_size: usize,
    pub(crate) small_write_buffer_size: usize,
}

impl RocksIndexOptions {
    /// Create options with the memory resources used to open and maintain the index.
    ///
    /// Per-column-family write-buffer sizes are captured from the shared
    /// manager's current budget and remain fixed for the lifetime of these
    /// options.
    pub fn new(archive_enabled: bool, direct_io: bool, memory_budget: RocksDbMemoryBudget) -> Self {
        let (large_write_buffer_size, small_write_buffer_size) =
            crate::sizing::write_buffer_sizes_for_budget(
                memory_budget.write_buffer_manager().get_buffer_size(),
            );

        Self {
            archive_enabled,
            direct_io,
            memory_budget,
            large_write_buffer_size,
            small_write_buffer_size,
        }
    }

    /// Whether historical element versions are retained.
    pub fn archive_enabled(&self) -> bool {
        self.archive_enabled
    }

    /// Whether RocksDB uses direct I/O.
    pub fn direct_io(&self) -> bool {
        self.direct_io
    }

    /// Memory resources shared by the query databases opened with these options.
    pub fn memory_budget(&self) -> &RocksDbMemoryBudget {
        &self.memory_budget
    }
}
