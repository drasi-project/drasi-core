// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_core_types::{ComponentId, StorageScope, StorePartition};
use drasi_lib::StateStoreProvider;
use drasi_state_store_redb::RedbStateStoreProvider;

fn owner() -> ComponentId {
    ComponentId::try_new("00000000-0000-0000-0000-000000000001").expect("valid owner UUID")
}

#[tokio::test]
async fn typed_partitions_remain_distinct_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.redb");
    let component = StorePartition::component(owner());
    let named = StorePartition::named(owner().to_string());
    let scoped = component.in_scope(&StorageScope::new("instance"));
    let provider = RedbStateStoreProvider::new(&path).unwrap();
    for (partition, value) in [(&component, 1), (&named, 2), (&scoped, 3)] {
        provider.set(partition, "key", vec![value]).await.unwrap();
    }
    provider.sync().await.unwrap();
    drop(provider);
    let provider = RedbStateStoreProvider::new(&path).unwrap();
    for (partition, value) in [(&component, 1), (&named, 2), (&scoped, 3)] {
        assert_eq!(
            provider.get(partition, "key").await.unwrap(),
            Some(vec![value])
        );
    }
    provider.sync().await.unwrap();
}

#[test]
fn unversioned_state_is_rejected_without_writing_a_new_marker() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.redb");
    let old: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("store_orders");
    let marker: redb::TableDefinition<&str, u32> =
        redb::TableDefinition::new("drasi.state.identity-format");
    {
        let database = redb::Database::create(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .open_table(old)
            .unwrap()
            .insert("key", &[7u8][..])
            .unwrap();
        transaction.commit().unwrap();
    }
    let error = RedbStateStoreProvider::new(&path).unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported state identity format"));
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_read().unwrap();
    assert_eq!(
        transaction
            .open_table(old)
            .unwrap()
            .get("key")
            .unwrap()
            .unwrap()
            .value(),
        &[7]
    );
    assert!(matches!(
        transaction.open_table(marker),
        Err(redb::TableError::TableDoesNotExist(_))
    ));
}

#[test]
fn unsupported_format_is_not_adopted_as_an_empty_store() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.redb");
    let marker: redb::TableDefinition<&str, u32> =
        redb::TableDefinition::new("drasi.state.identity-format");
    {
        let database = redb::Database::create(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .open_table(marker)
            .unwrap()
            .insert("version", 1)
            .unwrap();
        transaction.commit().unwrap();
    }
    assert!(RedbStateStoreProvider::new(&path).is_err());
}
