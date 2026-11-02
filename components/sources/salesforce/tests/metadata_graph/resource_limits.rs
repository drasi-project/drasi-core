// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

async fn runtime(
    fixture: &Fixture,
    root: &Path,
    max_rows: usize,
    max_snapshot_leases: usize,
) -> Result<DrasiLib> {
    let mut config = fixture.source_config(root, 3600)?;
    config.max_rows = max_rows;
    config.max_snapshot_leases = max_snapshot_leases;
    Ok(DrasiLib::builder()
        .with_id("salesforce-resource-limits")
        .with_source(
            SalesforceSource::builder(SOURCE_ID)
                .with_config(config)
                .build()?,
        )
        .with_query(
            Query::cypher(ACCOUNTS)
                .query(ACCOUNTS_QUERY)
                .from_source(SOURCE_ID)
                .enable_bootstrap(true)
                .build(),
        )
        .build()
        .await?)
}

fn account_id(number: usize) -> String {
    format!("001{number:012}AAA")
}

async fn offline_replacement(max_rows: usize) -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    let mut expected = Vec::new();
    for number in 1..=256 {
        let id = account_id(number);
        fixture.rest.upsert(&id, "Existing").await;
        expected.push(json!({"id": id, "name": "Existing"}));
    }

    let drasi = runtime(&fixture, temp.path(), max_rows, 4).await?;
    let initial = async {
        drasi.start().await?;
        wait_rows(&drasi, ACCOUNTS, expected.clone()).await
    }
    .await;
    drasi.shutdown().await?;
    drop(drasi);
    initial?;

    fixture.rest.remove(&account_id(1)).await;
    fixture.rest.upsert(&account_id(257), "Replacement").await;
    expected.remove(0);
    expected.push(json!({"id": account_id(257), "name": "Replacement"}));
    assert_eq!(fixture.rest.rows.read().await["Account"].len(), 256);

    let drasi = runtime(&fixture, temp.path(), max_rows, 4).await?;
    let resumed = async {
        drasi.start().await?;
        wait_rows(&drasi, ACCOUNTS, expected).await
    }
    .await;
    drasi.shutdown().await?;
    drop(drasi);
    fixture.close().await;
    resumed
}

async fn volatile_restart(max_snapshot_leases: usize) -> Result<()> {
    let temp = TempDir::new()?;
    let fixture = Fixture::new().await?;
    fixture.rest.upsert(ID_1, "Existing").await;
    let drasi = runtime(&fixture, temp.path(), 256, max_snapshot_leases).await?;
    let result = async {
        drasi.start().await?;
        let expected = vec![json!({"id": ID_1, "name": "Existing"})];
        wait_rows(&drasi, ACCOUNTS, expected.clone()).await?;
        drasi.stop_query(ACCOUNTS).await?;
        tokio::time::timeout(TIMEOUT, async {
            while drasi.get_query_status(ACCOUNTS).await? != ComponentStatus::Stopped {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        drasi.start_query(ACCOUNTS).await?;
        wait_rows(&drasi, ACCOUNTS, expected).await
    }
    .await;
    drasi.shutdown().await?;
    drop(drasi);
    fixture.close().await;
    result
}

#[tokio::test]
async fn offline_replacement_at_row_limit() -> Result<()> {
    offline_replacement(256).await
}

#[tokio::test]
async fn offline_replacement_with_one_spare_row() -> Result<()> {
    offline_replacement(257).await
}

#[tokio::test]
async fn volatile_restart_with_one_snapshot_lease() -> Result<()> {
    volatile_restart(1).await
}

#[tokio::test]
async fn volatile_restart_with_two_snapshot_leases() -> Result<()> {
    volatile_restart(2).await
}
