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

use std::collections::HashMap;
use std::sync::Arc;

use drasi_core::interface::{AccumulatorIndex, ElementArchiveIndex, ElementIndex, FutureQueue};
use drasi_index_rocksdb::{
    element_index::RocksDbElementIndex, future_queue::RocksDbFutureQueue, open_unified_db,
    result_index::RocksDbResultIndex, RocksDbMemoryBudget, RocksDbSessionState, RocksIndexOptions,
};
use rocksdb::{Options, DB};

const NON_DEFAULT_CFS: &[&str] = &[
    "elements",
    "slots",
    "inbound",
    "outbound",
    "partial",
    "archive",
    "values",
    "sorted-sets",
    "metadata",
    "fqueue",
    "findex",
    "stream_state",
    "outbox",
    "live_results",
];

const RECREATED_CFS: &[&str] = &[
    "elements",
    "slots",
    "inbound",
    "outbound",
    "partial",
    "archive",
    "values",
    "sorted-sets",
    "fqueue",
    "findex",
];

fn parse_options_file(db_dir: &std::path::Path) -> HashMap<String, HashMap<String, String>> {
    let options_file = std::fs::read_dir(db_dir)
        .expect("read db dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("OPTIONS-"))
        .max_by_key(|entry| entry.file_name().to_string_lossy().to_string())
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

fn option_value<'a>(
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

#[test]
fn shared_budget_tracks_multiple_query_databases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget =
        RocksDbMemoryBudget::new(64 * 1024 * 1024, 32 * 1024 * 1024, false).expect("budget");
    let options = RocksIndexOptions::new(true, false, budget.clone());

    let first = open_unified_db(dir.path().to_str().expect("utf-8 path"), "first", &options)
        .expect("open first database");
    let first_cf = first.cf_handle("elements").expect("elements CF");
    let cache_before_first = budget.block_cache().get_usage();
    for index in 0..2000_u32 {
        first
            .put_cf(&first_cf, format!("first-{index}"), vec![1_u8; 4096])
            .expect("write first database");
    }
    let first_usage = budget.write_buffer_manager().get_usage();
    assert!(first_usage > 0);
    assert!(budget.block_cache().get_usage() > cache_before_first);

    let second = open_unified_db(dir.path().to_str().expect("utf-8 path"), "second", &options)
        .expect("open second database");
    let second_cf = second.cf_handle("elements").expect("elements CF");
    let cache_before_second = budget.block_cache().get_usage();
    for index in 0..2000_u32 {
        second
            .put_cf(&second_cf, format!("second-{index}"), vec![2_u8; 4096])
            .expect("write second database");
    }
    assert!(budget.write_buffer_manager().get_usage() > first_usage);
    assert!(budget.block_cache().get_usage() > cache_before_second);
}

#[test]
fn memtable_writes_charge_the_shared_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget =
        RocksDbMemoryBudget::new(64 * 1024 * 1024, 32 * 1024 * 1024, false).expect("budget");
    let options = RocksIndexOptions::new(false, false, budget.clone());
    let db = open_unified_db(
        dir.path().to_str().expect("utf-8 path"),
        "memtable-charge",
        &options,
    )
    .expect("open database");
    let cf = db.cf_handle("elements").expect("elements CF");
    let usage_before = budget.block_cache().get_usage();

    for index in 0..2000_u32 {
        db.put_cf(&cf, format!("key-{index}"), vec![7_u8; 4096])
            .expect("write memtable");
    }

    assert!(budget.block_cache().get_usage() > usage_before);
}

#[test]
fn all_column_families_use_the_shared_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget =
        RocksDbMemoryBudget::new(64 * 1024 * 1024, 32 * 1024 * 1024, false).expect("budget");
    let cache = budget.block_cache().clone();
    let options = RocksIndexOptions::new(true, false, budget);
    let query_id = "cache-test";
    let query_path = dir.path().join(query_id);

    let schema_db = open_unified_db(dir.path().to_str().expect("utf-8 path"), query_id, &options)
        .expect("open database");
    drop(schema_db);

    let seed_db =
        DB::open_cf(&Options::default(), &query_path, NON_DEFAULT_CFS).expect("open seed database");
    for (index, cf_name) in std::iter::once(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
        .chain(NON_DEFAULT_CFS.iter().copied())
        .enumerate()
    {
        let cf = seed_db.cf_handle(cf_name).expect("seed CF");
        let key = format!("key-{index}");
        let value: Vec<u8> = (0..32 * 1024)
            .map(|offset| ((offset * 31 + index * 17) % 251) as u8)
            .collect();
        seed_db.put_cf(&cf, key.as_bytes(), value).expect("seed CF");
        seed_db.flush_cf(&cf).expect("flush seed CF");
    }
    drop(seed_db);

    let db = open_unified_db(dir.path().to_str().expect("utf-8 path"), query_id, &options)
        .expect("reopen database");
    for (index, cf_name) in std::iter::once(rocksdb::DEFAULT_COLUMN_FAMILY_NAME)
        .chain(NON_DEFAULT_CFS.iter().copied())
        .enumerate()
    {
        let cf = db.cf_handle(cf_name).expect("reopened CF");
        let key = format!("key-{index}");
        let usage_before = cache.get_usage();
        assert!(db
            .get_cf(&cf, key.as_bytes())
            .expect("read shared-cache CF")
            .is_some());
        assert!(
            cache.get_usage() > usage_before,
            "CF '{cf_name}' did not populate the shared cache"
        );
    }
}

#[tokio::test]
async fn clear_paths_preserve_column_family_options() {
    let dir = tempfile::tempdir().expect("tempdir");
    let budget =
        RocksDbMemoryBudget::new(64 * 1024 * 1024, 4 * 1024 * 1024, false).expect("budget");
    let mut cache = budget.block_cache().clone();
    let options = RocksIndexOptions::new(true, false, budget.clone());
    let query_id = "clear-test";
    let db = open_unified_db(dir.path().to_str().expect("utf-8 path"), query_id, &options)
        .expect("open database");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));

    let element_index =
        RocksDbElementIndex::new(db.clone(), options.clone(), session_state.clone());
    let result_index = RocksDbResultIndex::new(db.clone(), session_state.clone(), options.clone());
    let future_queue = RocksDbFutureQueue::new(db.clone(), session_state, options.clone());

    ElementIndex::clear(&element_index)
        .await
        .expect("clear element index");
    ElementArchiveIndex::clear(&element_index)
        .await
        .expect("clear archive index");
    AccumulatorIndex::clear(&result_index)
        .await
        .expect("clear result index");
    FutureQueue::clear(&future_queue)
        .await
        .expect("clear future queue");

    let write_usage_before = budget.write_buffer_manager().get_usage();
    for (cf_index, cf_name) in RECREATED_CFS.iter().copied().enumerate() {
        let cf = db.cf_handle(cf_name).expect("recreated CF");
        for entry in 0..2000_u32 {
            let key = format!("{cf_name}-{entry}");
            let value: Vec<u8> = (0..4096)
                .map(|offset| ((offset * 31 + cf_index * 17 + entry as usize) % 251) as u8)
                .collect();
            db.put_cf(&cf, key.as_bytes(), &value)
                .expect("write recreated CF");
        }
    }
    assert!(budget.write_buffer_manager().get_usage() > write_usage_before);

    drop(element_index);
    drop(result_index);
    drop(future_queue);
    drop(db);

    let query_path = dir.path().join(query_id);
    let flush_db = DB::open_cf(&Options::default(), &query_path, NON_DEFAULT_CFS)
        .expect("open flush database");
    for cf_name in
        std::iter::once(rocksdb::DEFAULT_COLUMN_FAMILY_NAME).chain(NON_DEFAULT_CFS.iter().copied())
    {
        let cf = flush_db.cf_handle(cf_name).expect("flush CF");
        flush_db.flush_cf(&cf).expect("flush CF");
    }
    drop(flush_db);

    let db = open_unified_db(dir.path().to_str().expect("utf-8 path"), query_id, &options)
        .expect("reopen database");

    let sections = parse_options_file(&query_path);
    for cf_name in RECREATED_CFS {
        assert_eq!(
            option_value(
                &sections,
                "CFOptions",
                cf_name,
                "max_write_buffer_size_to_maintain"
            ),
            "1048576",
            "CF '{cf_name}' lost the write-buffer history bound"
        );
        assert_eq!(
            option_value(
                &sections,
                "TableOptions/BlockBasedTable",
                cf_name,
                "cache_index_and_filter_blocks"
            ),
            "true",
            "CF '{cf_name}' leaves index/filter blocks outside the shared cache"
        );
    }
    for cf_name in ["elements", "slots", "values"] {
        assert_eq!(
            option_value(
                &sections,
                "CFOptions",
                cf_name,
                "memtable_whole_key_filtering"
            ),
            "true",
            "CF '{cf_name}' lost the point-lookup policy"
        );
    }

    for cf_name in RECREATED_CFS {
        let cf = db.cf_handle(cf_name).expect("recreated CF");
        let key = format!("{cf_name}-0");
        cache.set_capacity(0);
        cache.set_capacity(64 * 1024 * 1024);
        let usage_before = cache.get_usage();
        assert!(db
            .get_cf(&cf, key.as_bytes())
            .expect("read recreated CF")
            .is_some());
        assert!(
            cache.get_usage() > usage_before,
            "CF '{cf_name}' did not populate the shared cache"
        );
    }
}
