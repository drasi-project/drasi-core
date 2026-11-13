// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_core::interface::IndexBackendPlugin;
use drasi_core_types::{ComponentId, OutputPartition, StorageScope};
use drasi_index_rocksdb::RocksDbIndexProvider;

fn owner() -> ComponentId {
    ComponentId::try_new("00000000-0000-0000-0000-000000000001").expect("valid owner UUID")
}

#[tokio::test]
async fn scoped_outputs_reopen_without_aliasing_other_domains() {
    let directory = tempfile::tempdir().unwrap();
    let provider = RocksDbIndexProvider::new(directory.path(), false, false);
    let scope = StorageScope::new("one").nested(&StorageScope::new("two"));
    let other_scope = StorageScope::new("one/two");
    let partitions = [
        OutputPartition::Component(owner()),
        OutputPartition::Journal(owner().to_string()),
        OutputPartition::Component(owner()).metadata("state"),
    ];
    let indexes = provider
        .create_scoped_indexes(&scope, &owner())
        .await
        .unwrap();
    for (index, partition) in partitions.iter().enumerate() {
        indexes
            .checkpoint_store
            .as_ref()
            .unwrap()
            .write_result_sequence(partition, index as u64 + 1)
            .await
            .unwrap();
    }
    drop(indexes);
    let indexes = provider
        .create_scoped_indexes(&scope, &owner())
        .await
        .unwrap();
    for (index, partition) in partitions.iter().enumerate() {
        assert_eq!(
            indexes
                .checkpoint_store
                .as_ref()
                .unwrap()
                .read_result_sequence(partition)
                .await
                .unwrap(),
            Some(index as u64 + 1),
        );
    }
    let other = provider
        .create_scoped_indexes(&other_scope, &owner())
        .await
        .unwrap();
    assert_eq!(
        other
            .checkpoint_store
            .as_ref()
            .unwrap()
            .read_result_sequence(&partitions[0])
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn unversioned_nested_storage_is_rejected_before_new_scope_creation() {
    let directory = tempfile::tempdir().unwrap();
    let old = directory
        .path()
        .join("computation-v1")
        .join("old-graph")
        .join("old-query");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("CURRENT"), b"preserved old manifest").unwrap();
    let provider = RocksDbIndexProvider::new(directory.path(), false, false);
    let Err(error) = provider
        .create_scoped_indexes(&StorageScope::new("new-scope"), &owner())
        .await
    else {
        panic!("old storage must reject");
    };
    assert!(error.to_string().contains("unversioned RocksDB storage"));
    assert_eq!(
        std::fs::read(old.join("CURRENT")).unwrap(),
        b"preserved old manifest"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn database_owner_cannot_change_when_its_directory_is_renamed() {
    let directory = tempfile::tempdir().unwrap();
    let provider = RocksDbIndexProvider::new(directory.path(), false, false);
    drop(provider.create_indexes(&owner()).await.unwrap());
    let other = ComponentId::try_new("00000000-0000-0000-0000-000000000002").unwrap();
    std::fs::rename(
        directory.path().join(owner().as_str()),
        directory.path().join(other.as_str()),
    )
    .unwrap();
    let Err(error) = provider.create_indexes(&other).await else {
        panic!("different owner must reject");
    };
    assert!(error
        .to_string()
        .contains("identity format or owner does not match"));
}

#[tokio::test]
async fn long_structured_scopes_use_bounded_paths_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let provider = RocksDbIndexProvider::new(directory.path(), false, false);
    let scope = StorageScope::new("namespace".repeat(1000));
    let partition = OutputPartition::Component(owner());
    let indexes = provider
        .create_scoped_indexes(&scope, &owner())
        .await
        .unwrap();
    indexes
        .checkpoint_store
        .as_ref()
        .unwrap()
        .write_result_sequence(&partition, 41)
        .await
        .unwrap();
    drop(indexes);
    let indexes = provider
        .create_scoped_indexes(&scope, &owner())
        .await
        .unwrap();
    assert_eq!(
        indexes
            .checkpoint_store
            .as_ref()
            .unwrap()
            .read_result_sequence(&partition)
            .await
            .unwrap(),
        Some(41)
    );
}
