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

//! Per-column-family memory sizing policy for the unified query index DB.
//!
//! Every column family in the unified DB gets an explicit `write_buffer_size`
//! and `arena_block_size` here, instead of RocksDB's defaults (64 MiB buffers,
//! 1 MiB sanitized arena blocks). The defaults are tuned for a single busy
//! database; a query DB holds fifteen column families, most of them small or
//! idle, and pays fixed memory floors per CF that scale with the buffer size:
//! the memtable bloom filters on the point-lookup CFs are allocated at 2% of
//! `write_buffer_size` (~1.3 MiB each at the 64 MiB default, ~5.2 MiB per
//! query across the four such CFs), and any CF that accumulates more than the
//! arena's 2 KiB inline block allocates a full arena block. Sizing both
//! explicitly cuts the touched per-query floor roughly 5x and halves the
//! memtable ceiling under load, at throughput parity (see issue #692 for
//! measurements).
//!
//! Two buffer sizes, assigned by per-source-change byte volume:
//!
//! - **Large (16 MiB)**: `elements` (full element blob per change), `inbound` /
//!   `outbound` (adjacency entries per relation change), `values`
//!   (aggregation accumulators), `archive` (a full element version appended
//!   per update when enabled), `outbox` and `live_results` (a serialized
//!   result diff / row set update per result-producing change).
//! - **Small (8 MiB)**: everything else: same write *frequency* in some cases
//!   (`slots` is written per element upsert) but tiny values, or genuinely
//!   quiet CFs (`metadata`, `fqueue`, `findex`, `stream_state`, `partial`,
//!   `sorted-sets`, and the unused `default` CF).
//!
//! `arena_block_size` is derived, not configurable: `write_buffer_size / 64`,
//! clamped to [64 KiB, 1 MiB]. Left unset, RocksDB sanitizes it to
//! `min(1 MiB, write_buffer_size / 8)`, which puts a 1 MiB floor under every
//! CF that sees any real write; the derived value keeps barely-active CFs at
//! 128-256 KiB instead. Allocations larger than a quarter block (e.g. the
//! memtable bloom filters) bypass the block pool as exact-size allocations,
//! so small blocks waste nothing.
//!
//! The flushed-memtable history bound (see [`crate::bound_write_buffer_history`])
//! is applied here too, so this module is the single place that owns per-CF
//! option policy. The DB-level `db_write_buffer_size` cap in
//! [`crate::open_unified_db`] is intentionally untouched: with right-sized
//! buffers it acts as a backstop rather than the flush scheduler.

use rocksdb::{ColumnFamilyDescriptor, Options};

use crate::RocksIndexOptions;

/// Default `write_buffer_size` for column families with high byte volume per
/// source change.
pub const DEFAULT_LARGE_WRITE_BUFFER_SIZE: usize = 16 * 1024 * 1024;

/// Default `write_buffer_size` for the remaining column families: quiet ones,
/// and ones written often but with tiny values.
pub const DEFAULT_SMALL_WRITE_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Column families that get the large write buffer. Everything not listed here
/// (including the mandatory `default` CF) gets the small one. Names must
/// match the CF name constants in
/// their owning modules; `cf_sizing_tests` asserts the effective values per
/// name against the OPTIONS file RocksDB writes at open.
const LARGE_BUFFER_CFS: &[&str] = &[
    "elements",
    "inbound",
    "outbound",
    "values",
    "archive",
    "outbox",
    "live_results",
];

/// Derive `arena_block_size` from a write buffer size:
/// `write_buffer_size / 64`, clamped to [64 KiB, 1 MiB].
fn arena_block_size_for(write_buffer_size: usize) -> usize {
    (write_buffer_size / 64).clamp(64 * 1024, 1024 * 1024)
}

/// Apply the per-CF sizing policy (write buffer, arena block, history bound)
/// to `opts` for the column family named `cf_name`, then return `opts`.
///
/// Use this wherever a column family's options are materialized, both the
/// open-time descriptors and the drop/re-create paths, so no CF can be
/// created without the policy applied.
pub(crate) fn sized(cf_name: &str, mut opts: Options, index_opts: &RocksIndexOptions) -> Options {
    let write_buffer_size = if LARGE_BUFFER_CFS.contains(&cf_name) {
        index_opts.large_write_buffer_size
    } else {
        index_opts.small_write_buffer_size
    };
    opts.set_write_buffer_size(write_buffer_size);
    opts.set_arena_block_size(arena_block_size_for(write_buffer_size));
    crate::bound_write_buffer_history(&mut opts);
    opts
}

/// Build a [`ColumnFamilyDescriptor`] for `cf_name` from base options with
/// the sizing policy applied.
pub(crate) fn descriptor(
    cf_name: &str,
    base: Options,
    index_opts: &RocksIndexOptions,
) -> ColumnFamilyDescriptor {
    ColumnFamilyDescriptor::new(cf_name, sized(cf_name, base, index_opts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_block_size_derivation_is_clamped() {
        // 16 MiB / 64 = 256 KiB, inside the clamp range.
        assert_eq!(
            arena_block_size_for(DEFAULT_LARGE_WRITE_BUFFER_SIZE),
            256 * 1024
        );
        // 8 MiB / 64 = 128 KiB, inside the clamp range.
        assert_eq!(
            arena_block_size_for(DEFAULT_SMALL_WRITE_BUFFER_SIZE),
            128 * 1024
        );
        // Tiny buffers clamp up to 64 KiB.
        assert_eq!(arena_block_size_for(1024 * 1024), 64 * 1024);
        // Huge buffers clamp down to 1 MiB.
        assert_eq!(arena_block_size_for(256 * 1024 * 1024), 1024 * 1024);
    }

    #[test]
    fn large_buffer_set_is_exactly_the_seven_high_volume_cfs() {
        assert_eq!(
            LARGE_BUFFER_CFS,
            &[
                "elements",
                "inbound",
                "outbound",
                "values",
                "archive",
                "outbox",
                "live_results"
            ]
        );
    }
}
