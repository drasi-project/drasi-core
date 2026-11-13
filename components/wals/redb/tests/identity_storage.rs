// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
use drasi_core_types::{ComponentId, StorageScope};
use drasi_lib::{WalProvider, WriteAheadLogConfig};
use drasi_wal_redb::RedbWalProvider;

fn owner() -> ComponentId {
    ComponentId::try_new("00000000-0000-0000-0000-000000000001").expect("valid owner UUID")
}

fn change() -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("business-data", "item"),
            labels: Arc::from(vec![Arc::from("Item")]),
            effective_from: 1,
        },
    }
}

#[tokio::test]
async fn scope_is_not_an_id_and_reopen_preserves_each_log() {
    let directory = tempfile::tempdir().unwrap();
    let first = StorageScope::new("a").nested(&StorageScope::new("b"));
    let second = StorageScope::new("a/b");
    let long = StorageScope::new("scope".repeat(1000));
    let provider = RedbWalProvider::new(directory.path());
    for scope in [&first, &second, &long] {
        provider
            .register(scope, &owner(), WriteAheadLogConfig::default())
            .await
            .unwrap();
    }
    assert_eq!(
        provider.append(&first, &owner(), &change()).await.unwrap(),
        1
    );
    assert_eq!(provider.head_sequence(&second, &owner()).await.unwrap(), 0);
    assert_eq!(
        provider.append(&long, &owner(), &change()).await.unwrap(),
        1
    );
    drop(provider);
    let provider = RedbWalProvider::new(directory.path());
    for (scope, count) in [(&first, 1), (&second, 0), (&long, 1)] {
        provider
            .register(scope, &owner(), WriteAheadLogConfig::default())
            .await
            .unwrap();
        assert_eq!(
            provider.read_from(scope, &owner(), 1).await.unwrap().len(),
            count
        );
    }
}

#[tokio::test]
async fn old_root_is_rejected_before_a_new_namespace_is_created() {
    let directory = tempfile::tempdir().unwrap();
    let old = directory.path().join("old-scope-source.redb");
    std::fs::write(&old, b"preserved old data").unwrap();
    let provider = RedbWalProvider::new(directory.path());
    let error = provider
        .register(
            &StorageScope::new("new-scope"),
            &owner(),
            WriteAheadLogConfig::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unversioned WAL storage"));
    assert_eq!(std::fs::read(&old).unwrap(), b"preserved old data");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn a_valid_database_cannot_be_rebound_to_another_component() {
    let directory = tempfile::tempdir().unwrap();
    let scope = StorageScope::default();
    let provider = RedbWalProvider::new(directory.path());
    provider
        .register(&scope, &owner(), WriteAheadLogConfig::default())
        .await
        .unwrap();
    provider.append(&scope, &owner(), &change()).await.unwrap();
    drop(provider);
    let other = ComponentId::try_new("00000000-0000-0000-0000-000000000002").unwrap();
    std::fs::rename(
        directory.path().join(format!("{}.redb", owner())),
        directory.path().join(format!("{other}.redb")),
    )
    .unwrap();
    let provider = RedbWalProvider::new(directory.path());
    let error = provider
        .register(&scope, &other, WriteAheadLogConfig::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("WAL owner does not match"));
}
