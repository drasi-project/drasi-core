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

use rocksdb::{BlockBasedOptions, Cache, Options};

pub(crate) fn shared_block_based_options(block_cache: &Cache) -> BlockBasedOptions {
    let mut table_options = BlockBasedOptions::default();
    table_options.set_block_cache(block_cache);
    // Otherwise each table reader keeps SST index and filter blocks outside
    // the shared cache, so their memory grows independently of the budget.
    table_options.set_cache_index_and_filter_blocks(true);
    table_options
}

pub(crate) fn options_with_table(table_options: &BlockBasedOptions) -> Options {
    let mut options = Options::default();
    crate::bound_write_buffer_history(&mut options);
    options.set_block_based_table_factory(table_options);
    options
}

pub(crate) fn base_cf_options(block_cache: &Cache) -> Options {
    let table_options = shared_block_based_options(block_cache);
    options_with_table(&table_options)
}
