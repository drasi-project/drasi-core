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
//! arena's 2 KiB inline block allocates a full arena block. At the default
//! 128 MiB shared budget, sizing both explicitly cuts the touched per-query
//! floor roughly 5x and halves the memtable ceiling under load, at throughput
//! parity (see issue #692 for measurements). Larger budgets trade some of
//! those savings for fewer self-triggered flushes.
//!
//! Two buffer sizes are derived from the shared write-buffer manager's budget
//! and assigned by per-source-change byte volume. The large tier is one eighth
//! of the budget, clamped to 16-64 MiB; the small tier is half the large tier.
//! The default 128 MiB budget therefore preserves the original 16/8 MiB sizes,
//! while a larger budget lets busy column families build larger memtables
//! before self-triggering a flush:
//!
//! - **Large (16-64 MiB)**: `elements` (full element blob per change),
//!   `inbound` / `outbound` (adjacency entries per relation change), `values`
//!   (aggregation accumulators), `archive` (a full element version appended per
//!   update when enabled), `outbox` and `live_results` (a serialized result
//!   diff / row set update per result-producing change).
//! - **Small (8-32 MiB)**: everything else: same write *frequency* in some
//!   cases (`slots` is written per element upsert) but tiny values, or
//!   genuinely quiet CFs (`metadata`, `fqueue`, `findex`, `stream_state`,
//!   `partial`, `sorted-sets`, and the unused `default` CF).
//!
//! `arena_block_size` is derived, not configurable: `write_buffer_size / 64`,
//! clamped to [64 KiB, 1 MiB]. Left unset, RocksDB sanitizes it to
//! `min(1 MiB, write_buffer_size / 8)`, which puts a 1 MiB floor under every
//! CF that sees any real write. At the default budget, the derived value keeps
//! barely-active CFs at 128-256 KiB instead; at the maximum tier sizes those
//! blocks grow to 512 KiB for small CFs and 1 MiB for large CFs. Allocations
//! larger than a quarter block (e.g. the memtable bloom filters) bypass the
//! block pool as exact-size allocations, so small blocks waste nothing.
//!
//! Shared cache/table configuration and the flushed-memtable history bound are
//! applied by [`crate::cf_options`] before this module overlays the per-CF
//! write-buffer and arena sizing. The provider-wide write-buffer manager
//! remains the aggregate backstop across query databases.

use rocksdb::{ColumnFamilyDescriptor, Options};

use crate::RocksIndexOptions;

const MIB: usize = 1024 * 1024;

/// Floor for high-volume column families, preserving the original #692 size
/// when the shared write-buffer budget is 128 MiB or smaller.
const MIN_LARGE_WRITE_BUFFER_SIZE: usize = 16 * MIB;

/// Ceiling for high-volume column families, limiting flush bursts and the
/// eagerly allocated memtable bloom filters that scale with this value.
const MAX_LARGE_WRITE_BUFFER_SIZE: usize = 64 * MIB;

const LARGE_BUFFER_BUDGET_DIVISOR: usize = 8;
const SMALL_BUFFER_DIVISOR: usize = 2;

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

/// Derive the large and small per-CF write-buffer tiers from the aggregate
/// write-buffer budget shared across query databases.
pub(crate) fn write_buffer_sizes_for_budget(write_buffer_budget: usize) -> (usize, usize) {
    let large_write_buffer_size = (write_buffer_budget / LARGE_BUFFER_BUDGET_DIVISOR)
        .clamp(MIN_LARGE_WRITE_BUFFER_SIZE, MAX_LARGE_WRITE_BUFFER_SIZE);
    let small_write_buffer_size = large_write_buffer_size / SMALL_BUFFER_DIVISOR;
    (large_write_buffer_size, small_write_buffer_size)
}

/// Derive `arena_block_size` from a write buffer size:
/// `write_buffer_size / 64`, clamped to [64 KiB, 1 MiB].
fn arena_block_size_for(write_buffer_size: usize) -> usize {
    (write_buffer_size / 64).clamp(64 * 1024, 1024 * 1024)
}

/// Apply the per-CF write-buffer and arena sizing to `opts` for the column
/// family named `cf_name`, then return `opts`.
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
    fn write_buffer_sizes_scale_with_budget_within_bounds() {
        assert_eq!(write_buffer_sizes_for_budget(64 * MIB), (16 * MIB, 8 * MIB));
        assert_eq!(
            write_buffer_sizes_for_budget(128 * MIB),
            (16 * MIB, 8 * MIB)
        );
        assert_eq!(
            write_buffer_sizes_for_budget(256 * MIB),
            (32 * MIB, 16 * MIB)
        );
        assert_eq!(
            write_buffer_sizes_for_budget(512 * MIB),
            (64 * MIB, 32 * MIB)
        );
        assert_eq!(
            write_buffer_sizes_for_budget(1024 * MIB),
            (64 * MIB, 32 * MIB)
        );
    }

    #[test]
    fn arena_block_size_derivation_is_clamped() {
        // 16 MiB / 64 = 256 KiB, inside the clamp range.
        assert_eq!(
            arena_block_size_for(MIN_LARGE_WRITE_BUFFER_SIZE),
            256 * 1024
        );
        // 8 MiB / 64 = 128 KiB, inside the clamp range.
        assert_eq!(
            arena_block_size_for(MIN_LARGE_WRITE_BUFFER_SIZE / SMALL_BUFFER_DIVISOR),
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
