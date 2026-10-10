// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use redb::ReadableTableMetadata;

fn options(capacity: usize) -> ConfigurationStoreOptions {
    ConfigurationStoreOptions {
        receipt_batch_capacity: NonZeroUsize::new(capacity),
    }
}

fn changed() -> DesiredInstance {
    let mut desired = DesiredInstance::default();
    desired.topology.allow_incomplete = !desired.topology.allow_incomplete;
    desired
}

fn expired<T>(result: Result<T>) {
    let error = result.err().expect("expired retry must reject");
    assert!(
        matches!(
            error.downcast_ref::<ManagementError>(),
            Some(ManagementError::RequestExpired)
        ),
        "{error:#}"
    );
}

#[tokio::test]
async fn generated_batches_bound_receipts_and_reject_expired_ids_across_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("configuration.redb");
    let (first, latest);
    {
        let store = RedbConfigurationStore::new_with_options(&path, [11; 32], options(2))?;
        let a = store.open("a").await?;
        let b = store.open("b").await?;
        first = a.new_request_id().await?;
        let second = a.new_request_id().await?;
        let unused = a.new_request_id().await?;
        let foreign = b.new_request_id().await?;
        b.commit(0, &foreign, &DesiredInstance::default()).await?;
        expired(a.commit(0, &foreign, &DesiredInstance::default()).await);
        let receipt = a.commit(0, &first, &DesiredInstance::default()).await?;
        assert_eq!(
            a.commit(u64::MAX, &first, &DesiredInstance::default())
                .await?,
            receipt
        );
        assert!(matches!(
            a.commit(0, &first, &changed())
                .await
                .err()
                .unwrap()
                .downcast_ref::<ManagementError>(),
            Some(ManagementError::RequestConflict)
        ));
        a.commit(0, &second, &changed()).await?;
        assert!(matches!(
            a.commit(1, &unused, &changed())
                .await
                .err()
                .unwrap()
                .downcast_ref::<ManagementError>(),
            Some(ManagementError::RequestBatchFull)
        ));
        latest = a.new_request_id().await?;
        expired(a.receipt(&first).await);
        expired(a.commit(1, &first, &changed()).await);
        expired(a.commit(0, &first, &DesiredInstance::default()).await);
        expired(a.commit(1, &unused, &changed()).await);
        assert!(a.commit(1, "arbitrary", &changed()).await.is_err());
        assert_eq!(b.receipt(&foreign).await?.unwrap().revision, 0);
        a.commit(1, &latest, &changed()).await?;
        let transaction = store.owner.database.begin_read()?;
        assert_eq!(
            transaction.open_table(REQUESTS)?.len()?,
            2,
            "one current receipt per instance, no expired-ID tombstones"
        );
        a.close().await?;
        b.close().await?;
        assert!(a.new_request_id().await.is_err());
    }
    assert!(
        RedbConfigurationStore::new(&path, [11; 32]).is_err(),
        "expiry cannot silently become indefinite retries"
    );
    assert!(RedbConfigurationStore::new_with_options(&path, [11; 32], options(3)).is_err());
    let store = RedbConfigurationStore::new_with_options(&path, [11; 32], options(2))?;
    let session = store.open("a").await?;
    expired(session.commit(1, &first, &changed()).await);
    assert_eq!(session.receipt(&latest).await?.unwrap().revision, 1);
    assert_eq!(session.load().await?.desired, changed());
    session.close().await?;
    Ok(())
}

#[tokio::test]
async fn opting_into_expiry_preserves_configuration_and_snapshots_but_expires_legacy_receipts(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("configuration.redb");
    {
        let store = RedbConfigurationStore::new(&path, [12; 32])?;
        let session = store.open("instance").await?;
        session.commit(0, "legacy", &changed()).await?;
        session.snapshot("keep").await?;
        session.close().await?;
    }
    let store = RedbConfigurationStore::new_with_options(&path, [12; 32], options(2))?;
    let transaction = store.owner.database.begin_read()?;
    let metadata = transaction.open_table(METADATA)?;
    assert_eq!(
        unseal::<u32>(
            &store.owner,
            &metadata_context("key-check"),
            metadata.get("key-check")?.unwrap().value()
        )?,
        2
    );
    assert_eq!(transaction.open_table(REQUESTS)?.len()?, 0);
    let session = store.open("instance").await?;
    assert!(session.receipt("legacy").await.is_err());
    assert!(session.commit(1, "legacy", &changed()).await.is_err());
    assert_eq!(session.load().await?.desired, changed());
    assert_eq!(
        session.load_snapshot("keep").await?.unwrap().desired,
        changed()
    );
    let id = session.new_request_id().await?;
    assert!(session.receipt(&id).await?.is_none());
    session.commit(1, &id, &changed()).await?;
    session.close().await?;
    let seed = store.open("seed").await?;
    let id = seed.new_request_id().await?;
    seed.close().await?;
    assert!(
        store
            .initialize_if_absent("seed", &id, &DesiredInstance::default())
            .await?
    );
    assert!(
        !store
            .initialize_if_absent("seed", "obsolete", &changed())
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn damaged_request_batch_state_cannot_reinitialize_or_reapply_missing_receipts() -> Result<()>
{
    for damage in ["missing-receipt", "missing-window", "invalid-window"] {
        let directory = tempfile::tempdir()?;
        let store = RedbConfigurationStore::new_with_options(
            directory.path().join("configuration.redb"),
            [13; 32],
            options(2),
        )?;
        let session = store.open("instance").await?;
        let id = session.new_request_id().await?;
        session.commit(0, &id, &changed()).await?;
        session.close().await?;
        let transaction = store.owner.database.begin_write()?;
        match damage {
            "missing-receipt" => {
                transaction
                    .open_table(REQUESTS)?
                    .remove(key("instance", &id)?.as_str())?;
            }
            "missing-window" => {
                transaction
                    .open_table(METADATA)?
                    .remove(window_key("instance").as_str())?;
            }
            _ => {
                let name = window_key("instance");
                let bytes = seal(
                    &store.owner,
                    &metadata_context(&name),
                    &RequestWindow {
                        namespace: Uuid::nil(),
                        batch: 1,
                        accepted: 1,
                    },
                )?;
                transaction
                    .open_table(METADATA)?
                    .insert(name.as_str(), bytes.as_slice())?;
            }
        }
        transaction.commit()?;
        assert!(
            store.open("instance").await.is_err(),
            "{damage} must fail reconstruction"
        );
        assert!(
            store.owner.leases.lock().unwrap().is_empty(),
            "failed reconstruction releases ownership"
        );
    }
    Ok(())
}

#[tokio::test]
async fn snapshot_pages_and_conditional_deletion_leave_current_state_and_other_instances_unchanged(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = RedbConfigurationStore::new(directory.path().join("configuration.redb"), [14; 32])?;
    let session = store.open("a").await?;
    let other = store.open("aa").await?;
    session.commit(0, "first", &changed()).await?;
    let names = ["a", "a\nb", "a\"b", "b", "\u{e9}"];
    for name in names {
        session.snapshot(name).await?;
    }
    other.snapshot("other").await?;
    session
        .commit(1, "second", &DesiredInstance::default())
        .await?;
    let mut after = None;
    let mut found = std::collections::BTreeSet::new();
    loop {
        let page = session
            .list_snapshots(after.as_deref(), NonZeroUsize::new(2).unwrap())
            .await?;
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        for item in &page {
            assert_eq!(item.revision, 1);
            assert!(found.insert(item.name.clone()));
        }
        after = page.last().map(|item| item.name.clone());
    }
    assert_eq!(found, names.into_iter().map(str::to_owned).collect());
    assert!(session.delete_snapshot("a", 0).await.is_err());
    assert!(session.load_snapshot("a").await?.is_some());
    assert!(session.delete_snapshot("a", 1).await?);
    assert!(!session.delete_snapshot("a", 1).await?);
    assert_eq!(session.load().await?.revision, 2);
    assert_eq!(other.load_snapshot("other").await?.unwrap().revision, 0);
    session.close().await?;
    other.close().await?;
    Ok(())
}

async fn seed(store: &RedbConfigurationStore) -> Result<String> {
    let session = store.open("instance").await?;
    let id = session.new_request_id().await?;
    session.commit(0, &id, &changed()).await?;
    session.snapshot("saved").await?;
    assert!(
        store.rotate_key([22; 32]).await.is_err(),
        "live sessions block key rotation"
    );
    session.close().await?;
    Ok(id)
}

#[tokio::test]
async fn key_rotation_preserves_every_record_kind_and_batch_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("configuration.redb");
    let id;
    {
        let store = RedbConfigurationStore::new_with_options(&path, [21; 32], options(2))?;
        id = seed(&store).await?;
        store.rotate_key([22; 32]).await?;
        let session = store.open("instance").await?;
        assert_eq!(session.load().await?.desired, changed());
        assert_eq!(session.receipt(&id).await?.unwrap().revision, 1);
        assert_eq!(
            session.load_snapshot("saved").await?.unwrap().desired,
            changed()
        );
        session.close().await?;
    }
    assert!(RedbConfigurationStore::new_with_options(&path, [21; 32], options(2)).is_err());
    let store = RedbConfigurationStore::new_with_options(&path, [22; 32], options(2))?;
    let session = store.open("instance").await?;
    assert_eq!(session.commit(0, &id, &changed()).await?.revision, 1);
    session.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_key_rotation_keeps_ownership_until_before_and_after_commit_work_finishes(
) -> Result<()> {
    for after_commit in [false, true] {
        let directory = tempfile::tempdir()?;
        let store =
            RedbConfigurationStore::new(directory.path().join("configuration.redb"), [21; 32])?;
        let id = seed(&store).await?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let signal = entered.clone();
        let (resume, wait) = std::sync::mpsc::sync_channel(1);
        let worker = store.clone();
        let rotation = tokio::spawn(async move {
            worker
                .rotate_key_with([22; 32], move |phase| {
                    if matches!(phase, RotationPhase::AfterCommit) == after_commit {
                        signal.notify_one();
                        wait.recv().expect("resume rotation");
                    }
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await?;
        rotation.abort();
        assert!(rotation.await.unwrap_err().is_cancelled());
        assert!(
            store.open("new-instance").await.is_err(),
            "cancelled awaiter cannot release the actual worker"
        );
        assert!(store.rotate_key([23; 32]).await.is_err());
        resume.send(())?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.owner.rotating.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let session = store.open("instance").await?;
        assert_eq!(session.receipt(&id).await?.unwrap().revision, 1);
        assert_eq!(
            session.load_snapshot("saved").await?.unwrap().desired,
            changed()
        );
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn rotation_failure_before_commit_rolls_back_and_postcommit_failure_fences_until_reopen(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("configuration.redb");
    {
        let store = RedbConfigurationStore::new(&path, [21; 32])?;
        seed(&store).await?;
        let record = key("instance", "saved")?;
        let transaction = store.owner.database.begin_write()?;
        let original = {
            let mut snapshots = transaction.open_table(SNAPSHOTS)?;
            let original = snapshots.get(record.as_str())?.unwrap().value().to_vec();
            snapshots.insert(record.as_str(), &[0u8; 41][..])?;
            original
        };
        transaction.commit()?;
        assert!(store.rotate_key([22; 32]).await.is_err());
        let session = store.open("instance").await?;
        assert_eq!(
            session.load().await?.desired,
            changed(),
            "earlier rewritten records rolled back"
        );
        session.close().await?;
        let transaction = store.owner.database.begin_write()?;
        transaction
            .open_table(SNAPSHOTS)?
            .insert(record.as_str(), original.as_slice())?;
        transaction.commit()?;
        assert!(store
            .rotate_key_with([22; 32], |phase| {
                if matches!(phase, RotationPhase::AfterCommit) {
                    panic!("injected postcommit failure");
                }
            })
            .await
            .is_err());
        assert!(
            store.open("instance").await.is_err(),
            "an unconfirmed in-memory key cannot encrypt new records"
        );
        assert!(store.rotate_key([23; 32]).await.is_err());
    }
    assert!(RedbConfigurationStore::new(&path, [21; 32]).is_err());
    let store = RedbConfigurationStore::new(&path, [22; 32])?;
    let session = store.open("instance").await?;
    assert_eq!(session.load().await?.desired, changed());
    assert_eq!(
        session.load_snapshot("saved").await?.unwrap().desired,
        changed()
    );
    session.close().await?;
    Ok(())
}
