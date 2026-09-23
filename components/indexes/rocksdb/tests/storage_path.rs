// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use drasi_index_rocksdb::{open_unified_db, RocksDbMemoryBudget, RocksIndexOptions};

fn options() -> RocksIndexOptions {
    RocksIndexOptions::new(
        false,
        false,
        RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory budget"),
    )
}

#[test]
fn long_storage_identifiers_reopen_without_truncation_or_cross_component_aliases() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().to_str().expect("path");
    let first = format!("computation-v2-{}-first", "a".repeat(350));
    let second = format!("computation-v2-{}-second", "a".repeat(350));
    for (id, value) in [
        (&first, b"first".as_slice()),
        (&second, b"second".as_slice()),
    ] {
        let db = open_unified_db(path, id, &options()).expect("long logical ID");
        db.put(b"test-value", value).expect("persist");
    }
    for (id, value) in [
        (&first, b"first".as_slice()),
        (&second, b"second".as_slice()),
    ] {
        let db = open_unified_db(path, id, &options()).expect("reopen");
        assert_eq!(db.get(b"test-value").expect("read").as_deref(), Some(value));
    }
}

#[test]
fn existing_short_paths_and_invalid_identifier_rejections_are_preserved() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().to_str().expect("path");
    {
        let db = open_unified_db(path, "ordinary-query", &options()).expect("short name");
        db.put(b"test-value", b"old-layout").expect("persist");
    }
    assert!(directory.path().join("ordinary-query/CURRENT").exists());
    let db = open_unified_db(path, "ordinary-query", &options()).expect("existing layout");
    assert_eq!(
        db.get(b"test-value").expect("read").as_deref(),
        Some(b"old-layout".as_slice())
    );
    for invalid in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
        assert!(
            open_unified_db(path, invalid, &options()).is_err(),
            "{invalid:?}"
        );
    }
}

#[test]
fn long_unicode_identifiers_keep_their_full_identity_and_reject_an_incorrect_owner() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().to_str().expect("path");
    let identifier = "\u{754c}".repeat(200);
    {
        let db = open_unified_db(path, &identifier, &options()).expect("unicode storage");
        db.put(b"test-value", b"unicode").expect("persist");
    }
    {
        let db = open_unified_db(path, &identifier, &options()).expect("unicode reopen");
        assert_eq!(
            db.get(b"test-value").expect("read").as_deref(),
            Some(b"unicode".as_slice())
        );
        db.put(b"\0drasi:storage-identifier:v1", b"another-logical-owner")
            .expect("inject incorrect owner");
    }
    let error = open_unified_db(path, &identifier, &options())
        .err()
        .expect("owner mismatch");
    assert!(
        error
            .to_string()
            .contains("storage identifier does not match"),
        "{error}"
    );
}

#[cfg(feature = "computation")]
#[tokio::test]
async fn long_computation_graph_and_component_scopes_keep_transactional_state_on_reopen() {
    use drasi_core::computation::ComputationIndexProvider;
    use drasi_index_rocksdb::computation::RocksDbComputationProvider;

    let directory = tempfile::tempdir().expect("directory");
    let provider = RocksDbComputationProvider::new(directory.path(), options());
    let graph = format!("graph-{}", "a".repeat(250));
    let component = format!("component-{}", "b".repeat(250));
    {
        let resources = provider
            .create_indexes(&graph, &component)
            .await
            .expect("long scopes");
        resources
            .atomic_result_transaction()
            .expect("shared transaction");
        resources
            .indexes()
            .session_control
            .begin()
            .await
            .expect("begin");
        resources
            .checkpoint_store()
            .expect("checkpoint")
            .stage_checkpoint("source", 7, None)
            .await
            .expect("stage");
        resources
            .indexes()
            .session_control
            .commit()
            .await
            .expect("commit");
        resources
            .cleanup()
            .expect("owner")
            .shutdown()
            .await
            .expect("close");
    }
    let resources = provider
        .create_indexes(&graph, &component)
        .await
        .expect("reopen");
    let checkpoint = resources
        .checkpoint_store()
        .expect("checkpoint")
        .read_checkpoint("source")
        .await
        .expect("read")
        .expect("saved");
    assert_eq!(checkpoint.sequence, 7);
    resources
        .cleanup()
        .expect("owner")
        .shutdown()
        .await
        .expect("close");
}
