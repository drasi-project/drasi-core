// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::{
    computation::v1::{
        ComponentId, ComponentName, ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan,
    },
    management::{ComponentUpdateDefinition, OrdinaryComponent, OrdinaryComponentDefinition},
};

fn update() -> Result<(DesiredInstance, DesiredInstance, AcceptedComponentUpdate)> {
    let id = ComponentId::generate();
    let before = OrdinaryComponentDefinition {
        id: None,
        name: ComponentName::try_new("source")?,
        auto_start: false,
        component: OrdinaryComponent::Source {
            kind: "fixture".into(),
            configuration: serde_json::json!({"interval": 1}),
        },
    }
    .with_assigned_id(id.clone());
    let mut after = before.clone();
    after.component = OrdinaryComponent::Source {
        kind: "fixture".into(),
        configuration: serde_json::json!({"interval": 2}),
    };
    Ok((
        DesiredInstance {
            ordinary: vec![before.clone()],
            ..Default::default()
        },
        DesiredInstance {
            ordinary: vec![after.clone()],
            ..Default::default()
        },
        AcceptedComponentUpdate {
            component_id: id,
            operation_id: "accepted-update".into(),
            plan: ComponentUpdatePlan {
                mode: ComponentUpdateMode::Restart,
                data: ComponentUpdateData::Preserve,
            },
            current: ComponentUpdateDefinition::Ordinary(before),
            proposed: ComponentUpdateDefinition::Ordinary(after),
        },
    ))
}

#[tokio::test]
async fn accepted_updates_and_completion_survive_reopen_without_changing_receipts_or_snapshots(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("updates.redb");
    let (before, desired, update) = update()?;
    let receipt = {
        let store = RedbConfigurationStore::new(&path, [23; 32])?;
        let session = store.open("instance").await?;
        let initial = session.commit(0, "initial", &before, None, &[]).await?;
        let receipt = session
            .commit(
                initial.revision,
                "update",
                &desired,
                None,
                &[update.clone()],
            )
            .await?;
        assert_eq!(receipt.updates, vec![update.clone()]);
        assert_eq!(
            session.snapshot("pending").await?.pending_updates,
            vec![update.clone()]
        );
        session.close().await?;
        receipt
    };
    {
        let store = RedbConfigurationStore::new(&path, [23; 32])?;
        let session = store.open("instance").await?;
        assert_eq!(session.load().await?.pending_updates, vec![update.clone()]);
        let mut conflicting = update.clone();
        conflicting.operation_id = "different-operation".into();
        assert!(session
            .commit(1, "update", &desired, None, &[conflicting.clone()])
            .await
            .is_err());
        assert!(session
            .commit(
                receipt.revision,
                "second-update",
                &desired,
                None,
                &[conflicting]
            )
            .await
            .is_err());
        let completion = ComponentUpdateCompletion {
            component_id: update.component_id.clone(),
            operation_id: update.operation_id.clone(),
        };
        let wrong = ComponentUpdateCompletion {
            operation_id: "wrong-operation".into(),
            ..completion.clone()
        };
        assert!(session
            .complete_updates(receipt.revision, &[completion.clone(), wrong])
            .await
            .is_err());
        assert_eq!(session.load().await?.pending_updates, vec![update.clone()]);
        assert!(session
            .complete_updates(0, &[completion.clone()])
            .await
            .is_err());
        let completed = session
            .complete_updates(receipt.revision, &[completion.clone()])
            .await?;
        assert_eq!(completed.desired, desired);
        assert_eq!(completed.revision, receipt.revision);
        assert!(completed.pending_updates.is_empty());
        assert_eq!(
            session
                .complete_updates(receipt.revision, &[completion])
                .await?,
            completed
        );
        assert_eq!(session.receipt("update").await?, Some(receipt.clone()));
        assert_eq!(
            session
                .load_snapshot("pending")
                .await?
                .unwrap()
                .pending_updates,
            vec![update.clone()]
        );
        session.close().await?;
    }
    let store = RedbConfigurationStore::new(&path, [23; 32])?;
    let session = store.open("instance").await?;
    assert!(session.load().await?.pending_updates.is_empty());
    assert_eq!(
        session
            .commit(1, "update", &desired, None, &[update])
            .await?,
        receipt
    );
    assert!(session.load().await?.pending_updates.is_empty());
    session.close().await?;
    Ok(())
}

#[tokio::test]
async fn expiring_receipts_does_not_expire_an_unfinished_update() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = RedbConfigurationStore::new_with_options(
        directory.path().join("expiring.redb"),
        [24; 32],
        ConfigurationStoreOptions {
            receipt_batch_capacity: std::num::NonZeroUsize::new(1),
        },
    )?;
    let session = store.open("instance").await?;
    let (before, desired, update) = update()?;
    let initial = session.new_request_id().await?;
    let initial = session.commit(0, &initial, &before, None, &[]).await?;
    let request = session.new_request_id().await?;
    let receipt = session
        .commit(
            initial.revision,
            &request,
            &desired,
            None,
            &[update.clone()],
        )
        .await?;
    let _next = session.new_request_id().await?;
    assert!(matches!(
        session
            .receipt(&request)
            .await
            .unwrap_err()
            .downcast_ref::<ManagementError>(),
        Some(ManagementError::RequestExpired)
    ));
    assert_eq!(session.load().await?.pending_updates, vec![update.clone()]);
    let completed = session
        .complete_updates(
            receipt.revision,
            &[ComponentUpdateCompletion {
                component_id: update.component_id,
                operation_id: update.operation_id,
            }],
        )
        .await?;
    assert!(completed.pending_updates.is_empty());
    session.close().await?;
    Ok(())
}
