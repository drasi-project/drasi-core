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

/// Options controlling how the unified query index DB is opened.
///
/// Marked `#[non_exhaustive]` so new fields can be added without a breaking
/// change. Construct via [`RocksIndexOptions::new`] or
/// [`RocksIndexOptions::default`].
#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct RocksIndexOptions {
    /// Create the archive column family for `past()` support.
    pub archive_enabled: bool,
    /// Use direct I/O for SST reads and flush/compaction.
    pub direct_io: bool,
    /// `write_buffer_size` for high byte-volume column families.
    /// The arena block size is derived from it (`write_buffer_size / 64`,
    /// clamped to `[64 KiB, 1 MiB]`).
    pub(crate) large_write_buffer_size: usize,
    /// `write_buffer_size` for the remaining column families.
    /// The arena block size is derived from it (`write_buffer_size / 64`,
    /// clamped to `[64 KiB, 1 MiB]`).
    pub(crate) small_write_buffer_size: usize,
}

impl Default for RocksIndexOptions {
    fn default() -> Self {
        Self {
            archive_enabled: false,
            direct_io: false,
            large_write_buffer_size: crate::sizing::DEFAULT_LARGE_WRITE_BUFFER_SIZE,
            small_write_buffer_size: crate::sizing::DEFAULT_SMALL_WRITE_BUFFER_SIZE,
        }
    }
}

impl RocksIndexOptions {
    /// Options with the given archive/direct-I/O flags and the internal
    /// per-column-family sizing policy.
    pub fn new(archive_enabled: bool, direct_io: bool) -> Self {
        Self {
            archive_enabled,
            direct_io,
            ..Self::default()
        }
    }
}
