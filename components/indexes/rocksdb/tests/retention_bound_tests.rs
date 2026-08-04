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

//! Regression test for the flushed-memtable history bound.
//!
//! `max_write_buffer_size_to_maintain` must be explicitly nonzero on every
//! column family: a zero is sanitized by RocksDB back to a large default
//! (128 MiB per CF observed), and retained memtables count against process
//! memory after every flush. The effective (post-sanitization) values are
//! read back from the OPTIONS file RocksDB writes at open, so this catches
//! both a reintroduced zero and a column family the bound was never applied
//! to.

use drasi_index_rocksdb::element_index::RocksIndexOptions;
use drasi_index_rocksdb::open_unified_db;

#[test]
fn effective_retention_is_the_explicit_bound_on_every_cf() {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = RocksIndexOptions {
        archive_enabled: true,
        direct_io: false,
    };
    let db =
        open_unified_db(dir.path().to_str().unwrap(), "retention-test", &options).expect("open db");

    // RocksDB persists the effective options at open; read the newest
    // OPTIONS-* file from the DB directory.
    let db_dir = dir.path().join("retention-test");
    let options_file = std::fs::read_dir(&db_dir)
        .expect("read db dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("OPTIONS-"))
        .max_by_key(|e| e.file_name().to_string_lossy().to_string())
        .expect("OPTIONS file present");
    let text = std::fs::read_to_string(options_file.path()).expect("read OPTIONS");

    let mut cf_count = 0usize;
    let mut current_cf: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[CFOptions \"") {
            current_cf = rest.strip_suffix("\"]").map(str::to_string);
            cf_count += 1;
        } else if let Some(value) = line.strip_prefix("max_write_buffer_size_to_maintain=") {
            let cf = current_cf.clone().unwrap_or_default();
            // 1048576 = WRITE_BUFFER_HISTORY_BYTES (1 MiB), kept crate-private;
            // update both together if the bound ever changes.
            assert_eq!(
                value, "1048576",
                "CF '{cf}' has effective retention {value}, expected the explicit bound"
            );
        }
    }
    // All index CFs plus the default CF must have been listed.
    assert!(
        cf_count >= 15,
        "expected at least 15 CFOptions sections, found {cf_count}"
    );

    drop(db);
}
