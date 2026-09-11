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

use std::sync::Arc;

use drasi_core::interface::IndexBackendPlugin;
use drasi_index_rocksdb::RocksDbIndexProvider;
use shared_tests::query_output_transactions;

fn directory() -> tempfile::TempDir {
    std::fs::create_dir_all("test-data").expect("create test data directory");
    tempfile::Builder::new()
        .prefix("query-output-")
        .tempdir_in("test-data")
        .expect("isolated RocksDB directory")
}

macro_rules! scenario {
    ($name:ident, $scenario:ident, $($argument:expr),+) => {
        #[tokio::test]
        async fn $name() {
            let directory = directory();
            let factory = || -> Arc<dyn IndexBackendPlugin> {
                Arc::new(RocksDbIndexProvider::new(
                    directory.path(),
                    true,
                    false,
                ))
            };
            query_output_transactions::$scenario(factory, stringify!($name), $($argument),+)
                .await
                .expect(stringify!($name));
        }
    };
}

scenario!(rollback_raw_fixed, rollback_and_replay, false, false);
scenario!(rollback_raw_bounded, rollback_and_replay, false, true);
scenario!(rollback_cached_fixed, rollback_and_replay, true, false);
scenario!(rollback_cached_bounded, rollback_and_replay, true, true);
scenario!(busy_raw, busy_retains_input, false);
scenario!(busy_cached, busy_retains_input, true);
scenario!(bootstrap_raw, bootstrap_survives_reopen, false);
scenario!(bootstrap_cached, bootstrap_survives_reopen, true);
scenario!(temporal_bootstrap_raw_fixed, temporal_bootstrap_survives_reopen, false, false);
scenario!(temporal_bootstrap_raw_bounded, temporal_bootstrap_survives_reopen, false, true);
scenario!(temporal_bootstrap_cached_fixed, temporal_bootstrap_survives_reopen, true, false);
scenario!(temporal_bootstrap_cached_bounded, temporal_bootstrap_survives_reopen, true, true);
scenario!(unpublished_rows_raw, unpublished_rows_are_invisible, false);
scenario!(
    unpublished_rows_cached,
    unpublished_rows_are_invisible,
    true
);
