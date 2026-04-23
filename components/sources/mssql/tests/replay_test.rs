// Copyright 2025 The Drasi Authors.
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

//! Replay integration tests for MSSQL source.
//!
//! These tests verify:
//! - CDC events carry sequence positions
//! - Resume from a position filters already-seen events
//! - Subscribe rejects expired positions with PositionUnavailable
//!
//! Run with:
//!   cargo test -p drasi-source-mssql --test replay_test -- --ignored --nocapture

mod mssql_helpers;

use anyhow::{Context, Result};
use drasi_lib::channels::SourceEventWrapper;
use drasi_lib::component_graph::ComponentGraph;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::sources::{Source, SourceError};
use drasi_source_mssql::lsn::Lsn;
use drasi_source_mssql::stream::{cdc_position, position_to_seek_lsn};
use drasi_source_mssql::{MsSqlSource, StartPosition};
use mssql_helpers::{execute_sql, setup_mssql, MssqlConfig};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};

const TEST_DB: &str = "DrasiReplayTest";
const TEST_TABLE: &str = "dbo.Products";
const SOURCE_ID: &str = "replay-source";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_source(config: &MssqlConfig, start_pos: StartPosition) -> Result<MsSqlSource> {
    MsSqlSource::builder(SOURCE_ID)
        .with_host(&config.host)
        .with_port(config.port)
        .with_database(&config.database)
        .with_user(&config.user)
        .with_password(&config.password)
        .with_table(TEST_TABLE)
        .with_poll_interval_ms(500)
        .with_start_position(start_pos)
        .with_trust_server_certificate(true)
        .build()
        .context("Failed to build MsSqlSource")
}

fn make_context() -> SourceRuntimeContext {
    let (graph, _rx) = ComponentGraph::new("replay-test");
    SourceRuntimeContext::new("replay-test", SOURCE_ID, None, graph.update_sender(), None)
}

fn make_settings(
    resume_from: Option<drasi_lib::position::SequencePosition>,
) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: SOURCE_ID.to_string(),
        enable_bootstrap: false,
        query_id: "replay-query".to_string(),
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from,
        request_position_handle: false,
    }
}

/// Receive events from the source with a per-event timeout.
/// Returns all events collected before the timeout fires on a recv.
async fn collect_events(
    receiver: &mut Box<dyn drasi_lib::channels::ChangeReceiver<SourceEventWrapper>>,
    per_event_timeout: Duration,
    max_events: usize,
) -> Vec<Arc<SourceEventWrapper>> {
    let mut events = Vec::new();
    for _ in 0..max_events {
        match timeout(per_event_timeout, receiver.recv()).await {
            Ok(Ok(event)) => events.push(event),
            _ => break,
        }
    }
    events
}

async fn prepare_database(config: &MssqlConfig) -> Result<MssqlConfig> {
    let mut client = config.connect().await?;
    execute_sql(
        &mut client,
        &format!("IF DB_ID('{TEST_DB}') IS NULL CREATE DATABASE [{TEST_DB}]"),
    )
    .await?;
    drop(client);

    let mut db_config = config.clone();
    db_config.database = TEST_DB.to_string();
    let mut db_client = db_config.connect().await?;

    execute_sql(
        &mut db_client,
        "IF NOT EXISTS (SELECT 1 FROM sys.databases WHERE name = DB_NAME() AND is_cdc_enabled = 1) \
         EXEC sys.sp_cdc_enable_db;",
    )
    .await?;

    execute_sql(
        &mut db_client,
        "IF OBJECT_ID('dbo.Products', 'U') IS NOT NULL BEGIN \
             IF EXISTS (SELECT 1 FROM sys.tables WHERE name = 'Products' AND is_tracked_by_cdc = 1) \
                 EXEC sys.sp_cdc_disable_table @source_schema = 'dbo', @source_name = 'Products', @capture_instance = 'all'; \
             DROP TABLE dbo.Products; \
         END",
    )
    .await?;

    execute_sql(
        &mut db_client,
        "CREATE TABLE dbo.Products (
            ProductId INT PRIMARY KEY,
            Name NVARCHAR(100) NOT NULL,
            Price DECIMAL(10,2) NOT NULL
        );",
    )
    .await?;

    execute_sql(
        &mut db_client,
        "IF NOT EXISTS (SELECT 1 FROM sys.tables WHERE name = 'Products' AND is_tracked_by_cdc = 1) \
             EXEC sys.sp_cdc_enable_table @source_schema = 'dbo', @source_name = 'Products', @role_name = NULL;",
    )
    .await?;

    Ok(db_config)
}

/// Wait until the CDC change table contains at least `expected` rows.
async fn wait_for_cdc_rows(config: &MssqlConfig, expected: usize) -> Result<()> {
    for _ in 0..30 {
        let mut client = config.connect().await?;
        let stream = client
            .query(
                "SELECT COUNT(*) FROM cdc.dbo_Products_CT WHERE __$operation = 2",
                &[],
            )
            .await?;
        let rows = stream.into_first_result().await?;
        if let Some(row) = rows.first() {
            let count: i32 = row.get(0).unwrap_or(0);
            if count as usize >= expected {
                return Ok(());
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("Timed out waiting for {expected} CDC rows");
}

/// Query CDC change table to get (start_lsn, seqval) pairs for inserts,
/// ordered by position. Returns one entry per inserted ProductId.
async fn get_cdc_positions(config: &MssqlConfig) -> Result<Vec<(i32, Lsn, Lsn)>> {
    let mut client = config.connect().await?;
    let stream = client
        .query(
            "SELECT ProductId, __$start_lsn, __$seqval \
             FROM cdc.dbo_Products_CT \
             WHERE __$operation = 2 \
             ORDER BY __$start_lsn, __$seqval",
            &[],
        )
        .await?;
    let rows = stream.into_first_result().await?;

    let mut result = Vec::new();
    for row in &rows {
        let product_id: i32 = row
            .get(0)
            .ok_or_else(|| anyhow::anyhow!("NULL ProductId"))?;
        let lsn_bytes: &[u8] = row
            .try_get(1)?
            .ok_or_else(|| anyhow::anyhow!("NULL __$start_lsn"))?;
        let seqval_bytes: &[u8] = row
            .try_get(2)?
            .ok_or_else(|| anyhow::anyhow!("NULL __$seqval"))?;
        let start_lsn = Lsn::from_bytes(lsn_bytes)?;
        let seqval = Lsn::from_bytes(seqval_bytes)?;
        result.push((product_id, start_lsn, seqval));
    }

    Ok(result)
}

/// Extract the product IDs from source events (CDC insert/update/delete).
fn extract_product_ids(events: &[Arc<SourceEventWrapper>]) -> Vec<String> {
    use drasi_lib::channels::SourceEvent;

    events
        .iter()
        .filter_map(|e| match &e.event {
            SourceEvent::Change(change) => {
                let elem = match change {
                    drasi_core::models::SourceChange::Insert { element } => Some(element),
                    drasi_core::models::SourceChange::Update { element } => Some(element),
                    drasi_core::models::SourceChange::Delete { metadata } => {
                        return Some(metadata.reference.element_id.to_string());
                    }
                    _ => None,
                };
                elem.map(|e| match e {
                    drasi_core::models::Element::Node { metadata, .. } => {
                        metadata.reference.element_id.to_string()
                    }
                    drasi_core::models::Element::Relation { metadata, .. } => {
                        metadata.reference.element_id.to_string()
                    }
                })
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify that CDC events emitted by the MSSQL source carry non-None
/// sequence positions that are monotonically increasing.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_replay_events_carry_sequence_positions() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare database")?;

        // Start source with Beginning so we pick up everything
        let source = make_source(&db_config, StartPosition::Beginning)?;
        let ctx = make_context();
        source.initialize(ctx).await;

        let response = source.subscribe(make_settings(None)).await?;
        let mut receiver = response.receiver;

        source.start().await?;

        // Give the source time to initialize its CDC polling loop
        sleep(Duration::from_secs(3)).await;

        // Insert two rows (separate transactions for distinct positions)
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (1, 'Widget', 10.00);",
        )
        .await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (2, 'Gadget', 20.00);",
        )
        .await?;

        // Collect events
        let events = collect_events(&mut receiver, Duration::from_secs(15), 10).await;

        // We should have at least 2 CDC change events
        assert!(
            events.len() >= 2,
            "Expected at least 2 events, got {}",
            events.len()
        );

        // Every event should have a sequence position
        for (i, event) in events.iter().enumerate() {
            assert!(
                event.sequence.is_some(),
                "Event {i} has no sequence position"
            );
        }

        // Positions should be strictly increasing
        let positions: Vec<_> = events.iter().filter_map(|e| e.sequence).collect();
        for window in positions.windows(2) {
            assert!(
                window[0] < window[1],
                "Positions not strictly increasing: {:?} >= {:?}",
                window[0],
                window[1]
            );
        }

        source.stop().await?;
        mssql.cleanup().await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Test timed out after 300 seconds"),
    }

    Ok(())
}

/// Verify that resuming from a CDC position filters out already-seen events.
///
/// Inserts 3 rows in a single transaction (same start_lsn, different seqval),
/// resumes from row 2's position, and verifies only row 3 arrives.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_replay_resume_filters_already_seen() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare database")?;

        // Step 1: Insert 3 rows in a single transaction so they share start_lsn
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "BEGIN TRANSACTION; \
             INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (1, 'Alpha', 10.00); \
             INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (2, 'Beta', 20.00); \
             INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (3, 'Gamma', 30.00); \
             COMMIT;",
        )
        .await?;

        // Step 2: Wait for CDC to capture all 3 inserts
        wait_for_cdc_rows(&db_config, 3)
            .await
            .context("CDC did not capture 3 rows in time")?;

        // Step 3: Get the CDC positions for each row
        let positions = get_cdc_positions(&db_config).await?;
        assert!(
            positions.len() >= 3,
            "Expected 3 CDC positions, got {}",
            positions.len()
        );

        // Find positions for rows 1, 2, 3
        let row2_entry = positions
            .iter()
            .find(|(pid, _, _)| *pid == 2)
            .ok_or_else(|| anyhow::anyhow!("No CDC position for ProductId=2"))?;
        let resume_pos = cdc_position(&row2_entry.1, &row2_entry.2);

        log::info!(
            "Resuming from position of row 2: seek_lsn={}, position={:?}",
            row2_entry.1.to_hex(),
            resume_pos
        );

        // Step 4: Create source with resume_from = row 2's position
        let source = make_source(&db_config, StartPosition::Beginning)?;
        let ctx = make_context();
        source.initialize(ctx).await;

        let response = source
            .subscribe(make_settings(Some(resume_pos)))
            .await
            .context("subscribe with resume_from failed")?;
        let mut receiver = response.receiver;

        source.start().await.context("Failed to start source")?;

        // Step 5: Collect events — we should see row 3 but NOT rows 1 or 2
        let events = collect_events(&mut receiver, Duration::from_secs(15), 20).await;
        let ids = extract_product_ids(&events);

        log::info!("Received product IDs after resume: {ids:?}");

        // Element IDs are formatted as "dbo.Products:N"
        assert!(
            !ids.iter().any(|id| id == "dbo.Products:1"),
            "Row 1 should have been filtered out, but was received. IDs: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "dbo.Products:2"),
            "Row 2 should have been filtered out (at skip_threshold), but was received. IDs: {ids:?}"
        );

        // Row 3 should arrive
        assert!(
            ids.iter().any(|id| id == "dbo.Products:3"),
            "Row 3 should have arrived after resume. IDs: {ids:?}"
        );

        // Step 6: Insert row 4 and verify it arrives
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (4, 'Delta', 40.00);",
        )
        .await?;

        let new_events = collect_events(&mut receiver, Duration::from_secs(15), 10).await;
        let new_ids = extract_product_ids(&new_events);

        log::info!("Received product IDs for new insert: {new_ids:?}");
        assert!(
            new_ids.iter().any(|id| id == "dbo.Products:4"),
            "Row 4 should have been received. IDs: {new_ids:?}"
        );

        source.stop().await?;
        mssql.cleanup().await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Test timed out after 300 seconds"),
    }

    Ok(())
}

/// Verify that subscribe() returns PositionUnavailable when the requested
/// position is before the CDC retention window.
#[tokio::test]
#[ignore]
#[cfg(not(target_arch = "aarch64"))]
async fn test_mssql_replay_position_unavailable() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let result = timeout(Duration::from_secs(300), async {
        let mssql = setup_mssql()
            .await
            .context("Failed to start MSSQL container")?;
        let db_config = prepare_database(mssql.config())
            .await
            .context("Failed to prepare database")?;

        // Insert data so CDC has real LSNs (min_lsn > 0)
        let mut client = db_config.connect().await?;
        execute_sql(
            &mut client,
            "INSERT INTO dbo.Products (ProductId, Name, Price) VALUES (1, 'Test', 1.00);",
        )
        .await?;
        wait_for_cdc_rows(&db_config, 1)
            .await
            .context("CDC did not capture rows")?;

        // Verify min_lsn is non-zero
        let min_lsn_row = {
            let mut c = db_config.connect().await?;
            let stream = c
                .query(
                    "SELECT sys.fn_cdc_get_min_lsn('dbo_Products') AS min_lsn",
                    &[],
                )
                .await?;
            let rows = stream.into_first_result().await?;
            let lsn_bytes: &[u8] = rows[0]
                .try_get(0)?
                .ok_or_else(|| anyhow::anyhow!("min_lsn is NULL"))?;
            Lsn::from_bytes(lsn_bytes)?
        };
        log::info!("min_lsn = {}", min_lsn_row.to_hex());
        assert!(
            min_lsn_row > Lsn::zero(),
            "Expected non-zero min_lsn after inserting data"
        );

        // Try to subscribe with a position that's before min_lsn
        let expired_pos = cdc_position(&Lsn::zero(), &Lsn::zero());

        let source = make_source(&db_config, StartPosition::Beginning)?;
        let ctx = make_context();
        source.initialize(ctx).await;

        let subscribe_result = source.subscribe(make_settings(Some(expired_pos))).await;

        let err = match subscribe_result {
            Err(e) => e,
            Ok(_) => anyhow::bail!("subscribe should have failed with expired position"),
        };

        // Downcast to SourceError::PositionUnavailable
        let source_err = err
            .downcast_ref::<SourceError>()
            .expect("Error should be SourceError");

        match source_err {
            SourceError::PositionUnavailable {
                source_id,
                earliest_available,
                ..
            } => {
                assert_eq!(source_id, SOURCE_ID);
                assert!(
                    earliest_available.is_some(),
                    "earliest_available should be provided"
                );
                // Verify the earliest_available position extracts to a valid LSN
                let earliest = earliest_available.unwrap();
                let earliest_lsn = position_to_seek_lsn(&earliest)?;
                assert!(
                    earliest_lsn >= min_lsn_row,
                    "earliest_available LSN should be >= min_lsn"
                );
            }
        }

        log::info!("Got expected PositionUnavailable error");

        mssql.cleanup().await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match result {
        Ok(inner) => inner?,
        Err(_) => anyhow::bail!("Test timed out after 300 seconds"),
    }

    Ok(())
}
