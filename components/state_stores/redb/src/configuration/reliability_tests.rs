// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{future::Future, pin::Pin, task::Poll};

async fn pending<T>(mut future: Pin<&mut impl Future<Output = T>>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await
}

fn changed() -> DesiredInstance {
    let mut desired = DesiredInstance::default();
    desired.topology.allow_incomplete = !desired.topology.allow_incomplete;
    desired
}

#[tokio::test]
async fn cancelled_storage_call_and_close_retain_lease_until_actual_io_finishes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = RedbConfigurationStore::new(directory.path().join("leases.redb"), [41; 32])?;
    let old = store.open("instance").await?;
    let desired = changed();
    let blocker = store.owner.database.begin_write()?;
    let mut commit = Box::pin(old.commit(0, "accepted", &desired));
    pending(commit.as_mut()).await;
    drop(commit);
    let mut close = Box::pin(old.close());
    pending(close.as_mut()).await;
    drop(close);
    assert!(matches!(
        store
            .open("instance")
            .await
            .err()
            .expect("owned instance")
            .downcast_ref(),
        Some(ManagementError::AlreadyOwned(_))
    ));
    drop(blocker);
    old.close().await?;
    let replacement = store.open("instance").await?;
    assert_eq!(replacement.load().await?.desired, desired);
    assert_eq!(
        replacement
            .receipt("accepted")
            .await?
            .expect("accepted receipt")
            .revision,
        1
    );
    old.close().await?;
    assert!(matches!(
        old.commit(1, "stale", &DesiredInstance::default())
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(ManagementError::Closed)
    ));
    drop(old);
    assert!(matches!(
        store
            .open("instance")
            .await
            .err()
            .expect("owned instance")
            .downcast_ref(),
        Some(ManagementError::AlreadyOwned(_))
    ));
    replacement
        .commit(1, "new-owner", &DesiredInstance::default())
        .await?;
    replacement.close().await?;
    Ok(())
}

#[tokio::test]
async fn snapshots_and_commits_are_serialized_even_when_the_first_caller_cancels() -> Result<()> {
    for snapshot_first in [true, false] {
        let directory = tempfile::tempdir()?;
        let store = RedbConfigurationStore::new(directory.path().join("snapshots.redb"), [42; 32])?;
        let session = store.open("instance").await?;
        let before = changed();
        let after = DesiredInstance::default();
        session.commit(0, "before", &before).await?;
        let blocker = store.owner.database.begin_write()?;
        let mut snapshot = Box::pin(session.snapshot("cut"));
        let mut commit = Box::pin(session.commit(1, "after", &after));
        if snapshot_first {
            pending(snapshot.as_mut()).await;
            pending(commit.as_mut()).await;
            drop(snapshot);
            drop(blocker);
            assert_eq!(commit.await?.revision, 2);
        } else {
            pending(commit.as_mut()).await;
            pending(snapshot.as_mut()).await;
            drop(commit);
            drop(blocker);
            assert_eq!(snapshot.await?.revision, 2);
        }
        let cut = session.load_snapshot("cut").await?.expect("saved snapshot");
        assert_eq!(cut.revision, if snapshot_first { 1 } else { 2 });
        assert_eq!(
            cut.desired,
            if snapshot_first {
                before
            } else {
                after.clone()
            }
        );
        assert_eq!(session.load().await?.desired, after);
        assert_eq!(
            session
                .receipt("after")
                .await?
                .expect("committed receipt")
                .revision,
            2
        );
        session.close().await?;
        drop((session, store));
        let reopened =
            RedbConfigurationStore::new(directory.path().join("snapshots.redb"), [42; 32])?;
        let session = reopened.open("instance").await?;
        assert_eq!(session.load_snapshot("cut").await?, Some(cut));
        assert_eq!(session.load().await?.revision, 2);
        session.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn damaged_configuration_records_fail_explicitly_without_releasing_other_instances(
) -> Result<()> {
    for kind in ["current", "request", "snapshot"] {
        for damage in ["empty", "header", "ciphertext", "context", "json"] {
            let directory = tempfile::tempdir()?;
            let store =
                RedbConfigurationStore::new(directory.path().join("records.redb"), [43; 32])?;
            let session = store.open("damaged").await?;
            session.commit(0, "accepted", &changed()).await?;
            session.snapshot("saved").await?;
            let healthy = store.open("healthy").await?;
            healthy.commit(0, "accepted", &changed()).await?;
            let (table, record, context) = match kind {
                "current" => (CURRENT, "damaged".to_owned(), "current:damaged".to_owned()),
                "request" => {
                    let record = key("damaged", "accepted")?;
                    (REQUESTS, record.clone(), format!("request:{record}"))
                }
                "snapshot" => {
                    let record = key("damaged", "saved")?;
                    (SNAPSHOTS, record.clone(), format!("snapshot:{record}"))
                }
                _ => unreachable!(),
            };
            let transaction = store.owner.database.begin_write()?;
            {
                let mut table = transaction.open_table(table)?;
                let original = table
                    .get(record.as_str())?
                    .expect("saved record")
                    .value()
                    .to_vec();
                let mut corrupt = original;
                match damage {
                    "empty" => corrupt.clear(),
                    "header" => corrupt[0] = 2,
                    "ciphertext" => *corrupt.last_mut().expect("ciphertext byte") ^= 1,
                    "context" => corrupt = seal(&store.owner, "wrong-context", &changed())?,
                    "json" => {
                        corrupt = seal_bytes(
                            &store.owner.cipher.read().expect("cipher lock"),
                            &context,
                            b"{invalid-json",
                        )?
                    }
                    _ => unreachable!(),
                }
                table.insert(record.as_str(), corrupt.as_slice())?;
            }
            transaction.commit()?;
            let error = match kind {
                "current" => session.load().await.err(),
                "request" => session.receipt("accepted").await.err(),
                "snapshot" => session.load_snapshot("saved").await.err(),
                _ => unreachable!(),
            };
            assert!(
                error.is_some(),
                "{kind}/{damage} must not become empty state"
            );
            session.close().await?;
            if kind == "current" {
                let result = drasi_lib::DrasiLib::builder()
                    .with_id("damaged")
                    .with_configuration_store(Arc::new(store.clone()))
                    .build()
                    .await;
                assert!(result.is_err(), "corruption must reject public restoration");
                let retry = store.open("damaged").await?;
                assert!(retry.load().await.is_err());
                retry.close().await?;
            }
            assert_eq!(healthy.load().await?.desired, changed());
            assert_eq!(
                healthy
                    .receipt("accepted")
                    .await?
                    .expect("healthy receipt")
                    .revision,
                1
            );
            healthy.close().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_persisted_configuration_version_rejects_restore_without_overwriting_intent(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("version.redb"),
        [44; 32],
    )?);
    let session = store.open("instance").await?;
    let mut future = changed();
    future.version = 2;
    session.commit(0, "future-version", &future).await?;
    session.close().await?;
    assert!(drasi_lib::DrasiLib::builder()
        .with_id("instance")
        .with_configuration_store(store.clone())
        .build()
        .await
        .is_err());
    let retained = store.open("instance").await?;
    assert_eq!(retained.load().await?.desired, future);
    assert_eq!(
        retained
            .receipt("future-version")
            .await?
            .expect("preserved receipt")
            .revision,
        1
    );
    retained.close().await?;
    Ok(())
}
