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

//! Regression tests for the point-lookup column family options.
//!
//! `src/point_lookup.rs` unrolls RocksDB's `optimize_for_point_lookup` preset
//! into explicit settings. Dropping one of them changes no functional
//! behavior and fails no functional test; the cost is a silent point-read
//! regression. These tests read the effective (post-sanitization) values back
//! from the OPTIONS file RocksDB writes at open, so a dropped setting fails
//! here instead.
//!
//! Two of the unrolled settings cannot be checked this way and are held by
//! review against the upstream preset instead:
//!
//! - The bloom policy serializes as `bloomfilter` with no bits-per-key. The
//!   preset, which calls `NewBloomFilterPolicy(10)` directly, serializes as
//!   `bloomfilter:10:false`; going through the C API wraps the policy and the
//!   wrapper's name omits the parameters. Presence is still asserted, which
//!   catches the filter being dropped entirely.
//! - The block cache is not written to the OPTIONS file at all, in any form.

use std::collections::{BTreeSet, HashMap};

use drasi_index_rocksdb::open_unified_db;
use drasi_index_rocksdb::RocksIndexOptions;

/// Column families read only by exact key, which must carry the full
/// point-lookup policy.
const POINT_LOOKUP_CFS: &[&str] = &["elements", "slots", "values", "metadata"];

/// Representative scan-oriented column families, which must not. These are the
/// negative control: without them a test that asserted the policy everywhere,
/// or nowhere, would still pass.
const SCAN_CFS: &[&str] = &["inbound", "partial", "sorted-sets"];

/// `section -> key -> value` from the newest OPTIONS-* file of the DB at
/// `db_dir`. Sections are kept verbatim, e.g. `[CFOptions "elements"]` and
/// `[TableOptions/BlockBasedTable "elements"]`.
fn parse_options_file(db_dir: &std::path::Path) -> HashMap<String, HashMap<String, String>> {
    let options_file = std::fs::read_dir(db_dir)
        .expect("read db dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("OPTIONS-"))
        .max_by_key(|e| e.file_name().to_string_lossy().to_string())
        .expect("OPTIONS file present");
    let text = std::fs::read_to_string(options_file.path()).expect("read OPTIONS");

    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            current = line.to_string();
        } else if let Some((key, value)) = line.split_once('=') {
            if !current.is_empty() {
                sections
                    .entry(current.clone())
                    .or_default()
                    .insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    sections
}

fn value<'a>(
    sections: &'a HashMap<String, HashMap<String, String>>,
    section_prefix: &str,
    cf: &str,
    key: &str,
) -> &'a str {
    sections
        .get(&format!("[{section_prefix} \"{cf}\"]"))
        .unwrap_or_else(|| panic!("no [{section_prefix} \"{cf}\"] section in OPTIONS"))
        .get(key)
        .unwrap_or_else(|| panic!("no {key} for CF '{cf}'"))
        .as_str()
}

fn open_and_parse(dir: &tempfile::TempDir, name: &str) -> HashMap<String, HashMap<String, String>> {
    let options = RocksIndexOptions::new(true, false);
    let db = open_unified_db(dir.path().to_str().expect("utf-8 path"), name, &options)
        .expect("open unified db");
    let sections = parse_options_file(&dir.path().join(name));
    drop(db);
    sections
}

#[test]
fn point_lookup_cfs_carry_every_unrolled_setting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sections = open_and_parse(&dir, "point-lookup-test");

    for cf in POINT_LOOKUP_CFS {
        // Memtable whole-key bloom, sized at 2% of write_buffer_size and
        // allocated when the memtable is constructed.
        assert_eq!(
            value(
                &sections,
                "CFOptions",
                cf,
                "memtable_prefix_bloom_size_ratio"
            ),
            "0.020000",
            "CF '{cf}' lost the memtable bloom ratio"
        );
        assert_eq!(
            value(&sections, "CFOptions", cf, "memtable_whole_key_filtering"),
            "true",
            "CF '{cf}' lost memtable whole-key filtering"
        );
        // Hash index appended to each data block.
        assert_eq!(
            value(
                &sections,
                "TableOptions/BlockBasedTable",
                cf,
                "data_block_index_type"
            ),
            "kDataBlockBinaryAndHash",
            "CF '{cf}' lost the in-block hash index"
        );
        assert_eq!(
            value(
                &sections,
                "TableOptions/BlockBasedTable",
                cf,
                "data_block_hash_table_util_ratio"
            ),
            "0.750000",
            "CF '{cf}' has an unexpected data block hash ratio"
        );
        // Presence only: the serialized name carries no bits-per-key. See the
        // module comment.
        assert_eq!(
            value(
                &sections,
                "TableOptions/BlockBasedTable",
                cf,
                "filter_policy"
            ),
            "bloomfilter",
            "CF '{cf}' lost the SST bloom filter"
        );
    }
}

#[test]
fn scan_cfs_do_not_carry_the_point_lookup_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sections = open_and_parse(&dir, "scan-control-test");

    for cf in SCAN_CFS {
        assert_eq!(
            value(
                &sections,
                "CFOptions",
                cf,
                "memtable_prefix_bloom_size_ratio"
            ),
            "0.000000",
            "CF '{cf}' unexpectedly has a memtable bloom"
        );
        assert_eq!(
            value(&sections, "CFOptions", cf, "memtable_whole_key_filtering"),
            "false",
            "CF '{cf}' unexpectedly has memtable whole-key filtering"
        );
        assert_eq!(
            value(
                &sections,
                "TableOptions/BlockBasedTable",
                cf,
                "data_block_index_type"
            ),
            "kDataBlockBinarySearch",
            "CF '{cf}' unexpectedly has the in-block hash index"
        );
        assert_eq!(
            value(
                &sections,
                "TableOptions/BlockBasedTable",
                cf,
                "filter_policy"
            ),
            "nullptr",
            "CF '{cf}' unexpectedly has an SST bloom filter"
        );
    }
}

/// Pins the membership of the policy, not just its contents: exactly the four
/// point-lookup column families carry it. Catches the policy being applied to
/// a new CF, or a CF being added to `POINT_LOOKUP_CFS` without the options
/// actually reaching it.
#[test]
fn exactly_the_point_lookup_cfs_carry_the_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sections = open_and_parse(&dir, "membership-test");

    let mut with_policy = BTreeSet::new();
    for (section, keys) in &sections {
        let Some(rest) = section.strip_prefix("[CFOptions \"") else {
            continue;
        };
        let Some(cf) = rest.strip_suffix("\"]") else {
            continue;
        };
        if keys.get("memtable_whole_key_filtering").map(String::as_str) == Some("true") {
            with_policy.insert(cf.to_string());
        }
    }

    let expected: BTreeSet<String> = POINT_LOOKUP_CFS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        with_policy, expected,
        "the set of CFs carrying the point-lookup policy changed"
    );
}
