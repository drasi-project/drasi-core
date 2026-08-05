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

//! Column-family options for point-lookup access.
//!
//! Four column families are read only by exact key: `elements` and `slots`
//! (16-byte element-reference hashes) and `values` and `metadata` (`u64` set
//! ids). None of them has a prefix extractor, and none is ever scanned.
//!
//! They previously took their options from RocksDB's
//! `Options::optimize_for_point_lookup` convenience preset. The preset is
//! unrolled here so every setting it bundles is explicit and reviewable, and
//! so the block cache is a handle this crate constructs rather than one the
//! preset builds internally from a size argument. The preset's signature
//! accepts only a size, which is what prevents a shared cache from being
//! injected through it (see #634).
//!
//! The settings below mirror `ColumnFamilyOptions::OptimizeForPointLookup`
//! in RocksDB 8.1.1 (`options/options.cc:609`), which is the version pinned
//! by `librocksdb-sys 0.11.0+8.1.1`:
//!
//! | `OptimizeForPointLookup(mb)` | Here |
//! |---|---|
//! | `data_block_index_type = kDataBlockBinaryAndHash` | [`DataBlockIndexType::BinaryAndHash`] |
//! | `data_block_hash_table_util_ratio = 0.75` | [`DATA_BLOCK_HASH_RATIO`] |
//! | `filter_policy = NewBloomFilterPolicy(10)` | [`BLOOM_BITS_PER_KEY`] |
//! | `block_cache = NewLRUCache(mb * 1024 * 1024)` | `block_cache_bytes` argument |
//! | `memtable_prefix_bloom_size_ratio = 0.02` | [`MEMTABLE_BLOOM_RATIO`] |
//! | `memtable_whole_key_filtering = true` | set unconditionally |
//!
//! The preset also replaces the whole table factory rather than merging into
//! it, so unrolling it is only equivalence-preserving as long as these column
//! families set no other block-based table options. They set none today; a
//! future one must be added to [`point_lookup_cf_options`] rather than layered
//! on at the call site, or it will be silently dropped.
//!
//! Two of these values are not recoverable from the OPTIONS file RocksDB
//! writes at open, so `point_lookup_options_tests` cannot pin them and they
//! are held by this module instead: the bloom policy serializes as
//! `bloomfilter` without its bits-per-key (the C API wraps the policy and the
//! wrapper's name omits the parameters), and the block cache is not
//! serialized at all.

use rocksdb::{BlockBasedOptions, Cache, DataBlockIndexType, Options};

/// Bits per key for the SST bloom filter, matching `NewBloomFilterPolicy(10)`.
const BLOOM_BITS_PER_KEY: f64 = 10.0;

/// Memtable bloom filter size, as a fraction of `write_buffer_size`. Allocated
/// eagerly when the memtable is constructed, so it is a fixed per-CF floor.
const MEMTABLE_BLOOM_RATIO: f64 = 0.02;

/// Target utilization of the hash table appended to each data block.
const DATA_BLOCK_HASH_RATIO: f64 = 0.75;

/// Options for a column family read only by exact key.
///
/// `block_cache_bytes` is in **bytes**, unlike the megabytes taken by the
/// `optimize_for_point_lookup` preset this replaces.
///
/// Each call constructs its own [`Cache`], so two column families built from
/// this function get independent caches of the given size, matching the
/// per-call cache the preset created. #634 replaces this with one shared
/// handle passed in; until then, hoisting a single `Cache` into a shared
/// value here would quietly halve the capacity available to `elements` and
/// `slots`, and nothing in the OPTIONS file would show it.
pub(crate) fn point_lookup_cf_options(block_cache_bytes: usize) -> Options {
    let mut opts = Options::default();

    let mut table_opts = BlockBasedOptions::default();
    table_opts.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);
    table_opts.set_data_block_hash_ratio(DATA_BLOCK_HASH_RATIO);
    // `false` selects the full-filter builder. On RocksDB 8.1.1 the flag is
    // ignored (`NewBloomFilterPolicy`'s second parameter is named
    // `IGNORED_use_block_based_builder`), but the block-based builder is the
    // one that would be wrong if it were ever honoured again.
    table_opts.set_bloom_filter(BLOOM_BITS_PER_KEY, false);
    table_opts.set_block_cache(&Cache::new_lru_cache(block_cache_bytes));
    opts.set_block_based_table_factory(&table_opts);

    opts.set_memtable_prefix_bloom_ratio(MEMTABLE_BLOOM_RATIO);
    opts.set_memtable_whole_key_filtering(true);

    opts
}
