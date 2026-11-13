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

use drasi_core_types::{ComponentId, StorageScope};
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
    let first = StorageScope::new(format!("scope-{}-first", "a".repeat(350)));
    let second = StorageScope::new(format!("scope-{}-second", "a".repeat(350)));
    let component = ComponentId::generate();
    let other = ComponentId::generate();
    for (scope, owner, value) in [
        (&first, &component, b"first".as_slice()),
        (&second, &component, b"second".as_slice()),
        (&first, &other, b"other owner".as_slice()),
    ] {
        let db = open_unified_db(path, scope, owner, &options()).expect("long storage scope");
        db.put(b"test-value", value).expect("persist");
    }
    for (scope, owner, value) in [
        (&first, &component, b"first".as_slice()),
        (&second, &component, b"second".as_slice()),
        (&first, &other, b"other owner".as_slice()),
    ] {
        let db = open_unified_db(path, scope, owner, &options()).expect("reopen");
        assert_eq!(db.get(b"test-value").expect("read").as_deref(), Some(value));
    }
    let paths = std::fs::read_dir(directory.path().join(".drasi-long-identifiers-v1"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(paths.len(), 3);
    for entry in paths {
        let name = entry.file_name().into_string().unwrap();
        assert_eq!(name.len(), 64);
        assert!(name.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[test]
fn uuid_paths_reopen_and_malformed_owners_are_rejected_at_the_boundary() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().to_str().expect("path");
    let owner = ComponentId::generate();
    {
        let db = open_unified_db(path, &StorageScope::default(), &owner, &options())
            .expect("UUID owner");
        db.put(b"test-value", b"current-layout").expect("persist");
    }
    assert!(directory
        .path()
        .join(owner.as_str())
        .join("CURRENT")
        .exists());
    let db = open_unified_db(path, &StorageScope::default(), &owner, &options())
        .expect("existing UUID layout");
    assert_eq!(
        db.get(b"test-value").expect("read").as_deref(),
        Some(b"current-layout".as_slice())
    );
    for invalid in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
        assert!(ComponentId::try_new(invalid).is_err(), "{invalid:?}");
    }
}

#[test]
fn long_unicode_identifiers_keep_their_full_identity_and_reject_an_incorrect_owner() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().to_str().expect("path");
    let scope = StorageScope::new("\u{754c}".repeat(200));
    let owner = ComponentId::generate();
    {
        let db = open_unified_db(path, &scope, &owner, &options()).expect("unicode storage");
        db.put(b"test-value", b"unicode").expect("persist");
    }
    {
        let db = open_unified_db(path, &scope, &owner, &options()).expect("unicode reopen");
        assert_eq!(
            db.get(b"test-value").expect("read").as_deref(),
            Some(b"unicode".as_slice())
        );
        db.put(
            b"\0drasi:storage-identity-format",
            serde_json::to_vec(&(2, &scope, ComponentId::generate())).unwrap(),
        )
        .expect("inject incorrect owner");
    }
    let error = open_unified_db(path, &scope, &owner, &options())
        .err()
        .expect("owner mismatch");
    assert!(
        error
            .to_string()
            .contains("identity format or owner does not match"),
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
    let component = ComponentId::generate();
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
            .stage_checkpoint(
                &drasi_core_types::CheckpointKey::Source(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                ),
                7,
                None,
            )
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
        .read_checkpoint(&drasi_core_types::CheckpointKey::Source(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
        ))
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
